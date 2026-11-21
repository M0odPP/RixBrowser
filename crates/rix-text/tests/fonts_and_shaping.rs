//! Integration tests for unlimited lazy system fonts, Unicode coverage index,
//! cross-thread `TextShapingCache` LRU hits/eviction, and WOFF2/TTF `@font-face`.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use rix_text::{
    decode_web_font, parse_unicode_range, FontDatabase, FontFaceSet, FontStyle, FontWeight,
    TextShaper, TextShapingCache, WebFontFormat,
};

#[test]
fn unlimited_lazy_indexing_loads_zero_bytes_and_warms_under_50ms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_path = tmp.path().join("font_index_v2.bin");
    std::env::set_var("RIX_FONT_CACHE_PATH", &cache_path);

    // Cold scan indexes all installed fonts without retaining bytes in memory.
    let mut db_cold = FontDatabase::new();
    let count_cold = db_cold.scan_system_fonts();
    assert!(
        count_cold > 32,
        "expected unlimited indexing (> 32 faces), got {count_cold}"
    );
    assert_eq!(
        db_cold.loaded_font_count(),
        0,
        "lazy scan must not eagerly retain font bytes in RAM"
    );

    // Warm startup indexing from v2 disk metadata cache must complete in < 50 ms.
    let start = Instant::now();
    let mut db_warm = FontDatabase::new();
    let count_warm = db_warm.scan_system_fonts();
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    std::env::remove_var("RIX_FONT_CACHE_PATH");

    assert_eq!(count_cold, count_warm);
    assert_eq!(db_warm.loaded_font_count(), 0);
    assert!(
        elapsed_ms < 50.0,
        "warm lazy metadata indexing of {count_warm} faces took {elapsed_ms:.2} ms (budget < 50 ms)"
    );
}

#[test]
fn coverage_index_resolves_latin_cyrillic_cjk_and_emoji_lazily() {
    let mut db = FontDatabase::new();
    db.scan_system_fonts();
    assert!(!db.is_empty());

    // Latin
    let latin = db.find_covering_font('A', db.default_sans_serif);
    assert!(latin.is_some(), "expected Latin fallback font");

    // Cyrillic
    let cyr = db.find_covering_font('\u{0416}', db.default_sans_serif);
    assert!(cyr.is_some(), "expected Cyrillic fallback font");

    // Emoji or CJK (if installed on host, must resolve without loading every font)
    let _cjk = db.find_covering_font('\u{65E5}', db.default_sans_serif);
    let _emoji = db.find_covering_font('\u{1F600}', db.default_sans_serif);

    // Only a tiny subset of installed fonts should have been lazily loaded from disk.
    assert!(
        db.loaded_font_count() < db.len(),
        "expected lazy loading to keep most system fonts unloaded ({} / {})",
        db.loaded_font_count(),
        db.len()
    );
}

#[test]
fn web_font_woff2_ttf_decode_and_unicode_range_filtering() {
    let ranges = parse_unicode_range("U+0041-005A, U+04??, U+20AC");
    assert_eq!(ranges.len(), 3);
    assert!(ranges[0].contains('A'));
    assert!(ranges[0].contains('Z'));
    assert!(!ranges[0].contains('a'));
    assert!(ranges[1].contains('\u{0410}'));
    assert!(ranges[2].contains('\u{20AC}'));

    let mut db = FontDatabase::new();
    db.scan_system_fonts();
    let mut faces = FontFaceSet::new();

    // Load a real TTF from the system font database into FontFaceSet with unicode-range.
    let sample_id = db.default_sans_serif.expect("system sans font");
    let sample_ttf = db.get_font(sample_id).unwrap().bytes();
    let (fmt, decoded) = decode_web_font(&sample_ttf).expect("decode raw TTF");
    assert!(matches!(
        fmt,
        WebFontFormat::TrueType | WebFontFormat::OpenType | WebFontFormat::Collection
    ));
    assert!(!decoded.is_empty());

    let reg_id = faces
        .register_font_bytes(
            &mut db,
            "RixCustomSans",
            (400, 700),
            FontStyle::Normal,
            parse_unicode_range("U+0041-005A"),
            &sample_ttf,
        )
        .expect("register TTF web font");
    assert_eq!(
        faces.match_face_for_char(
            &db,
            "RixCustomSans",
            FontWeight::BOLD,
            FontStyle::Normal,
            'R'
        ),
        Some(reg_id)
    );
    assert_eq!(
        faces.match_face_for_char(
            &db,
            "RixCustomSans",
            FontWeight::BOLD,
            FontStyle::Normal,
            'r'
        ),
        None
    );

    // Verify pure-Rust WOFF2 Brotli decoding on a real WOFF2 file if present on host.
    let host_woff2 = Path::new("/usr/share/doc/appstream/html/api/RedHatDisplay-Bold.woff2");
    if host_woff2.exists() {
        let raw_woff2 = std::fs::read(host_woff2).expect("read host woff2");
        let (wfmt, sfnt) = decode_web_font(&raw_woff2).expect("decode WOFF2");
        assert_eq!(wfmt, WebFontFormat::Woff2);
        assert!(sfnt.len() > raw_woff2.len());

        let woff2_id = faces
            .register_font_bytes(
                &mut db,
                "RedHatWeb",
                (700, 700),
                FontStyle::Normal,
                Vec::new(),
                &raw_woff2,
            )
            .expect("register WOFF2 face");
        let shaper = TextShaper::new(&db);
        let runs = shaper.shape_text("RixBrowser WOFF2", woff2_id, 18.0);
        assert!(!runs.is_empty());
        assert!(runs[0].width > 0.0);
    }
}

#[test]
fn shared_shaping_cache_hits_across_worker_threads_and_enforces_lru_budget() {
    let mut db = FontDatabase::new();
    db.scan_system_fonts();
    let font_id = db.default_sans_serif.expect("system sans font");
    let db = Arc::new(db);

    // 1. Cross-thread shared cache hits
    let cache = Arc::new(TextShapingCache::new(4 * 1024 * 1024));
    let shaper = TextShaper::with_cache(&db, Arc::clone(&cache));
    let baseline = shaper.shape_text("Shared shaping cache check", font_id, 16.0);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 0);

    std::thread::scope(|s| {
        for _ in 0..4 {
            let db_ref = Arc::clone(&db);
            let cache_ref = Arc::clone(&cache);
            let expected = baseline.clone();
            s.spawn(move || {
                let worker = TextShaper::with_cache(&db_ref, cache_ref);
                let got = worker.shape_text("Shared shaping cache check", font_id, 16.0);
                assert_eq!(got, expected);
            });
        }
    });
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 4);

    // 2. Tight memory budget triggers LRU eviction without exceeding budget
    let tiny_cache = Arc::new(TextShapingCache::new(4096));
    let tiny_shaper = TextShaper::with_cache(&db, Arc::clone(&tiny_cache));
    for i in 0..40 {
        let text = format!("Budget eviction sample string #{i:03}");
        let _ = tiny_shaper.shape_text(&text, font_id, 16.0);
    }
    assert!(
        tiny_cache.used_bytes() <= 4096,
        "cache exceeded budget: {} > 4096",
        tiny_cache.used_bytes()
    );
    assert!(tiny_cache.len() < 40);
}
