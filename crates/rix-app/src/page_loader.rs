//! High-throughput parallel subresource loader, decoded asset cache, `@import`/`@font-face`
//! pipeline, and speculative origin preconnector for [`crate::BrowserController`].
//!
//! Pipeline stages per navigation:
//! 1. **Wave 1 Discovery**: Runs `rix-html::PreloadScanner` and inspects `<link rel="stylesheet">`,
//!    `<script src>`, `<img src/srcset>`, `<source srcset>`, and `<base href>` in a single pass.
//! 2. **Concurrent Fetch**: Fetches all stylesheets, scripts, and images concurrently via
//!    `rix-net::ResourceManager::load_all_with_priorities` (`Style > Font > Script > Media`),
//!    skipping images already present in [`DecodedAssetCache`].
//! 3. **Parallel CSS Compilation (`rayon`) + `@import` Expansion**: Compiles fetched stylesheets
//!    via the process-wide `Stylesheet::parse_cached` cache and recursively resolves up to 2
//!    levels of `@import` rules in strict cascade order.
//! 4. **Wave 2 Discovery**: Extracts `background-image: url(...)` and `@font-face` WOFF2/TTF/OTF
//!    URLs from all compiled stylesheets and fetches them concurrently.
//! 5. **Parallel Decode (`rayon`)**: Decodes all raster images in parallel via
//!    `rix_layout::decode_raster_batch_parallel`, parses SVG assets into vector paths via
//!    `extract_svg_from_document`, and registers `@font-face` web fonts into `FontFaceSet`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rix_chrome::NetworkEntry;
use rix_core::RixUrl;
use rix_css::{
    BackgroundImageValue, CascadeOrigin, ColorRgba, FontFaceSource, MediaContext, StyleContext,
    Stylesheet,
};
use rix_dom::{Document, NodeData};
use rix_html::{parse_html, PreloadKind, PreloadScanner};
use rix_layout::{
    decode_raster_batch_parallel, extract_svg_from_document, parse_best_srcset_url, AssetRegistry,
    DecodedRasterBytes, DecodedSvg,
};
use rix_net::{HttpClient, Resource, ResourceManager, ResourcePriority};
use rix_text::{FontDatabase, FontFaceSet, FontStyle};
use url::Url;

/// Upper bound on subresources fetched per navigation to bound memory and tail latency.
const MAX_SUBRESOURCES_PER_PAGE: usize = 96;
/// Maximum cached decoded assets shared across tabs.
const MAX_DECODED_ASSET_CACHE_ENTRIES: usize = 256;
/// TTL before an origin is eligible for speculative preconnect again.
const PRECONNECT_TTL: Duration = Duration::from_secs(30);
/// Maximum distinct origins tracked in the speculative preconnector table.
const MAX_PRECONNECT_ORIGINS: usize = 128;

/// Decoded cross-tab cached asset entry.
#[derive(Clone)]
pub enum CachedAsset {
    Raster(Arc<DecodedRasterBytes>),
    Svg(Arc<DecodedSvg>),
    WebFont(Arc<Vec<u8>>),
}

/// Thread-safe decoded raster/SVG/web-font cache shared across tabs and navigations.
#[derive(Clone, Default)]
pub struct DecodedAssetCache {
    entries: Arc<RwLock<HashMap<String, CachedAsset>>>,
}

