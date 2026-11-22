use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rix_chrome::{
    chrome_asset_provider, chrome_stylesheet, palette_for, parse_chrome_hit_id,
    render_bookmarks_html, render_browser_chrome_html, render_crashed_html, render_error_html,
    render_history_html, render_hud_html, render_newtab_html, render_overlay_html,
    render_settings_html, resolve_omnibox_input, resolve_omnibox_input_with_settings,
    BrowserSettings, ChromeHitTarget, ChromeRenderer, ChromeState, CookiePolicy, DevHudState,
    FrameStageSample, HudTab, JitMode, NetworkEntry, SearchEngine, SecurityState, SettingsSection,
    TabLayoutMode, ThemeMode, URL_BOOKMARKS, URL_HISTORY, URL_NEWTAB, URL_SETTINGS,
};
use rix_core::{Point, Size};
use rix_ipc::PipelineId;
use rix_net::{HttpClient, ResourceManager};
use rix_render::{encode_display_list, EncodeOptions, ResourceRegistry, VelloRenderer};
use rix_text::FontDatabase;
use vello::peniko::Color;
use vello::Scene;

fn shared_fonts() -> &'static FontDatabase {
    static DB: OnceLock<FontDatabase> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = FontDatabase::new();
        db.scan_system_fonts();
        db
    })
}

fn render_layer_rgba(
    list: &rix_render::DisplayList,
    pipeline: PipelineId,
    resources: &mut ResourceRegistry,
    width: u32,
    height: u32,
    clear: [u8; 4],
) -> Vec<u8> {
    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        list,
        resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut renderer = VelloRenderer::new();
    renderer
        .render_to_rgba_blocking(
            &scene,
            width,
            height,
            Color::from_rgba8(clear[0], clear[1], clear[2], clear[3]),
        )
        .expect("headless Vello render succeeds")
}

fn parse_hex_rgb(hex: &str) -> (f64, f64, f64) {
    let clean = hex.trim_start_matches('#');
    let r = u8::from_str_radix(&clean[0..2], 16).expect("valid hex r");
    let g = u8::from_str_radix(&clean[2..4], 16).expect("valid hex g");
    let b = u8::from_str_radix(&clean[4..6], 16).expect("valid hex b");
    (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    )
}

