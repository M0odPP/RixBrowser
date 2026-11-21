use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rix_chrome::{
    chrome_asset_provider, chrome_stylesheet, palette_for, render_browser_chrome_html,
    render_crashed_html, render_error_html, render_hud_html, render_newtab_html,
    resolve_omnibox_input, ChromeHitTarget, ChromeRenderer, ChromeState, DevHudState,
    FrameStageSample, HudTab, NetworkEntry, SecurityState, TabLayoutMode, ThemeMode,
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

/// Automated `unslop` audit across CSS tokens and rendered HTML documents.
fn assert_zero_unslop_violations(doc_name: &str, html: &str, css: &str) {
    // 1. Zero purple/indigo/pink AI gradients or gradient text.
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

    // 3. Zero hype words or exclamation marks in UI copy.
    let lower = html.to_ascii_lowercase();
    for hype in [
        "supercharge",
        "seamless",
        "unlock",
        "magic",
        "revolutionize",
        "!!",
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
    let viewport = Size::new(1280.0, 720.0);

    for theme in [ThemeMode::Dark, ThemeMode::Light] {
        state.theme = theme;
        let css = chrome_stylesheet(theme);
        let clear = palette_for(theme).clear_rgba;

        let browser_html = render_browser_chrome_html(&state);
        let newtab_html = render_newtab_html(&state);
        let error_html =
            render_error_html(theme, "https://example.invalid", "DNS resolution failed");
        let crashed_html =
            render_crashed_html(theme, "https://example.com", Some(4242), "SIGKILL received");
        let hud_html = render_hud_html(&state, &hud, base + Duration::from_millis(400));

        // Verify 16px SVG stroke icons, keycaps, and focus rings are present.
        assert!(browser_html.contains(r#"<svg width="16" height="16""#));
        assert!(browser_html.contains(r#"class="keycap">Ctrl+L"#));
        assert!(browser_html.contains(r#"class="omnibox focused""#));

        // Run strict anti-slop audit on every Chrome screen.
        assert_zero_unslop_violations("browser.html", &browser_html, &css);
        assert_zero_unslop_violations("newtab.html", &newtab_html, &css);
        assert_zero_unslop_violations("error.html", &error_html, &css);
        assert_zero_unslop_violations("crashed.html", &crashed_html, &css);
        assert_zero_unslop_violations("hud.html", &hud_html, &css);

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
            120,
            clear,
        );
        assert!(chrome_px.as_chunks::<4>().0.iter().any(|px| *px != clear));

        // Hit-testing finds the omnibox in the toolbar region (y ~ 60, x ~ 500).
        let hit = chrome_layer.hit_test_target(Point::new(500.0, 62.0));
        assert_eq!(hit, Some(ChromeHitTarget::Omnibox));

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
            Size::new(480.0, 460.0),
            shared_fonts(),
            p,
            &mut resources,
        );
        let hud_px = render_layer_rgba(&hud_layer.display_list, p, &mut resources, 480, 460, clear);
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
        "error.html",
        "crashed.html",
        "hud.html",
    ] {
        let (bytes, mime) = provider(path).unwrap_or_else(|| panic!("missing chrome asset {path}"));
        assert!(!bytes.is_empty());
        assert!(mime.starts_with("text/"));
    }

    // Verify ResourceManager accepts the provider cleanly without circular crate dependencies.
    let client = HttpClient::new(None).expect("http client");
    let _rm = ResourceManager::new(client).with_chrome_asset_provider(provider);
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

    state.navigate_active("http://localhost:3000/form");
    assert_eq!(state.active().security_state(), SecurityState::InsecureHttp);
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
    assert!(state.sidebar_width() > 200.0);

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