impl DecodedAssetCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, url: &str) -> Option<CachedAsset> {
        self.entries.read().ok()?.get(url).cloned()
    }

    pub fn insert(&self, url: String, asset: CachedAsset) {
        if let Ok(mut guard) = self.entries.write() {
            if guard.len() >= MAX_DECODED_ASSET_CACHE_ENTRIES {
                guard.clear();
            }
            guard.insert(url, asset);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |g| g.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Deduplicated non-blocking origin preconnector triggered on link hover and omnibox typing.
#[derive(Default)]
pub struct SpeculativePreconnector {
    recent_origins: HashMap<String, Instant>,
}

impl SpeculativePreconnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires a non-blocking TCP+TLS/QUIC preconnect for `url_str`'s origin if not recently warmed.
    pub fn maybe_preconnect(
        &mut self,
        url_str: &str,
        http_client: &HttpClient,
        runtime: &tokio::runtime::Runtime,
    ) -> bool {
        let Ok(parsed) = Url::parse(url_str) else {
            return false;
        };
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let port = parsed.port_or_known_default().unwrap_or(80);
        let origin_key = format!("{}://{host}:{port}", parsed.scheme());
        let now = Instant::now();
        if let Some(&prev) = self.recent_origins.get(&origin_key) {
            if now.saturating_duration_since(prev) < PRECONNECT_TTL {
                return false;
            }
        }
        if self.recent_origins.len() >= MAX_PRECONNECT_ORIGINS {
            self.recent_origins
                .retain(|_, ts| now.saturating_duration_since(*ts) < PRECONNECT_TTL);
        }
        let Ok(rix_url) = RixUrl::parse(&origin_key) else {
            return false;
        };
        self.recent_origins.insert(origin_key, now);
        let client = http_client.clone();
        runtime.spawn(async move {
            client.preconnect(&rix_url).await;
        });
        true
    }
}

/// Decoded `@font-face` payload ready for registration into a tab's `FontDatabase` & `FontFaceSet`.
#[derive(Clone)]
pub struct LoadedWebFont {
    pub family: String,
    pub weight_range: (u16, u16),
    pub italic: bool,
    pub bytes: Arc<Vec<u8>>,
}

/// Timing and cache-hit breakdown for a full subresource load.
#[derive(Debug, Clone, Copy, Default)]
pub struct PageLoadStats {
    pub wave1_ms: f32,
    pub wave2_ms: f32,
    pub css_parse_ms: f32,
    pub image_decode_ms: f32,
    pub js_ms: f32,
    pub subresources_fetched: usize,
    pub network_cache_hits: usize,
    pub decoded_asset_cache_hits: usize,
    pub js_error_count: usize,
}

/// Complete set of subresources loaded for a web navigation.
#[derive(Default)]
pub struct LoadedPageAssets {
    pub external_stylesheets: Vec<Arc<Stylesheet>>,
    pub external_scripts: HashMap<String, String>,
    pub rasters: Vec<(String, Vec<String>, Arc<DecodedRasterBytes>)>,
    pub svgs: Vec<(String, Vec<String>, Arc<DecodedSvg>)>,
    pub web_fonts: Vec<LoadedWebFont>,
    pub network_entries: Vec<NetworkEntry>,
    pub stats: PageLoadStats,
}

impl LoadedPageAssets {
    /// Registers all loaded raster and SVG assets (including raw attribute aliases) into `assets`.
    pub fn register_into_assets(&self, assets: &mut AssetRegistry) {
        for (url, aliases, decoded) in &self.rasters {
            let _ = assets.register_rgba8_shared(
                url,
                decoded.width,
                decoded.height,
                Arc::clone(&decoded.rgba),
            );
            for alias in aliases {
                if alias != url {
                    assets.register_alias(alias, url);
                }
            }
        }
        for (url, aliases, svg) in &self.svgs {
            assets.register_svg(url, (**svg).clone());
            for alias in aliases {
                if alias != url {
                    assets.register_alias(alias, url);
                }
            }
        }
    }

    /// Registers all loaded `@font-face` web fonts into `font_db` and `face_set`.
    /// Returns the number of successfully decoded web fonts.
    pub fn register_web_fonts(
        &self,
        font_db: &mut FontDatabase,
        face_set: &mut FontFaceSet,
    ) -> usize {
        let mut count = 0usize;
        for font in &self.web_fonts {
            let style = if font.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
            if face_set
                .register_font_bytes(
                    font_db,
                    font.family.clone(),
                    font.weight_range,
                    style,
                    Vec::new(),
                    &font.bytes,
                )
                .is_ok()
            {
                count += 1;
            }
        }
        count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubresourceRole {
    Stylesheet,
    Script,
    Image,
    Font,
}

struct DiscoveredRefs {
    doc: Document,
    effective_base: Option<Url>,
    stylesheets: Vec<(String, String)>, // (raw_href, resolved_url)
    scripts: Vec<(String, String)>,     // (raw_src, resolved_url)
    images: Vec<(String, String)>,      // (raw_src_or_candidate, resolved_url)
    inline_styles: Vec<String>,
}

/// Discovers and concurrently fetches all external stylesheets, `@import`s, scripts, images
/// (`<img>`, `<picture><source>`, CSS `background-image: url(...)`), SVGs, and `@font-face`
/// web fonts referenced by `html` at `document_url`.
pub fn load_page_subresources(
    document_url: &str,
    html: &str,
    media: &MediaContext,
    resource_manager: &ResourceManager,
    runtime: &tokio::runtime::Runtime,
    asset_cache: &DecodedAssetCache,
) -> LoadedPageAssets {
    let mut out = LoadedPageAssets::default();
    let allow_file_scheme = document_url.starts_with("file://");
    let discovered = discover_wave1_refs(document_url, html, media);

    // Collect image alias map (resolved_url -> Vec<raw_alias>).
    let mut image_aliases: HashMap<String, Vec<String>> = HashMap::new();
    for (raw, resolved) in &discovered.images {
        let list = image_aliases.entry(resolved.clone()).or_default();
        if !list.contains(raw) {
            list.push(raw.clone());
        }
    }

    // Build deduplicated Wave 1 fetch list.
    let mut wave1_queue: Vec<(String, ResourcePriority, SubresourceRole)> = Vec::new();
    let mut seen_wave1: HashSet<String> = HashSet::new();
    let mut cached_rasters: HashMap<String, Arc<DecodedRasterBytes>> = HashMap::new();
    let mut cached_svgs: HashMap<String, Arc<DecodedSvg>> = HashMap::new();

    for (_, resolved) in &discovered.stylesheets {
        if wave1_queue.len() >= MAX_SUBRESOURCES_PER_PAGE {
            break;
        }
        if is_fetchable_url(resolved, allow_file_scheme) && seen_wave1.insert(resolved.clone()) {
            wave1_queue.push((
                resolved.clone(),
                ResourcePriority::Style,
                SubresourceRole::Stylesheet,
            ));
        }
    }
    for (_, resolved) in &discovered.scripts {
        if wave1_queue.len() >= MAX_SUBRESOURCES_PER_PAGE {
            break;
        }
        if is_fetchable_url(resolved, allow_file_scheme) && seen_wave1.insert(resolved.clone()) {
            wave1_queue.push((
                resolved.clone(),
                ResourcePriority::Script,
                SubresourceRole::Script,
            ));
        }
    }
    for (_, resolved) in &discovered.images {
        if let Some(hit) = asset_cache.get(resolved) {
            match hit {
                CachedAsset::Raster(r) => {
                    cached_rasters.insert(resolved.clone(), r);
                    out.stats.decoded_asset_cache_hits += 1;
                    continue;
                }
                CachedAsset::Svg(s) => {
                    cached_svgs.insert(resolved.clone(), s);
                    out.stats.decoded_asset_cache_hits += 1;
                    continue;
                }
                CachedAsset::WebFont(_) => {}
            }
        }
        if wave1_queue.len() >= MAX_SUBRESOURCES_PER_PAGE {
            break;
        }
        if is_fetchable_url(resolved, allow_file_scheme) && seen_wave1.insert(resolved.clone()) {
            wave1_queue.push((
                resolved.clone(),
                ResourcePriority::Media,
                SubresourceRole::Image,
            ));
        }
    }

    let t_wave1 = Instant::now();
    let wave1_results = fetch_batch(
        &wave1_queue,
        allow_file_scheme,
        resource_manager,
        runtime,
        &mut out.network_entries,
        &mut out.stats,
    );
    out.stats.wave1_ms = t_wave1.elapsed().as_secs_f32() * 1000.0;

    // Partition Wave 1 payloads by role.
    let mut fetched_css_by_url: HashMap<String, String> = HashMap::new();
    let mut raw_image_payloads: Vec<(String, String, Vec<u8>)> = Vec::new(); // (url, mime, bytes)

    for ((url, _, role), res_opt) in wave1_queue.into_iter().zip(wave1_results) {
        let Some(res) = res_opt else {
            continue;
        };
        match role {
            SubresourceRole::Stylesheet => {
                fetched_css_by_url.insert(url, String::from_utf8_lossy(&res.data).into_owned());
            }
            SubresourceRole::Script => {
                let code = String::from_utf8_lossy(&res.data).into_owned();
                out.external_scripts.insert(url, code);
            }
            SubresourceRole::Image => {
                raw_image_payloads.push((url, res.mime_type, res.data));
            }
            SubresourceRole::Font => {}
        }
    }

    // Also map raw `<script src>` aliases to their fetched script text.
    for (raw_src, resolved) in &discovered.scripts {
        if raw_src != resolved {
            if let Some(code) = out.external_scripts.get(resolved).cloned() {
                out.external_scripts.insert(raw_src.clone(), code);
            }
        }
    }

    // Parallel CSS compilation via `Stylesheet::parse_cached`.
    let t_css = Instant::now();
    let ordered_ext_sources: Vec<(String, String)> = discovered
        .stylesheets
        .iter()
        .filter_map(|(_, resolved)| {
            fetched_css_by_url
                .get(resolved)
                .map(|css| (resolved.clone(), css.clone()))
        })
        .collect();

    let compiled_ext: Vec<(String, Arc<Stylesheet>)> = ordered_ext_sources
        .par_iter()
        .map(|(url, css)| {
            (
                url.clone(),
                Stylesheet::parse_cached(css, CascadeOrigin::Author, media),
            )
        })
        .collect();

    // Parse inline `<style>` blocks (cached!) so we also discover their `@import`,
    // `background-image: url(...)`, and `@font-face` rules.
    let compiled_inline: Vec<Arc<Stylesheet>> = discovered
        .inline_styles
        .par_iter()
        .map(|css| Stylesheet::parse_cached(css, CascadeOrigin::Author, media))
        .collect();

    // Resolve up to 2 levels of `@import` stylesheets in parallel batches.
    let mut visited_css_urls: HashSet<String> =
        compiled_ext.iter().map(|(u, _)| u.clone()).collect();
    let mut imported_sheets_by_parent: HashMap<String, Vec<(String, Arc<Stylesheet>)>> =
        HashMap::new();
    let mut inline_imported_sheets: Vec<(String, Arc<Stylesheet>)> = Vec::new();

    let doc_base_str = discovered
        .effective_base
        .as_ref()
        .map_or(document_url, Url::as_str)
        .to_string();

    // Collect level-1 `@import`s from external stylesheets and inline `<style>` tags.
    let mut import_requests: Vec<(Option<String>, String)> = Vec::new(); // (parent_css_url_or_None_for_inline, resolved_import_url)
    for (sheet_url, sheet) in &compiled_ext {
        for imp in &sheet.imports {
            if let Some(resolved) = resolve_url(sheet_url, imp) {
                if is_fetchable_url(&resolved, allow_file_scheme)
                    && visited_css_urls.insert(resolved.clone())
                {
                    import_requests.push((Some(sheet_url.clone()), resolved));
                }
            }
        }
    }
    for sheet in &compiled_inline {
        for imp in &sheet.imports {
            if let Some(resolved) = resolve_url(&doc_base_str, imp) {
                if is_fetchable_url(&resolved, allow_file_scheme)
                    && visited_css_urls.insert(resolved.clone())
                {
                    import_requests.push((None, resolved));
                }
            }
        }
    }

    if !import_requests.is_empty() {
        let queue: Vec<(String, ResourcePriority, SubresourceRole)> = import_requests
            .iter()
            .take(MAX_SUBRESOURCES_PER_PAGE / 4)
            .map(|(_, url)| {
                (
                    url.clone(),
                    ResourcePriority::Style,
                    SubresourceRole::Stylesheet,
                )
            })
            .collect();
        let results = fetch_batch(
            &queue,
            allow_file_scheme,
            resource_manager,
            runtime,
            &mut out.network_entries,
            &mut out.stats,
        );
        let parsed_imports: Vec<(Option<String>, String, Arc<Stylesheet>)> = import_requests
            .into_iter()
            .zip(results)
            .filter_map(|((parent, url), res_opt)| {
                let res = res_opt?;
                let css = String::from_utf8_lossy(&res.data);
                let sheet = Stylesheet::parse_cached(&css, CascadeOrigin::Author, media);
                Some((parent, url, sheet))
            })
            .collect();

        // Check level-2 `@import`s inside fetched imports.
        let mut level2_reqs: Vec<(Option<String>, String)> = Vec::new();
        for (parent, imp_url, sheet) in &parsed_imports {
            for nested in &sheet.imports {
                if let Some(resolved) = resolve_url(imp_url, nested) {
                    if is_fetchable_url(&resolved, allow_file_scheme)
                        && visited_css_urls.insert(resolved.clone())
                    {
                        level2_reqs.push((parent.clone(), resolved));
                    }
                }
            }
        }
        if !level2_reqs.is_empty() {
            let q2: Vec<(String, ResourcePriority, SubresourceRole)> = level2_reqs
                .iter()
                .take(16)
                .map(|(_, u)| {
                    (
                        u.clone(),
                        ResourcePriority::Style,
                        SubresourceRole::Stylesheet,
                    )
                })
                .collect();
            let r2 = fetch_batch(
                &q2,
                allow_file_scheme,
                resource_manager,
                runtime,
                &mut out.network_entries,
                &mut out.stats,
            );
            for ((parent, url), res_opt) in level2_reqs.into_iter().zip(r2) {
                if let Some(res) = res_opt {
                    let css = String::from_utf8_lossy(&res.data);
                    let sheet = Stylesheet::parse_cached(&css, CascadeOrigin::Author, media);
                    match parent {
                        Some(p) => imported_sheets_by_parent
                            .entry(p)
                            .or_default()
                            .push((url, sheet)),
                        None => inline_imported_sheets.push((url, sheet)),
                    }
                }
            }
        }

        for (parent, url, sheet) in parsed_imports {
            match parent {
                Some(p) => imported_sheets_by_parent
                    .entry(p)
                    .or_default()
                    .push((url, sheet)),
                None => inline_imported_sheets.push((url, sheet)),
            }
        }
    }

    // Assemble final ordered external stylesheets (`@import` sheets precede their parent sheet!).
    let mut all_sheets_with_base: Vec<(String, Arc<Stylesheet>)> = Vec::new();
    for (url, sheet) in compiled_ext {
        if let Some(imps) = imported_sheets_by_parent.remove(&url) {
            for (imp_url, imp_sheet) in imps {
                out.external_stylesheets.push(Arc::clone(&imp_sheet));
                all_sheets_with_base.push((imp_url, imp_sheet));
            }
        }
        out.external_stylesheets.push(Arc::clone(&sheet));
        all_sheets_with_base.push((url, sheet));
    }
    for (imp_url, imp_sheet) in inline_imported_sheets {
        out.external_stylesheets.push(Arc::clone(&imp_sheet));
        all_sheets_with_base.push((imp_url, imp_sheet));
    }
    for inline_sheet in compiled_inline {
        all_sheets_with_base.push((doc_base_str.clone(), inline_sheet));
    }
    out.stats.css_parse_ms = t_css.elapsed().as_secs_f32() * 1000.0;

    // Wave 2 Discovery (Used-Only!): resolve styles on the parsed DOM (~2 ms) so we ONLY fetch
    // `background-image: url(...)` and `@font-face` families ACTUALLY matched by visible DOM
    // elements instead of downloading hundreds of unused framework CSS sprites & banners!
    let mut wave2_queue: Vec<(String, ResourcePriority, SubresourceRole)> = Vec::new();
    let mut seen_wave2: HashSet<String> = seen_wave1;
    let mut pending_font_specs: Vec<(String, String, (u16, u16), bool)> = Vec::new(); // (url, family, weight_range, italic)

    let mut used_bg_urls: HashSet<String> = HashSet::new();
    let mut used_font_families: HashSet<String> = HashSet::new();
    {
        let mut probe_doc = discovered.doc;
        let mut probe_styles = StyleContext::new(*media);
        for (_, sheet) in &all_sheets_with_base {
            probe_styles.add_stylesheet(Arc::clone(sheet));
        }
        probe_styles.cascade_full(&mut probe_doc);
        for node in probe_doc.descendants(probe_doc.root()) {
            let Some(st) = probe_styles.style_for(node) else {
                continue;
            };
            if st.display == rix_css::DisplayMode::None {
                continue;
            }
            for layer in &st.visual.background_layers {
                if let BackgroundImageValue::Url(u) = &layer.image {
                    let trimmed = u.as_str().trim();
                    if !trimmed.is_empty() {
                        used_bg_urls.insert(trimmed.to_string());
                    }
                }
            }
            if let BackgroundImageValue::Url(u) = &st.visual.mask_image {
                let trimmed = u.as_str().trim();
                if !trimmed.is_empty() {
                    used_bg_urls.insert(trimmed.to_string());
                }
            }
            for fam in &st.text.font_family {
                let lower = fam
                    .as_str()
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_ascii_lowercase();
                if !lower.is_empty() {
                    used_font_families.insert(lower);
                }
            }
        }
    }

    for (base_url, sheet) in &all_sheets_with_base {
        for raw_img in sheet.referenced_image_urls() {
            if !used_bg_urls.contains(&raw_img) {
                continue;
            }
            let Some(resolved) = resolve_url(base_url, &raw_img) else {
                continue;
            };
            if is_non_image_media_url(&resolved) {
                continue;
            }
            let aliases = image_aliases.entry(resolved.clone()).or_default();
            if !aliases.contains(&raw_img) {
                aliases.push(raw_img.clone());
            }
            if cached_rasters.contains_key(&resolved) || cached_svgs.contains_key(&resolved) {
                continue;
            }
            if let Some(hit) = asset_cache.get(&resolved) {
                match hit {
                    CachedAsset::Raster(r) => {
                        cached_rasters.insert(resolved.clone(), r);
                        out.stats.decoded_asset_cache_hits += 1;
                        continue;
                    }
                    CachedAsset::Svg(s) => {
                        cached_svgs.insert(resolved.clone(), s);
                        out.stats.decoded_asset_cache_hits += 1;
                        continue;
                    }
                    CachedAsset::WebFont(_) => {}
                }
            }
            if wave2_queue.len() < MAX_SUBRESOURCES_PER_PAGE
                && is_fetchable_url(&resolved, allow_file_scheme)
                && seen_wave2.insert(resolved.clone())
            {
                wave2_queue.push((resolved, ResourcePriority::Media, SubresourceRole::Image));
            }
        }

        for ff in &sheet.font_faces {
            let Some(ref family) = ff.family else {
                continue;
            };
            let fam_lower = family
                .as_str()
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_ascii_lowercase();
            if !used_font_families.contains(&fam_lower) {
                continue;
            }
            let Some(raw_font_url) = pick_best_font_face_url(&ff.sources) else {
                continue;
            };
            let Some(resolved) = resolve_url(base_url, raw_font_url) else {
                continue;
            };
            let w_min = ff.weight_range.0.round().clamp(100.0, 900.0) as u16;
            let w_max = ff.weight_range.1.round().clamp(100.0, 900.0) as u16;
            let weight_range = (w_min.min(w_max), w_min.max(w_max));

            if let Some(CachedAsset::WebFont(bytes)) = asset_cache.get(&resolved) {
                out.web_fonts.push(LoadedWebFont {
                    family: family.as_str().to_string(),
                    weight_range,
                    italic: ff.italic,
                    bytes,
                });
                out.stats.decoded_asset_cache_hits += 1;
                continue;
            }

            pending_font_specs.push((
                resolved.clone(),
                family.as_str().to_string(),
                weight_range,
                ff.italic,
            ));
            if wave2_queue.len() < MAX_SUBRESOURCES_PER_PAGE
                && is_fetchable_url(&resolved, allow_file_scheme)
                && seen_wave2.insert(resolved.clone())
            {
                wave2_queue.push((resolved, ResourcePriority::Font, SubresourceRole::Font));
            }
        }
    }

    let t_wave2 = Instant::now();
    let mut fetched_fonts_by_url: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    if !wave2_queue.is_empty() {
        let wave2_results = fetch_batch(
            &wave2_queue,
            allow_file_scheme,
            resource_manager,
            runtime,
            &mut out.network_entries,
            &mut out.stats,
        );
        for ((url, _, role), res_opt) in wave2_queue.into_iter().zip(wave2_results) {
            let Some(res) = res_opt else {
                continue;
            };
            match role {
                SubresourceRole::Image => raw_image_payloads.push((url, res.mime_type, res.data)),
                SubresourceRole::Font => {
                    let bytes = Arc::new(res.data);
                    asset_cache.insert(url.clone(), CachedAsset::WebFont(Arc::clone(&bytes)));
                    fetched_fonts_by_url.insert(url, bytes);
                }
                SubresourceRole::Stylesheet | SubresourceRole::Script => {}
            }
        }
    }
    out.stats.wave2_ms = t_wave2.elapsed().as_secs_f32() * 1000.0;

    for (url, family, weight_range, italic) in pending_font_specs {
        if let Some(bytes) = fetched_fonts_by_url.get(&url) {
            out.web_fonts.push(LoadedWebFont {
                family,
                weight_range,
                italic,
                bytes: Arc::clone(bytes),
            });
        }
    }

    // Parallel Image & SVG decode across CPU cores via `rayon`.
    let t_decode = Instant::now();
    let mut raster_inputs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut svg_inputs: Vec<(String, Vec<u8>)> = Vec::new();

    for (url, mime, bytes) in raw_image_payloads {
        if looks_like_svg(&url, &mime, &bytes) {
            svg_inputs.push((url, bytes));
        } else {
            raster_inputs.push((url, bytes));
        }
    }

    if !raster_inputs.is_empty() {
        let decoded = decode_raster_batch_parallel(&raster_inputs);
        for (url, res) in decoded {
            if let Ok(img) = res {
                let shared = Arc::new(img);
                asset_cache.insert(url.clone(), CachedAsset::Raster(Arc::clone(&shared)));
                cached_rasters.insert(url, shared);
            }
        }
    }

    if !svg_inputs.is_empty() {
        let decoded_svgs: Vec<(String, Option<DecodedSvg>)> = svg_inputs
            .par_iter()
            .map(|(url, bytes)| {
                let markup = String::from_utf8_lossy(bytes);
                let doc = parse_html(&markup);
                let svg = extract_svg_from_document(&doc, ColorRgba::BLACK);
                (url.clone(), svg)
            })
            .collect();
        for (url, svg_opt) in decoded_svgs {
            if let Some(svg) = svg_opt {
                let shared = Arc::new(svg);
                asset_cache.insert(url.clone(), CachedAsset::Svg(Arc::clone(&shared)));
                cached_svgs.insert(url, shared);
            }
        }
    }
    out.stats.image_decode_ms = t_decode.elapsed().as_secs_f32() * 1000.0;

    for (url, decoded) in cached_rasters {
        let aliases = image_aliases.remove(&url).unwrap_or_default();
        out.rasters.push((url, aliases, decoded));
    }
    for (url, svg) in cached_svgs {
        let aliases = image_aliases.remove(&url).unwrap_or_default();
        out.svgs.push((url, aliases, svg));
    }

    out
}

fn discover_wave1_refs(document_url: &str, html: &str, media: &MediaContext) -> DiscoveredRefs {
    // 1. Run the streaming HTML PreloadScanner to discover `<base>`, preload links, etc.
    let mut scanner = PreloadScanner::new(Some(document_url));
    scanner.push_bytes(html.as_bytes());
    let hints = scanner.take_hints();
    let effective_base = scanner
        .base_url()
        .cloned()
        .or_else(|| Url::parse(document_url).ok());
    let base_str = effective_base.as_ref().map_or(document_url, Url::as_str);

    // 2. Inspect parsed HTML elements for exact document order & raw attribute aliases.
    let doc: Document = parse_html(html);
    let mut stylesheets: Vec<(String, String)> = Vec::new();
    let mut scripts: Vec<(String, String)> = Vec::new();
    let mut images: Vec<(String, String)> = Vec::new();
    let mut inline_styles: Vec<String> = Vec::new();

    let mut seen_css = HashSet::new();
    let mut seen_js = HashSet::new();
    let mut seen_img = HashSet::new();

    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(el)) = doc.get(node) else {
            continue;
        };
        let tag: &str = el.name.local.as_ref();
        if tag.eq_ignore_ascii_case("link") {
            let rel = el.get_attribute("rel").unwrap_or("");
            let is_stylesheet = rel
                .split_ascii_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("stylesheet"));
            let is_alternate = rel
                .split_ascii_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("alternate"));
            let is_modulepreload = rel
                .split_ascii_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("modulepreload"));
            if is_stylesheet && !is_alternate && !el.has_attribute("disabled") {
                let media_ok = el
                    .get_attribute("media")
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .is_none_or(|m| media.evaluate_media_query_str(m));
                if media_ok {
                    if let Some(href) = el.get_attribute("href").map(str::trim) {
                        if !href.is_empty() {
                            if let Some(resolved) = resolve_url(base_str, href) {
                                if seen_css.insert(resolved.clone()) {
                                    stylesheets.push((href.to_string(), resolved));
                                }
                            }
                        }
                    }
                }
            } else if is_modulepreload {
                if let Some(href) = el.get_attribute("href").map(str::trim) {
                    if !href.is_empty() {
                        if let Some(resolved) = resolve_url(base_str, href) {
                            if seen_js.insert((href.to_string(), resolved.clone())) {
                                scripts.push((href.to_string(), resolved));
                            }
                        }
                    }
                }
            }
        } else if tag.eq_ignore_ascii_case("script") {
            if let Some(src) = el.get_attribute("src").map(str::trim) {
                if !src.is_empty() {
                    if let Some(resolved) = resolve_url(base_str, src) {
                        if seen_js.insert((src.to_string(), resolved.clone())) {
                            scripts.push((src.to_string(), resolved));
                        }
                    }
                }
            }
        } else if tag.eq_ignore_ascii_case("img") {
            if let Some(srcset) = el.get_attribute("srcset") {
                if let Some(best) = parse_best_srcset_url(srcset) {
                    if let Some(resolved) = resolve_url(base_str, &best) {
                        if !is_non_image_media_url(&resolved)
                            && seen_img.insert((best.clone(), resolved.clone()))
                        {
                            images.push((best, resolved));
                        }
                    }
                }
            }
            if let Some(src) = el.get_attribute("src").map(str::trim) {
                if !src.is_empty() {
                    if let Some(resolved) = resolve_url(base_str, src) {
                        if !is_non_image_media_url(&resolved)
                            && seen_img.insert((src.to_string(), resolved.clone()))
                        {
                            images.push((src.to_string(), resolved));
                        }
                    }
                }
            }
        } else if tag.eq_ignore_ascii_case("source") {
            // Only fetch `<source srcset>` when inside `<picture>` (NEVER `<video>`/`<audio>` streams!).
            let in_picture = doc.parent(node).is_some_and(|p| {
                matches!(
                    doc.get(p),
                    Some(NodeData::Element(pe)) if pe.name.local.eq_str_ignore_ascii_case("picture")
                )
            });
            if in_picture {
                if let Some(srcset) = el.get_attribute("srcset") {
                    if let Some(best) = parse_best_srcset_url(srcset) {
                        if let Some(resolved) = resolve_url(base_str, &best) {
                            if !is_non_image_media_url(&resolved)
                                && seen_img.insert((best.clone(), resolved.clone()))
                            {
                                images.push((best, resolved));
                            }
                        }
                    }
                }
            }
        } else if tag.eq_ignore_ascii_case("style") {
            let mut css = String::new();
            for ch in doc.children(node) {
                if let Some(NodeData::Text(t)) = doc.get(ch) {
                    css.push_str(t);
                }
            }
            if !css.trim().is_empty() {
                inline_styles.push(css);
            }
        }
    }

    // Incorporate any additional `<link rel="preload">` hints from PreloadScanner (decoding `&amp;`
    // entities and skipping non-image media streams like `.webm`/`.mp4`).
    for hint in hints {
        let clean_url = hint.url.replace("&amp;", "&");
        match hint.kind {
            PreloadKind::Image => {
                if !is_non_image_media_url(&clean_url)
                    && seen_img.insert((clean_url.clone(), clean_url.clone()))
                {
                    images.push((clean_url.clone(), clean_url));
                }
            }
            PreloadKind::Style => {
                if seen_css.insert(clean_url.clone()) {
                    stylesheets.push((clean_url.clone(), clean_url));
                }
            }
            PreloadKind::Script | PreloadKind::Module => {}
            PreloadKind::Font => {}
        }
    }

    DiscoveredRefs {
        doc,
        effective_base,
        stylesheets,
        scripts,
        images,
        inline_styles,
    }
}