fn relative_luminance(hex: &str) -> f64 {
    let linear = |c: f64| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = parse_hex_rgb(hex);
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn wcag_contrast_ratio(fg: &str, bg: &str) -> f64 {
    let l1 = relative_luminance(fg);
    let l2 = relative_luminance(bg);
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// Strips HTML tags so copy audits inspect visible user-facing text only.
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Automated `unslop` audit across CSS tokens and rendered HTML documents.
fn assert_zero_unslop_violations(doc_name: &str, html: &str, css: &str) {
    // 1. Zero purple/indigo/pink AI gradients, glows, pulse rings, or uppercase micro-labels.
    for forbidden in [
        "linear-gradient",
        "radial-gradient",
        "conic-gradient",
        "background-clip: text",
        "animate-pulse",
        "animate-ping",
        "box-shadow: 0 0",
        "text-transform: uppercase",
    ] {
        assert!(
            !css.contains(forbidden) && !html.contains(forbidden),
            "[{doc_name}] unslop violation: forbidden pattern `{forbidden}`"
        );
    }

    // 2. Zero emoji characters anywhere in UI markup or copy.
    for ch in html.chars() {
        let cp = ch as u32;
        let is_emoji =
            (0x1F300..=0x1FAFF).contains(&cp) || (0x2600..=0x27BF).contains(&cp) || cp == 0x2728;
        assert!(
            !is_emoji,
            "[{doc_name}] unslop violation: emoji character U+{cp:04X} found in Chrome UI"
        );
    }

    // 3. Zero hype words or exclamation marks in visible UI copy.
    let visible = strip_html_tags(html);
    assert!(
        !visible.contains('!'),
        "[{doc_name}] unslop violation: exclamation mark in visible UI copy: {visible}"
    );
    let lower = visible.to_ascii_lowercase();
    for hype in [
        "supercharge",
        "seamless",
        "unlock",
        "magic",
        "revolutionize",
        "blazing fast",
        "next-gen",
    ] {
        assert!(
            !lower.contains(hype),
            "[{doc_name}] unslop violation: hype copy `{hype}` found"
        );
    }

    // 4. Zero weight 800/900 headings.
    assert!(
        !css.contains("font-weight: 800") && !css.contains("font-weight: 900"),
        "[{doc_name}] unslop violation: excessive heading weight 800/900"
    );
}

#[test]
fn wcag_contrast_ratios_pass_aa_across_all_dark_and_light_roles() {
    for theme in [ThemeMode::Dark, ThemeMode::Light] {
        let p = palette_for(theme);
        for (bg_name, bg) in [("bg_toolbar", p.bg_toolbar), ("bg_card", p.bg_card)] {
            for (fg_name, fg) in [
                ("text_primary", p.text_primary),
                ("text_secondary", p.text_secondary),
                ("text_muted", p.text_muted),
            ] {
                let ratio = wcag_contrast_ratio(fg, bg);
                assert!(
                    ratio >= 4.5,
                    "[{theme:?}] WCAG AA contrast failure: {fg_name} ({fg}) on {bg_name} ({bg}) = {ratio:.2}:1 (< 4.5:1)"
                );
            }
        }
        let btn_ratio = wcag_contrast_ratio(p.accent_fg, p.accent_solid);
        assert!(
            btn_ratio >= 4.5,
            "[{theme:?}] WCAG AA contrast failure on primary button: {btn_ratio:.2}:1"
        );
    }
}

#[test]
fn unslop_audit_and_headless_gpu_render_dark_and_light_screens() {
    let mut state = ChromeState::new();
    state.new_tab("https://en.wikipedia.org/wiki/Rust_(programming_language)");
    state.active_mut().is_loading = true;
    state.active_mut().load_progress = 0.6;
    state.omnibox.focus_and_select_all();

    let mut hud = DevHudState::new();
    let base = Instant::now();
    for i in 0..24 {
        hud.record_frame_at(
            FrameStageSample {
                style_ms: 0.4,
                layout_ms: 1.2,
                paint_ms: 0.5,
                ipc_ms: 0.2,
                gpu_ms: 1.8,
                total_ms: if i == 10 { 18.4 } else { 4.1 },
            },
            base + Duration::from_millis(i * 16),
        );
    }
    hud.record_missing_feature("CSS Property", "backdrop-filter");
    hud.record_network(NetworkEntry {
        url: "https://en.wikipedia.org/wiki/Rust".to_string(),
        method: "GET".to_string(),
        protocol: "H2".to_string(),
        status: 200,
        mime: "text/html".to_string(),
        bytes: 48_200,
        cache_hit: false,
        ttfb_ms: 24.5,
        total_ms: 42.1,
    });

    let mut chrome_renderer = ChromeRenderer::new();
    let viewport = Size::new(1280.0, 800.0);

    for theme in [ThemeMode::Dark, ThemeMode::Light] {
        state.set_theme(theme);
        let css = chrome_stylesheet(theme);
        let clear = palette_for(theme).clear_rgba;

        let browser_html = render_browser_chrome_html(&state);
        let newtab_html = render_newtab_html(&state);
        let bookmarks_html = render_bookmarks_html(&state);
        let history_html = render_history_html(&state);
        let error_html =
            render_error_html(theme, "https://example.invalid", "DNS resolution failed");
        let crashed_html =
            render_crashed_html(theme, "https://example.com", Some(4242), "SIGKILL received");
        let hud_html = render_hud_html(&state, &hud, base + Duration::from_millis(400));
        let overlay_html = render_overlay_html(&state, &hud, base + Duration::from_millis(400));

        // Verify 16px SVG stroke icons, keycaps, and focus rings are present.
        assert!(browser_html.contains(r#"<svg width="16" height="16""#));
        assert!(browser_html.contains(r#"class="keycap">Ctrl+L"#));
        assert!(browser_html.contains(r#"class="omnibox focused""#));

        // Run strict anti-slop audit on every Chrome screen and every Settings section.
        assert_zero_unslop_violations("browser.html", &browser_html, &css);
        assert_zero_unslop_violations("newtab.html", &newtab_html, &css);
        assert_zero_unslop_violations("bookmarks.html", &bookmarks_html, &css);
        assert_zero_unslop_violations("history.html", &history_html, &css);
        assert_zero_unslop_violations("error.html", &error_html, &css);
        assert_zero_unslop_violations("crashed.html", &crashed_html, &css);
        assert_zero_unslop_violations("hud.html", &hud_html, &css);
        assert_zero_unslop_violations("overlay", &overlay_html, &css);

        for section in SettingsSection::ALL {
            state.settings_section = section;
            let settings_html = render_settings_html(&state);
            assert_zero_unslop_violations(
                &format!("settings.html ({})", section.id()),
                &settings_html,
                &css,
            );
        }
        state.settings_section = SettingsSection::Appearance;

        // Render every screen headlessly on the GPU via Vello and confirm non-empty raster output.
        let mut resources = ResourceRegistry::default();
        let p = PipelineId(10);

        let chrome_layer =
            chrome_renderer.render_chrome(&state, viewport, shared_fonts(), p, &mut resources);
        let chrome_px = render_layer_rgba(
            &chrome_layer.display_list,
            p,
            &mut resources,
            1280,
            128,
            clear,
        );
        assert!(chrome_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        // Hit-testing finds the omnibox in the toolbar region (y ~ 62, x ~ 500).
        let hit = chrome_layer.hit_test_target(Point::new(500.0, 62.0));
        assert_eq!(hit, Some(ChromeHitTarget::Omnibox));

        // Hit-testing finds left-side nav buttons now that nested flex sizing sums accurately!
        assert_eq!(
            chrome_layer.hit_test_target(Point::new(24.0, 62.0)),
            Some(ChromeHitTarget::Back)
        );
        assert_eq!(
            chrome_layer.hit_test_target(Point::new(60.0, 62.0)),
            Some(ChromeHitTarget::Forward)
        );
        assert_eq!(
            chrome_layer.hit_test_target(Point::new(96.0, 62.0)),
            Some(ChromeHitTarget::Reload)
        );
        assert_eq!(
            chrome_layer.hit_test_target(Point::new(132.0, 62.0)),
            Some(ChromeHitTarget::Home)
        );

        let newtab_layer =
            chrome_renderer.render_newtab(&state, viewport, shared_fonts(), p, &mut resources);
        let newtab_px = render_layer_rgba(
            &newtab_layer.display_list,
            p,
            &mut resources,
            960,
            540,
            clear,
        );
        assert!(newtab_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        let settings_layer =
            chrome_renderer.render_settings(&state, viewport, shared_fonts(), p, &mut resources);
        let settings_px = render_layer_rgba(
            &settings_layer.display_list,
            p,
            &mut resources,
            1280,
            720,
            clear,
        );
        assert!(settings_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        let bookmarks_layer =
            chrome_renderer.render_bookmarks(&state, viewport, shared_fonts(), p, &mut resources);
        let bookmarks_px = render_layer_rgba(
            &bookmarks_layer.display_list,
            p,
            &mut resources,
            960,
            540,
            clear,
        );
        assert!(bookmarks_px
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| *px != clear));

        let history_layer =
            chrome_renderer.render_history(&state, viewport, shared_fonts(), p, &mut resources);
        let history_px = render_layer_rgba(
            &history_layer.display_list,
            p,
            &mut resources,
            960,
            540,
            clear,
        );
        assert!(history_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        let crashed_layer = chrome_renderer.render_crashed(
            theme,
            "https://example.com",
            Some(4242),
            "SIGKILL received",
            viewport,
            shared_fonts(),
            p,
            &mut resources,
        );
        let crashed_px = render_layer_rgba(
            &crashed_layer.display_list,
            p,
            &mut resources,
            960,
            420,
            clear,
        );
        assert!(crashed_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        let hud_layer = chrome_renderer.render_hud_standalone(
            &state,
            &hud,
            base + Duration::from_millis(400),
            Size::new(480.0, 480.0),
            shared_fonts(),
            p,
            &mut resources,
        );
        let hud_px = render_layer_rgba(&hud_layer.display_list, p, &mut resources, 480, 480, clear);
        assert!(hud_px.as_chunks::<4>().0.iter().any(|px| *px != clear));
    }
}

#[test]
fn chrome_asset_provider_integrates_with_resource_manager() {
    let provider = chrome_asset_provider();
    for path in [
        "tokens.css",
        "tokens-light.css",
        "browser.html",
        "newtab.html",
        "settings.html",
        "bookmarks.html",
        "history.html",
        "error.html",
        "crashed.html",
        "hud.html",
    ] {
        let (bytes, mime) = provider(path).unwrap_or_else(|| panic!("missing chrome asset {path}"));
        assert!(!bytes.is_empty());
        assert!(mime.starts_with("text/"));
    }

    let client = HttpClient::new(None).expect("http client");
    let _rm = ResourceManager::new(client).with_chrome_asset_provider(provider);
}

#[test]
fn settings_persistence_bookmarks_find_and_hit_ids_work() {
    let mut settings = BrowserSettings {
        theme: ThemeMode::Light,
        tab_layout: TabLayoutMode::Vertical,
        show_bookmarks_bar: false,
        compact_toolbar: true,
        search_engine: SearchEngine::Kagi,
        cookie_policy: CookiePolicy::BlockAll,
        jit_mode: JitMode::BaselineOnly,
        ..BrowserSettings::default()
    };
    settings.step_zoom(2);
    assert_eq!(settings.default_zoom_pct, 125);
    settings.step_font_size(2);
    assert_eq!(settings.base_font_size_px, 16);

    let tmp_path = std::env::temp_dir().join(format!(
        "rix_chrome_settings_test_{}.json",
        std::process::id()
    ));
    settings
        .save_to_file(&tmp_path)
        .expect("settings save succeeds");
    let reloaded = BrowserSettings::load_from_file(&tmp_path).expect("settings load succeeds");
    let _ = std::fs::remove_file(&tmp_path);
    assert_eq!(settings, reloaded);

    // Search engine & internal shorthand URL resolution.
    assert_eq!(resolve_omnibox_input("rix://settings"), URL_SETTINGS);
    assert_eq!(resolve_omnibox_input("rix://bookmarks"), URL_BOOKMARKS);
    assert_eq!(resolve_omnibox_input("rix://history"), URL_HISTORY);
    assert_eq!(resolve_omnibox_input("rix://newtab"), URL_NEWTAB);
    assert!(
        resolve_omnibox_input_with_settings("rust cranelift", &settings)
            .starts_with("https://kagi.com/search?q=")
    );

    // Verify all new ChromeHitTarget ID mappings.
    assert_eq!(
        parse_chrome_hit_id("settings-sec-privacy"),
        Some(ChromeHitTarget::SettingsSelectSection(
            SettingsSection::Privacy
        ))
    );
    assert_eq!(
        parse_chrome_hit_id("set-engine-google"),
        Some(ChromeHitTarget::SettingsSetSearchEngine(
            SearchEngine::Google
        ))
    );
    assert_eq!(
        parse_chrome_hit_id("set-cookie-block-all"),
        Some(ChromeHitTarget::SettingsSetCookiePolicy(
            CookiePolicy::BlockAll
        ))
    );
    assert_eq!(
        parse_chrome_hit_id("set-jit-baseline"),
        Some(ChromeHitTarget::SettingsSetJitMode(JitMode::BaselineOnly))
    );
    assert_eq!(
        parse_chrome_hit_id("btn-bookmark-star"),
        Some(ChromeHitTarget::ToggleBookmarkStar)
    );
    assert_eq!(
        parse_chrome_hit_id("bm-del-2"),
        Some(ChromeHitTarget::DeleteBookmark(2))
    );
}

#[test]
fn chrome_state_tabs_omnibox_editing_autocomplete_and_hud_idle() {
    let mut state = ChromeState::new();
    assert_eq!(state.tabs.len(), 1);
    assert_eq!(state.active().security_state(), SecurityState::Internal);

    // Smart URL vs search resolution.
    assert_eq!(resolve_omnibox_input("example.com"), "https://example.com");
    assert_eq!(
        resolve_omnibox_input("localhost:8080/test"),
        "http://localhost:8080/test"
    );
    assert!(resolve_omnibox_input("rust borrow checker").starts_with("https://duckduckgo.com/?q="));

    // Tab lifecycle: Ctrl+T, navigate, Ctrl+W, Ctrl+Shift+T, Ctrl+Tab, sidebar toggle.
    let t2 = state.new_tab("https://doc.rust-lang.org/std/");
    assert_eq!(state.tabs.len(), 2);
    assert_eq!(state.active().id, t2);
    assert_eq!(state.active().security_state(), SecurityState::SecureHttps);
    assert!(state.is_bookmarked("https://doc.rust-lang.org/std/"));

    state.navigate_active("http://localhost:3000/form");
    assert_eq!(state.active().security_state(), SecurityState::InsecureHttp);
    assert!(state.toggle_bookmark_active());
    assert!(state.is_bookmarked("http://localhost:3000/form"));
    assert!(!state.toggle_bookmark_active());
    assert!(!state.is_bookmarked("http://localhost:3000/form"));

    assert!(state.active().can_go_back());
    assert_eq!(
        state.go_back().as_deref(),
        Some("https://doc.rust-lang.org/std/")
    );
    assert_eq!(
        state.go_forward().as_deref(),
        Some("http://localhost:3000/form")
    );

    assert!(state.cycle_tab(false));
    assert_eq!(state.active_tab, 0);

    state.close_tab(1);
    assert_eq!(state.tabs.len(), 1);
    assert!(state.reopen_closed_tab().is_some());
    assert_eq!(state.tabs.len(), 2);

    state.toggle_tab_layout();
    assert_eq!(state.tab_layout, TabLayoutMode::Vertical);
    assert!(state.sidebar_width() >= 240.0);

    // Omnibox editing + selection + autocomplete.
    state.omnibox.focus_and_select_all();
    state.omnibox.insert_str("wiki");
    assert_eq!(state.omnibox.text, "wiki");
    let suggestions = state.autocomplete_suggestions(5);
    assert!(suggestions.iter().any(|s| s.url.contains("wikipedia.org")));
    state.omnibox.select_all();
    assert_eq!(state.omnibox.cut_selection().as_deref(), Some("wiki"));
    assert_eq!(state.omnibox.text, "");

    // DevHudState rolling 120-frame cap, IDLE 0 Hz detection, and missing-feature dedup.
    let mut hud = DevHudState::new();
    let t0 = Instant::now();
    assert!(hud.is_idle(t0));
    assert_eq!(hud.current_fps(t0), 0);

    for i in 0..150 {
        hud.record_frame_at(
            FrameStageSample {
                total_ms: 5.0,
                ..FrameStageSample::default()
            },
            t0 + Duration::from_millis(i * 10),
        );
    }
    assert_eq!(hud.samples.len(), 120);
    assert!(!hud.is_idle(t0 + Duration::from_millis(1500)));
    assert!(hud.current_fps(t0 + Duration::from_millis(1500)) > 0);
    assert!(hud.is_idle(t0 + Duration::from_millis(2100)));
    assert_eq!(hud.current_fps(t0 + Duration::from_millis(2100)), 0);

    hud.record_missing_feature("CSS Property", "mask-image");
    hud.record_missing_feature("CSS Property", "mask-image");
    hud.record_missing_feature("Web API", "ResizeObserver");
    assert_eq!(hud.missing_features[0].name, "mask-image");
    assert_eq!(hud.missing_features[0].count, 2);
    hud.active_tab = HudTab::MissingFeatures;
    let html = render_hud_html(&state, &hud, t0 + Duration::from_millis(2100));
    assert!(html.contains("IDLE 0 Hz"));
    assert!(html.contains("mask-image"));
}