fn fetch_batch(
    queue: &[(String, ResourcePriority, SubresourceRole)],
    allow_file_scheme: bool,
    resource_manager: &ResourceManager,
    runtime: &tokio::runtime::Runtime,
    network_entries: &mut Vec<NetworkEntry>,
    stats: &mut PageLoadStats,
) -> Vec<Option<Resource>> {
    if queue.is_empty() {
        return Vec::new();
    }
    let mut results: Vec<Option<Resource>> = vec![None; queue.len()];
    let mut net_indices: Vec<usize> = Vec::new();
    let mut net_reqs: Vec<(RixUrl, ResourcePriority)> = Vec::new();

    for (idx, (url_str, priority, _)) in queue.iter().enumerate() {
        if allow_file_scheme {
            if let Some(file_path) = url_str.strip_prefix("file://") {
                let start = Instant::now();
                let clean_path = file_path.split(['?', '#']).next().unwrap_or(file_path);
                if let Ok(bytes) = std::fs::read(Path::new(clean_path)) {
                    let mime = guess_mime_from_path(clean_path);
                    let elapsed = start.elapsed().as_secs_f32() * 1000.0;
                    network_entries.push(NetworkEntry {
                        url: url_str.clone(),
                        method: "GET".to_string(),
                        protocol: "file".to_string(),
                        status: 200,
                        mime: mime.clone(),
                        bytes: bytes.len(),
                        cache_hit: false,
                        ttfb_ms: elapsed,
                        total_ms: elapsed,
                    });
                    stats.subresources_fetched += 1;
                    if let Ok(parsed) = RixUrl::parse(url_str) {
                        results[idx] = Some(Resource {
                            url: parsed,
                            mime_type: mime,
                            data: bytes,
                            etag: None,
                            last_modified: None,
                            from_cache: false,
                        });
                    }
                }
                continue;
            }
        }
        if let Ok(parsed) = RixUrl::parse(url_str) {
            net_indices.push(idx);
            net_reqs.push((parsed, *priority));
        }
    }

    if !net_reqs.is_empty() {
        let start = Instant::now();
        // Run all subresources concurrently with a PER-REQUEST 4s timeout so a single lagging CDN
        // host NEVER discards already-completed stylesheets, scripts, or images!
        let fetched = runtime.block_on(async {
            let sem = Arc::new(tokio::sync::Semaphore::new(16));
            let mut handles = Vec::with_capacity(net_reqs.len());
            for (url, prio) in net_reqs {
                let rm = resource_manager.clone();
                let sem = Arc::clone(&sem);
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await.ok();
                    match tokio::time::timeout(
                        Duration::from_secs(4),
                        rm.load_prioritized(&url, prio),
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(_) => Err(rix_net::NetError::Timeout(Duration::from_secs(4))),
                    }
                }));
            }
            let mut out = Vec::with_capacity(handles.len());
            for h in handles {
                match h.await {
                    Ok(res) => out.push(res),
                    Err(_) => out.push(Err(rix_net::NetError::Http(
                        "subresource task cancelled".into(),
                    ))),
                }
            }
            out
        });
        let batch_ms = start.elapsed().as_secs_f32() * 1000.0;
        for (slot_idx, res) in net_indices.into_iter().zip(fetched) {
            let url_str = &queue[slot_idx].0;
            match res {
                Ok(resource) => {
                    stats.subresources_fetched += 1;
                    if resource.from_cache {
                        stats.network_cache_hits += 1;
                    }
                    network_entries.push(NetworkEntry {
                        url: url_str.clone(),
                        method: "GET".to_string(),
                        protocol: if resource.from_cache {
                            "cache".to_string()
                        } else if url_str.starts_with("data:") {
                            "data".to_string()
                        } else {
                            "http".to_string()
                        },
                        status: 200,
                        mime: resource.mime_type.clone(),
                        bytes: resource.data.len(),
                        cache_hit: resource.from_cache,
                        ttfb_ms: batch_ms,
                        total_ms: batch_ms,
                    });
                    results[slot_idx] = Some(resource);
                }
                Err(_) => {
                    network_entries.push(NetworkEntry {
                        url: url_str.clone(),
                        method: "GET".to_string(),
                        protocol: "net".to_string(),
                        status: 0,
                        mime: "application/octet-stream".to_string(),
                        bytes: 0,
                        cache_hit: false,
                        ttfb_ms: batch_ms,
                        total_ms: batch_ms,
                    });
                }
            }
        }
    }

    results
}

fn resolve_url(base_url: &str, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("mailto:")
    {
        return None;
    }
    if trimmed.starts_with("data:")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return Some(trimmed.to_string());
    }
    Url::parse(base_url)
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|u| u.to_string())
}

fn is_fetchable_url(url: &str, allow_file_scheme: bool) -> bool {
    url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("data:")
        || (allow_file_scheme && url.starts_with("file://"))
}

fn is_non_image_media_url(url: &str) -> bool {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let lower = clean.to_ascii_lowercase();
    lower.ends_with(".webm")
        || lower.ends_with(".mp4")
        || lower.ends_with(".m4v")
        || lower.ends_with(".mov")
        || lower.ends_with(".ogv")
        || lower.ends_with(".ogg")
        || lower.ends_with(".mp3")
        || lower.ends_with(".wav")
        || lower.ends_with(".flac")
        || lower.ends_with(".m3u8")
        || lower.ends_with(".mpd")
}

fn pick_best_font_face_url(sources: &[FontFaceSource]) -> Option<&str> {
    let mut best: Option<(u8, &str)> = None;
    for src in sources {
        let FontFaceSource::Url { url, format } = src else {
            continue;
        };
        let u = url.as_str().trim();
        if u.is_empty() {
            continue;
        }
        let lower_fmt = format
            .as_ref()
            .map(|f| f.as_str().to_ascii_lowercase())
            .unwrap_or_default();
        let lower_url = u.split(['?', '#']).next().unwrap_or(u).to_ascii_lowercase();
        let rank = if lower_fmt.contains("woff2") || lower_url.ends_with(".woff2") {
            4
        } else if lower_fmt == "woff" || lower_url.ends_with(".woff") {
            3
        } else if lower_fmt.contains("truetype")
            || lower_url.ends_with(".ttf")
            || lower_fmt.contains("opentype")
            || lower_url.ends_with(".otf")
        {
            2
        } else if lower_fmt.contains("embedded-opentype")
            || lower_url.ends_with(".eot")
            || lower_fmt.contains("svg")
        {
            0
        } else {
            1
        };
        if rank == 0 {
            continue;
        }
        if best.is_none_or(|(prev, _)| rank > prev) {
            best = Some((rank, u));
        }
    }
    best.map(|(_, u)| u)
}

fn looks_like_svg(url: &str, mime: &str, bytes: &[u8]) -> bool {
    if mime.to_ascii_lowercase().contains("svg") {
        return true;
    }
    let clean_url = url.split(['?', '#']).next().unwrap_or(url);
    if clean_url.to_ascii_lowercase().ends_with(".svg") {
        return true;
    }
    let prefix = std::str::from_utf8(&bytes[..bytes.len().min(256)]).unwrap_or("");
    let trimmed = prefix.trim_start();
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
}

fn guess_mime_from_path(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".css") {
        "text/css".to_string()
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "application/javascript".to_string()
    } else if lower.ends_with(".svg") {
        "image/svg+xml".to_string()
    } else if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".woff2") {
        "font/woff2".to_string()
    } else if lower.ends_with(".woff") {
        "font/woff".to_string()
    } else if lower.ends_with(".ttf") {
        "font/ttf".to_string()
    } else if lower.ends_with(".otf") {
        "font/otf".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}
