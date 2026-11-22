//! Embedded `rix://chrome/*` HTML/CSS assets and [`chrome_asset_provider`] for
//! injection into `rix_net::ResourceManager` (zero circular dependencies).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use rix_net::ChromeAssetProvider;
use url::Url;

use crate::hud::{DevHudState, HudTab};
use crate::settings::{CookiePolicy, JitMode, SearchEngine, SettingsSection};
use crate::state::{
    display_host, domain_monogram, ChromeState, SecurityState, TabLayoutMode, URL_BOOKMARKS,
    URL_HISTORY, URL_NEWTAB, URL_SETTINGS,
};
use crate::tokens::{
    chrome_stylesheet, ThemeMode, ICON_ALERT, ICON_APPEARANCE, ICON_BACK, ICON_BOOKMARK,
    ICON_CHEVRON_DOWN, ICON_CHEVRON_UP, ICON_CLOSE, ICON_CPU, ICON_FORWARD, ICON_GLOBE,
    ICON_HISTORY, ICON_HOME, ICON_HUD, ICON_INFO, ICON_LOCK, ICON_MENU, ICON_PLUS, ICON_RELOAD,
    ICON_RIX_MARK, ICON_SEARCH, ICON_SETTINGS, ICON_SHIELD, ICON_SIDEBAR, ICON_STAR,
    ICON_STAR_FILLED, ICON_STOP, ICON_THEME, ICON_TRASH,
};

/// Escapes HTML special characters so dynamic URLs/titles/errors can never inject markup.
#[must_use]
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Returns a [`ChromeAssetProvider`] ready to pass into
/// `rix_net::ResourceManager::with_chrome_asset_provider`.
#[must_use]
pub fn chrome_asset_provider() -> ChromeAssetProvider {
    Arc::new(|path: &str| {
        let clean = path.trim_start_matches('/');
        match clean {
            "tokens.css" => Some((
                chrome_stylesheet(ThemeMode::Dark).into_bytes(),
                "text/css; charset=utf-8",
            )),
            "tokens-light.css" => Some((
                chrome_stylesheet(ThemeMode::Light).into_bytes(),
                "text/css; charset=utf-8",
            )),
            "browser.html" => Some((
                render_browser_chrome_html(&ChromeState::new()).into_bytes(),
                "text/html; charset=utf-8",
            )),
            "newtab.html" => Some((
                render_newtab_html(&ChromeState::new()).into_bytes(),
                "text/html; charset=utf-8",
            )),
            "settings.html" => Some((
                render_settings_html(&ChromeState::new()).into_bytes(),
                "text/html; charset=utf-8",
            )),
            "bookmarks.html" => Some((
                render_bookmarks_html(&ChromeState::new()).into_bytes(),
                "text/html; charset=utf-8",
            )),
            "history.html" => Some((
                render_history_html(&ChromeState::new()).into_bytes(),
                "text/html; charset=utf-8",
            )),
            "error.html" => Some((
                render_error_html(
                    ThemeMode::Dark,
                    "https://example.invalid",
                    "DNS lookup failed for host",
                )
                .into_bytes(),
                "text/html; charset=utf-8",
            )),
            "crashed.html" => Some((
                render_crashed_html(
                    ThemeMode::Dark,
                    "https://example.com",
                    None,
                    "Content process terminated unexpectedly",
                )
                .into_bytes(),
                "text/html; charset=utf-8",
            )),
            "hud.html" => Some((
                render_hud_html(&ChromeState::new(), &DevHudState::new(), Instant::now())
                    .into_bytes(),
                "text/html; charset=utf-8",
            )),
            _ => None,
        }
    })
}

/// Renders the parent-process Browser Chrome frame (`rix://chrome/browser.html`)
/// reflecting active tabs, toolbar, omnibox, bookmarks bar, vertical sidebar,
/// loading hairline, and focus rings.
#[must_use]
pub fn render_browser_chrome_html(state: &ChromeState) -> String {
    let active = state.active();
    let compact = state.settings.compact_toolbar;
    let mut html = String::with_capacity(6144);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>RixBrowser</title></head><body class="chrome-body">"#,
    );
    html.push_str(r#"<div id="chrome-shell" class="chrome-shell">"#);

    if state.tab_layout == TabLayoutMode::Horizontal {
        let strip_cls = if compact {
            "tab-strip compact"
        } else {
            "tab-strip"
        };
        let _ = write!(html, r#"<div id="tab-strip" class="{strip_cls}">"#);
        for (idx, tab) in state.tabs.iter().enumerate() {
            let is_active = idx == state.active_tab;
            let mut cls = String::from("tab-item");
            if is_active {
                cls.push_str(" active");
            }
            if tab.crashed_reason.is_some() {
                cls.push_str(" crashed");
            }
            if state.focused_control.as_deref() == Some(&format!("tab-{idx}")) {
                cls.push_str(" focused");
            }
            let icon = tab_icon_for(tab.crashed_reason.is_some(), &tab.url);
            let title = escape_html(&tab.title);
            let active_bar = if is_active {
                r#"<div class="tab-active-bar"></div>"#
            } else {
                ""
            };
            let _ = write!(
                html,
                r#"<div id="tab-{idx}" class="{cls}">{active_bar}<span class="tab-icon">{icon}</span><span class="tab-title">{title}</span><div id="tab-close-{idx}" class="tab-close">{ICON_CLOSE}</div></div>"#
            );
            // Quiet hairline divider between inactive adjacent tabs.
            if !is_active && idx + 1 < state.tabs.len() && idx + 1 != state.active_tab {
                html.push_str(r#"<div class="tab-divider"></div>"#);
            }
        }
        let _ = write!(
            html,
            r#"<div id="btn-new-tab" class="tab-new-btn">{ICON_PLUS}</div>"#
        );
        html.push_str("</div>");
    }

    // Navigation toolbar
    let bar_cls = if compact {
        "toolbar compact"
    } else {
        "toolbar"
    };
    let _ = write!(html, r#"<div id="toolbar" class="{bar_cls}">"#);
    html.push_str(r#"<div class="nav-group">"#);

    let back_cls = nav_button_class(state, "btn-back", active.can_go_back(), false);
    let fwd_cls = nav_button_class(state, "btn-forward", active.can_go_forward(), false);
    let reload_cls = nav_button_class(state, "btn-reload", true, false);
    let home_cls = nav_button_class(state, "btn-home", true, active.url == URL_NEWTAB);
    let reload_icon = if active.is_loading {
        ICON_STOP
    } else {
        ICON_RELOAD
    };
    let _ = write!(
        html,
        r#"<div id="btn-back" class="{back_cls}">{ICON_BACK}</div><div id="btn-forward" class="{fwd_cls}">{ICON_FORWARD}</div><div id="btn-reload" class="{reload_cls}">{reload_icon}</div><div id="btn-home" class="{home_cls}">{ICON_HOME}</div>"#
    );
    html.push_str("</div>");

    // Smart omnibox
    let omni_cls = if state.omnibox.focused {
        "omnibox focused"
    } else {
        "omnibox"
    };
    let sec = active.security_state();
    let sec_icon = match sec {
        SecurityState::SecureHttps => ICON_LOCK,
        SecurityState::InsecureHttp => ICON_GLOBE,
        SecurityState::Internal => ICON_SHIELD,
    };
    let sec_cls = sec.css_class();
    let sec_label = sec.badge_label();
    let omni_display = format_omnibox_display(state);
    let starred = state.is_bookmarked(&active.url);
    let star_cls = if starred {
        "icon-btn sm starred"
    } else {
        "icon-btn sm"
    };
    let star_icon = if starred { ICON_STAR_FILLED } else { ICON_STAR };
    let zoom_html = if state.settings.default_zoom_pct != 100 {
        format!(
            r#"<div id="btn-zoom-reset" class="zoom-pill">{}%</div>"#,
            state.settings.default_zoom_pct
        )
    } else {
        String::new()
    };

    let _ = write!(
        html,
        r#"<div id="omnibox" class="{omni_cls}"><span class="{sec_cls}">{sec_icon}<span>{sec_label}</span></span>{omni_display}<div class="omnibox-actions">{zoom_html}<span class="keycap">Ctrl+L</span><div id="btn-bookmark-star" class="{star_cls}">{star_icon}</div></div>"#
    );
    if active.is_loading && active.load_progress > 0.01 {
        let pct = (active.load_progress.clamp(0.05, 1.0) * 100.0).round() as u32;
        let _ = write!(
            html,
            r#"<div class="load-progress" style="width: {pct}%;"></div>"#
        );
    }
    html.push_str("</div>");

    // Right-hand precision instrument controls
    let sidebar_cls = nav_button_class(
        state,
        "btn-sidebar",
        true,
        state.tab_layout == TabLayoutMode::Vertical,
    );
    let theme_cls = nav_button_class(state, "btn-theme", true, false);
    let hud_cls = nav_button_class(state, "btn-hud", true, state.hud_open);
    let settings_cls = nav_button_class(state, "btn-settings", true, active.url == URL_SETTINGS);
    let menu_cls = nav_button_class(state, "btn-menu", true, state.app_menu_open);
    let _ = write!(
        html,
        r#"<div class="nav-group"><div id="btn-sidebar" class="{sidebar_cls}">{ICON_SIDEBAR}</div><div id="btn-theme" class="{theme_cls}">{ICON_THEME}</div><div id="btn-hud" class="{hud_cls}">{ICON_HUD}</div><div class="toolbar-sep"></div><div id="btn-settings" class="{settings_cls}">{ICON_SETTINGS}</div><div id="btn-menu" class="{menu_cls}">{ICON_MENU}</div></div>"#
    );
    html.push_str("</div>");

    // Optional Bookmarks Bar
    if state.settings.show_bookmarks_bar {
        html.push_str(r#"<div id="bookmarks-bar" class="bookmarks-bar">"#);
        for (idx, bm) in state.bookmarks.iter().take(6).enumerate() {
            let label = escape_html(&bm.short_label);
            let mono = escape_html(&domain_monogram(&bm.url, &bm.short_label));
            let active_cls = if active.url == bm.url {
                "bm-pill active"
            } else {
                "bm-pill"
            };
            let _ = write!(
                html,
                r#"<div id="bm-bar-{idx}" class="{active_cls}"><span class="bm-dot">{mono}</span><span>{label}</span></div>"#
            );
        }
        html.push_str(r#"<div class="bm-spacer"></div>"#);
        let bm_cls = if active.url == URL_BOOKMARKS {
            "bm-pill active"
        } else {
            "bm-pill"
        };
        let hist_cls = if active.url == URL_HISTORY {
            "bm-pill active"
        } else {
            "bm-pill"
        };
        let _ = write!(
            html,
            r#"<div id="bm-open-bookmarks" class="{bm_cls}">{ICON_BOOKMARK}<span>Bookmarks</span></div><div id="bm-open-history" class="{hist_cls}">{ICON_HISTORY}<span>History</span></div>"#
        );
        html.push_str("</div>");
    }

    html.push_str("</div>");

    // Optional Vertical Sidebar Tab Rail
    if state.tab_layout == TabLayoutMode::Vertical {
        html.push_str(r#"<div id="sidebar-tabs" class="sidebar-tabs">"#);
        let _ = write!(
            html,
            r#"<div class="sidebar-header"><span>Open tabs</span><span class="sidebar-count">{}</span></div>"#,
            state.tabs.len()
        );
        for (idx, tab) in state.tabs.iter().enumerate() {
            let is_active = idx == state.active_tab;
            let cls = if is_active {
                "sidebar-tab-row active"
            } else {
                "sidebar-tab-row"
            };
            let bar = if is_active {
                r#"<div class="sidebar-active-bar"></div>"#
            } else {
                ""
            };
            let icon = tab_icon_for(tab.crashed_reason.is_some(), &tab.url);
            let title = escape_html(&tab.title);
            let _ = write!(
                html,
                r#"<div id="tab-{idx}" class="{cls}">{bar}<span class="tab-icon">{icon}</span><span class="tab-title">{title}</span><div id="tab-close-{idx}" class="tab-close">{ICON_CLOSE}</div></div>"#
            );
        }
        let _ = write!(
            html,
            r#"<div id="btn-new-tab" class="sidebar-new-tab">{ICON_PLUS}<span>New tab</span></div>"#
        );
        let _ = write!(
            html,
            r#"<div class="sidebar-footer"><div id="nav-open-bookmarks" class="sidebar-tab-row">{ICON_BOOKMARK}<span>Bookmarks</span></div><div id="nav-open-history" class="sidebar-tab-row">{ICON_HISTORY}<span>History</span></div><div id="nav-open-settings" class="sidebar-tab-row">{ICON_SETTINGS}<span>Settings</span></div></div>"#
        );
        html.push_str("</div>");
    }

    html.push_str("</body></html>");
    html
}

/// Renders the calm, composed New Tab page (`rix://chrome/newtab.html`).
#[must_use]
pub fn render_newtab_html(state: &ChromeState) -> String {
    let engine = state.settings.search_engine;
    let engine_action = engine.action_url();
    let engine_label = engine.label();
    let mut html = String::with_capacity(4096);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>New tab</title></head><body class="page-body">"#,
    );
    html.push_str(r#"<main class="newtab-wrap">"#);

    // Header row: Brand lockup + quiet workspace shortcuts
    let _ = write!(
        html,
        r#"<header class="newtab-top">
  <div class="brand-lockup">
    <div class="brand-mark">{ICON_RIX_MARK}</div>
    <div>
      <h1 class="brand-name">RixBrowser</h1>
      <p class="brand-meta">v0.2 · Vello GPU · Cranelift JIT · Site Isolation</p>
    </div>
  </div>
  <nav class="newtab-nav">
    <div id="nav-open-bookmarks" class="pill-link">{ICON_BOOKMARK}<span>Bookmarks</span></div>
    <div id="nav-open-history" class="pill-link">{ICON_HISTORY}<span>History</span></div>
    <div id="nav-open-settings" class="pill-link">{ICON_SETTINGS}<span>Settings</span></div>
  </nav>
</header>"#
    );

    // Unified search / address bar
    let _ = write!(
        html,
        r#"<form action="{engine_action}" method="get"><div class="search-shell"><span class="tab-icon">{ICON_SEARCH}</span><input id="newtab-search" type="text" name="q" placeholder="Search with {engine_label} or enter address" class="search-input"><span class="engine-chip">{engine_label}</span><button id="newtab-go" type="submit" class="btn-primary">Search</button></div></form>"#
    );

    // Pinned shortcuts grid (4 columns)
    if !state.bookmarks.is_empty() {
        let _ = write!(
            html,
            r#"<div class="section-head"><h2 class="section-label">Pinned shortcuts</h2><span class="section-count">{} saved</span></div><div class="shortcuts-grid">"#,
            state.bookmarks.len()
        );
        for (i, bm) in state.bookmarks.iter().take(4).enumerate() {
            let title = escape_html(&bm.short_label);
            let url = escape_html(&bm.url);
            let host = escape_html(&display_host(&bm.url));
            let mono = escape_html(&domain_monogram(&bm.url, &bm.short_label));
            let _ = write!(
                html,
                r#"<a id="bookmark-{i}" href="{url}" class="shortcut-tile"><div class="shortcut-top"><div class="monogram">{mono}</div><span class="tab-icon">{ICON_LOCK}</span></div><p class="shortcut-title">{title}</p><p class="shortcut-host">{host}</p></a>"#
            );
        }
        html.push_str("</div>");
    }

    // Recent browsing history list
    if !state.history.is_empty() {
        let _ = write!(
            html,
            r#"<div class="section-head"><h2 class="section-label">Recent pages</h2><span class="section-count">{} entries</span></div><div class="list-card">"#,
            state.history.len()
        );
        let items: Vec<_> = state.history.iter().take(5).collect();
        let total = items.len();
        for (i, item) in items.into_iter().enumerate() {
            let row_cls = if i + 1 == total {
                "list-row last"
            } else {
                "list-row"
            };
            let title = escape_html(&item.title);
            let url = escape_html(&item.url);
            let mono = escape_html(&domain_monogram(&item.url, &item.title));
            let visits_label = if item.visits == 1 {
                "1 visit".to_string()
            } else {
                format!("{} visits", item.visits)
            };
            let _ = write!(
                html,
                r#"<a id="history-{i}" href="{url}" class="{row_cls}"><div class="list-main"><div class="monogram">{mono}</div><div class="list-text"><p class="list-title">{title}</p><p class="list-url">{url}</p></div></div><div class="list-right"><span class="pill-badge">{visits_label}</span></div></a>"#
            );
        }
        html.push_str("</div>");
    }

    // Single-row non-wrapping keyboard shortcut hints
    html.push_str(
        r#"<footer class="hint-bar">
  <span class="hint-item"><span class="keycap">Ctrl+L</span><span>Address bar</span></span>
  <span class="hint-item"><span class="keycap">Ctrl+T</span><span>New tab</span></span>
  <span class="hint-item"><span class="keycap">Ctrl+F</span><span>Find in page</span></span>
  <span class="hint-item"><span class="keycap">Ctrl+,</span><span>Settings</span></span>
  <span class="hint-item"><span class="keycap">F12</span><span>Developer HUD</span></span>
</footer>"#,
    );

    html.push_str("</main></body></html>");
    html
}

/// Renders the full RixBrowser Preferences workspace (`rix://chrome/settings.html`).
#[must_use]
pub fn render_settings_html(state: &ChromeState) -> String {
    let sec = state.settings_section;
    let s = &state.settings;
    let mut html = String::with_capacity(8192);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>Settings</title></head><body class="page-body"><div class="workspace">"#,
    );

    // Left Navigation Rail
    html.push_str(r#"<aside class="ws-sidebar">"#);
    let _ = write!(
        html,
        r#"<div class="ws-brand"><div class="brand-mark">{ICON_RIX_MARK}</div><span class="ws-brand-title">Settings</span></div>"#
    );
    for item in SettingsSection::ALL {
        let active = item == sec;
        let cls = if active {
            "ws-nav-item active"
        } else {
            "ws-nav-item"
        };
        let bar = if active {
            r#"<div class="ws-nav-bar"></div>"#
        } else {
            ""
        };
        let icon = match item {
            SettingsSection::Appearance => ICON_APPEARANCE,
            SettingsSection::Search => ICON_SEARCH,
            SettingsSection::Privacy => ICON_SHIELD,
            SettingsSection::Performance => ICON_CPU,
            SettingsSection::About => ICON_INFO,
        };
        let _ = write!(
            html,
            r#"<div id="settings-sec-{}" class="{cls}">{bar}{icon}<span>{}</span></div>"#,
            item.id(),
            item.label()
        );
    }
    html.push_str(r#"<div class="ws-nav-divider"></div>"#);
    let _ = write!(
        html,
        r#"<div id="nav-open-bookmarks" class="ws-nav-item">{ICON_BOOKMARK}<span>Bookmarks</span></div>
<div id="nav-open-history" class="ws-nav-item">{ICON_HISTORY}<span>History</span></div>
<div id="nav-open-newtab" class="ws-nav-item">{ICON_HOME}<span>New tab</span></div>
<div class="ws-sidebar-foot">RixBrowser 0.2.0-alpha.1</div>"#
    );
    html.push_str("</aside>");

    // Right Content Pane
    let _ = write!(
        html,
        r#"<main class="ws-main"><div class="ws-inner"><header class="ws-header"><h1 class="ws-title">{}</h1><p class="ws-sub">{}</p></header>"#,
        sec.label(),
        sec.subtitle()
    );

    match sec {
        SettingsSection::Appearance => {
            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Theme and layout</div>"#);
            let dark_cls = seg_class(s.theme == ThemeMode::Dark);
            let light_cls = seg_class(s.theme == ThemeMode::Light);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Color theme</p><p class="setting-desc">Switch between graphite dark and warm paper-stone light chrome.</p></div><div class="setting-ctrl"><div class="seg-group"><div id="set-theme-dark" class="{dark_cls}">Dark</div><div id="set-theme-light" class="{light_cls}">Light</div></div></div></div>"#
            );

            let horiz_cls = seg_class(s.tab_layout == TabLayoutMode::Horizontal);
            let vert_cls = seg_class(s.tab_layout == TabLayoutMode::Vertical);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Tab strip orientation</p><p class="setting-desc">Keep tabs across the top bar or dock them in a left sidebar rail.</p></div><div class="setting-ctrl"><div class="seg-group"><div id="set-tabs-horizontal" class="{horiz_cls}">Horizontal</div><div id="set-tabs-vertical" class="{vert_cls}">Vertical</div></div></div></div>"#
            );

            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Show bookmarks bar</p><p class="setting-desc">Display pinned bookmarks and library links directly below the address bar.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-bookmarks-bar", s.show_bookmarks_bar)
            );
            let _ = write!(
                html,
                r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Compact toolbar</p><p class="setting-desc">Reduce tab strip and navigation bar height by 4px for more page area.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-compact", s.compact_toolbar)
            );
            html.push_str("</section>");

            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Zoom and typography</div>"#);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Default page zoom</p><p class="setting-desc">Base scaling applied to pages and internal tools (75% to 150%).</p></div><div class="setting-ctrl"><div class="stepper"><div id="set-zoom-out" class="step-btn">-</div><span class="step-val">{}%</span><div id="set-zoom-in" class="step-btn">+</div></div></div></div>"#,
                s.default_zoom_pct
            );
            let _ = write!(
                html,
                r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Base UI font size</p><p class="setting-desc">Default readable text size for browser pages (13px to 18px).</p></div><div class="setting-ctrl"><div class="stepper"><div id="set-font-dec" class="step-btn">-</div><span class="step-val">{}px</span><div id="set-font-inc" class="step-btn">+</div></div></div></div>"#,
                s.base_font_size_px
            );
            html.push_str("</section>");
        }
        SettingsSection::Search => {
            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Search provider</div>"#);
            html.push_str(r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Default search engine</p><p class="setting-desc">Used when submitting search terms from the address bar or New Tab.</p></div><div class="setting-ctrl"><div class="seg-group">"#);
            for engine in SearchEngine::ALL {
                let cls = seg_class(s.search_engine == engine);
                let _ = write!(
                    html,
                    r#"<div id="set-engine-{}" class="{cls}">{}</div>"#,
                    engine.id(),
                    engine.label()
                );
            }
            html.push_str("</div></div></div></section>");

            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Address bar</div>"#);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Autocomplete suggestions</p><p class="setting-desc">Suggest matching local history and built-in pages while typing.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-autocomplete", s.autocomplete_enabled)
            );
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Always show full URL</p><p class="setting-desc">Display the protocol scheme alongside the highlighted origin host.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-full-url", s.show_full_url)
            );
            let home = escape_html(&s.home_url);
            let _ = write!(
                html,
                r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Home page</p><p class="setting-desc">Destination opened when clicking the Home button in the toolbar.</p></div><div class="setting-ctrl"><span class="pill-badge">{home}</span></div></div>"#
            );
            html.push_str("</section>");
        }
        SettingsSection::Privacy => {
            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Transport and tracking protection</div>"#);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">HTTPS-Only mode</p><p class="setting-desc">Prefer encrypted TLS connections for bare domain navigation.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-https-only", s.https_only)
            );
            html.push_str(r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Cookie policy</p><p class="setting-desc">Control cross-site cookie storage across isolated site origins.</p></div><div class="setting-ctrl"><div class="seg-group">"#);
            for policy in CookiePolicy::ALL {
                let cls = seg_class(s.cookie_policy == policy);
                let _ = write!(
                    html,
                    r#"<div id="set-cookie-{}" class="{cls}">{}</div>"#,
                    policy.id(),
                    policy.label()
                );
            }
            html.push_str("</div></div></div>");
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Send Do Not Track header</p><p class="setting-desc">Include DNT: 1 with outgoing HTTP requests.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-dnt", s.send_dnt)
            );
            html.push_str(r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Strict Site Isolation</p><p class="setting-desc">Each site origin runs in a dedicated sandboxed Content Process with OS-enforced boundaries.</p></div><div class="setting-ctrl"><span class="pill-badge positive">Enforced</span></div></div></section>"#);

            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">Local data</div>"#);
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">Browsing history</p><p class="setting-desc">{} history entries · {} saved bookmarks · {} recently closed tabs.</p></div><div class="setting-ctrl"><div id="set-clear-history" class="btn-danger">{ICON_TRASH}<span>Clear history</span></div></div></div>"#,
                state.history.len(),
                state.bookmarks.len(),
                state.closed_tabs.len()
            );
            html.push_str(r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Restore default settings</p><p class="setting-desc">Reset theme, layout, search, privacy, and performance options to defaults.</p></div><div class="setting-ctrl"><div id="set-reset-defaults" class="btn-secondary">Reset defaults</div></div></div></section>"#);
        }
        SettingsSection::Performance => {
            html.push_str(r#"<section class="settings-group"><div class="settings-group-head">JavaScript and compositing</div>"#);
            html.push_str(r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">JavaScript execution engine</p><p class="setting-desc">Select Cranelift JIT compilation tier for content process execution.</p></div><div class="setting-ctrl"><div class="seg-group">"#);
            for mode in JitMode::ALL {
                let cls = seg_class(s.jit_mode == mode);
                let _ = write!(
                    html,
                    r#"<div id="set-jit-{}" class="{cls}">{}</div>"#,
                    mode.id(),
                    mode.label()
                );
            }
            html.push_str("</div></div></div>");
            let _ = write!(
                html,
                r#"<div class="setting-row"><div class="setting-info"><p class="setting-title">GPU Vello compositing</p><p class="setting-desc">Accelerate 2D vectors, text glyphs, Canvas2D, and YUV video overlays on wgpu.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-gpu", s.hardware_compositing)
            );
            let _ = write!(
                html,
                r#"<div class="setting-row last"><div class="setting-info"><p class="setting-title">Open Developer HUD on startup</p><p class="setting-desc">Show live frame timing, RSS memory, and video telemetry automatically.</p></div><div class="setting-ctrl">{}</div></div>"#,
                render_toggle("set-toggle-hud-startup", s.open_hud_on_startup)
            );
            html.push_str("</section>");

            html.push_str(
                r#"<section class="settings-group"><div class="settings-group-head">Pure-Rust media and graphics stack</div>
<div class="spec-grid">
  <div class="spec-tile"><p class="spec-label">Compositor</p><p class="spec-val">Vello 0.10 · wgpu · APZ</p></div>
  <div class="spec-tile"><p class="spec-label">Video decoders</p><p class="spec-val">AV1 · H.264 · VP9 · VP8</p></div>
  <div class="spec-tile"><p class="spec-label">Audio &amp; DSP</p><p class="spec-val">Opus · AAC · Vorbis · WebAudio</p></div>
  <div class="spec-tile"><p class="spec-label">Streaming</p><p class="spec-val">MSE fMP4/WebM · Zero-copy YUV</p></div>
</div>
<div class="setting-row last"><div class="setting-info"><p class="setting-title">Live engine diagnostics</p><p class="setting-desc">Inspect per-frame Style, Layout, Paint, IPC, GPU, and A/V sync metrics.</p></div><div class="setting-ctrl"><div id="btn-hud" class="btn-secondary">Toggle Developer HUD</div></div></div>
</section>"#,
            );
        }
        SettingsSection::About => {
            html.push_str(
                r#"<section class="settings-group"><div class="settings-group-head">Engine build</div>
<div class="spec-grid">
  <div class="spec-tile"><p class="spec-label">Version</p><p class="spec-val">0.2.0-alpha.1</p></div>
  <div class="spec-tile"><p class="spec-label">Architecture</p><p class="spec-val">Multi-process · Site Isolated</p></div>
  <div class="spec-tile"><p class="spec-label">JavaScript &amp; Wasm</p><p class="spec-val">rix-js · Cranelift · Wasmtime</p></div>
  <div class="spec-tile"><p class="spec-label">Safety</p><p class="spec-val">100% Safe Rust core crates</p></div>
</div></section>"#,
            );

            html.push_str(
                r#"<section class="settings-group"><div class="settings-group-head">Keyboard shortcuts</div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">Focus address bar</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+L</span></div></div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">New tab / Close tab / Reopen closed tab</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+T</span><span class="keycap">Ctrl+W</span><span class="keycap">Ctrl+Shift+T</span></div></div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">Find in page</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+F</span></div></div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">Bookmark current page</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+D</span></div></div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">Zoom in / out / reset</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+=</span><span class="keycap">Ctrl+-</span><span class="keycap">Ctrl+0</span></div></div>
<div class="setting-row shortcut-row"><div class="setting-info"><p class="setting-title">Open Settings</p></div><div class="setting-ctrl"><span class="keycap">Ctrl+,</span></div></div>
<div class="setting-row shortcut-row last"><div class="setting-info"><p class="setting-title">Toggle Developer HUD</p></div><div class="setting-ctrl"><span class="keycap">F12</span></div></div>
</section>"#,
            );
        }
    }

    html.push_str("</div></main></div></body></html>");
    html
}

/// Renders the built-in Bookmarks manager (`rix://chrome/bookmarks.html`).
#[must_use]
pub fn render_bookmarks_html(state: &ChromeState) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>Bookmarks</title></head><body class="page-body"><div class="workspace">"#,
    );
    render_library_sidebar(&mut html, "bookmarks");
    let _ = write!(
        html,
        r#"<main class="ws-main"><div class="ws-inner"><header class="ws-header"><h1 class="ws-title">Bookmarks</h1><p class="ws-sub">{} saved pages pinned to your toolbar and New Tab grid.</p></header>"#,
        state.bookmarks.len()
    );
    if state.bookmarks.is_empty() {
        html.push_str(r#"<div class="settings-group"><div class="setting-row last"><div class="setting-info"><p class="setting-title">No bookmarks saved</p><p class="setting-desc">Press Ctrl+D or click the star button in the address bar to save any page.</p></div></div></div>"#);
    } else {
        html.push_str(r#"<div class="list-card">"#);
        let total = state.bookmarks.len();
        for (i, bm) in state.bookmarks.iter().enumerate() {
            let cls = if i + 1 == total {
                "list-row last"
            } else {
                "list-row"
            };
            let title = escape_html(&bm.title);
            let url = escape_html(&bm.url);
            let mono = escape_html(&domain_monogram(&bm.url, &bm.title));
            let _ = write!(
                html,
                r#"<div class="{cls}"><a id="bookmark-{i}" href="{url}" class="list-main" style="text-decoration: none;"><div class="monogram">{mono}</div><div class="list-text"><p class="list-title">{title}</p><p class="list-url">{url}</p></div></a><div class="list-right"><div id="bm-del-{i}" class="icon-btn sm">{ICON_TRASH}</div></div></div>"#
            );
        }
        html.push_str("</div>");
    }
    html.push_str("</div></main></div></body></html>");
    html
}

/// Renders the built-in History manager (`rix://chrome/history.html`).
#[must_use]
pub fn render_history_html(state: &ChromeState) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>History</title></head><body class="page-body"><div class="workspace">"#,
    );
    render_library_sidebar(&mut html, "history");
    let _ = write!(
        html,
        r#"<main class="ws-main"><div class="ws-inner"><header class="ws-header" style="display: flex; align-items: center; justify-content: space-between; gap: 16px;"><div><h1 class="ws-title">History</h1><p class="ws-sub">{} pages visited in this browser profile.</p></div><div id="set-clear-history" class="btn-danger">{ICON_TRASH}<span>Clear history</span></div></header>"#,
        state.history.len()
    );
    if state.history.is_empty() {
        html.push_str(r#"<div class="settings-group"><div class="setting-row last"><div class="setting-info"><p class="setting-title">History is empty</p><p class="setting-desc">Visited web pages will appear here and power omnibox autocomplete.</p></div></div></div>"#);
    } else {
        html.push_str(r#"<div class="list-card">"#);
        let total = state.history.len();
        for (i, item) in state.history.iter().enumerate() {
            let cls = if i + 1 == total {
                "list-row last"
            } else {
                "list-row"
            };
            let title = escape_html(&item.title);
            let url = escape_html(&item.url);
            let mono = escape_html(&domain_monogram(&item.url, &item.title));
            let visits = if item.visits == 1 {
                "1 visit".to_string()
            } else {
                format!("{} visits", item.visits)
            };
            let _ = write!(
                html,
                r#"<div class="{cls}"><a id="history-{i}" href="{url}" class="list-main" style="text-decoration: none;"><div class="monogram">{mono}</div><div class="list-text"><p class="list-title">{title}</p><p class="list-url">{url}</p></div></a><div class="list-right"><span class="pill-badge">{visits}</span><div id="hist-del-{i}" class="icon-btn sm">{ICON_TRASH}</div></div></div>"#
            );
        }
        html.push_str("</div>");
    }
    html.push_str("</div></main></div></body></html>");
    html
}

/// Renders clear network/TLS/DNS error diagnostics (`rix://chrome/error.html`).
#[must_use]
pub fn render_error_html(_theme: ThemeMode, url: &str, reason: &str) -> String {
    let safe_url = escape_html(url);
    let safe_reason = escape_html(reason);
    format!(
        r#"<!DOCTYPE html><html><head><title>Connection error</title></head><body class="page-body">
<main class="page-wrap">
  <section class="card">
    <div class="status-kicker">
      {ICON_ALERT}
      <span>Unable to reach page</span>
    </div>
    <h1 class="page-title">Connection failed</h1>
    <p class="page-sub">{safe_reason}</p>
    <div class="action-row">
      <div id="btn-retry" class="btn-primary">Try again</div>
      <div id="btn-back" class="btn-secondary">Go back</div>
    </div>
    <div class="meta-row">
      <span>Target</span>
      <span>{safe_url}</span>
    </div>
  </section>
</main>
</body></html>"#
    )
}

/// Renders isolated tab crash recovery page (`rix://chrome/crashed.html`).
#[must_use]
pub fn render_crashed_html(_theme: ThemeMode, url: &str, pid: Option<u32>, reason: &str) -> String {
    let safe_url = escape_html(url);
    let safe_reason = escape_html(reason);
    let pid_text = pid.map_or_else(|| "unknown".to_string(), |p| p.to_string());
    format!(
        r#"<!DOCTYPE html><html><head><title>Tab crashed</title></head><body class="page-body">
<main class="page-wrap">
  <section class="card">
    <div class="status-kicker">
      {ICON_ALERT}
      <span>Isolated content process stopped</span>
    </div>
    <h1 class="page-title">This tab stopped responding</h1>
    <p class="page-sub">The browser chrome and your other tabs remain unaffected. Reload this tab to start a fresh content process.</p>
    <div class="action-row">
      <div id="btn-reload-crashed" class="btn-primary">Reload tab</div>
      <div id="btn-newtab-crashed" class="btn-secondary">Open new tab</div>
    </div>
    <div class="meta-row">
      <span>Process {pid_text}</span>
      <span>{safe_reason}</span>
      <span>{safe_url}</span>
    </div>
  </section>
</main>
</body></html>"#
    )
}

/// Renders the overlay layer HTML: omnibox autocomplete dropdown, app menu popover,
/// find-in-page bar, right-click context menu, hover URL preview pill, and `F12` HUD.
#[must_use]
pub fn render_overlay_html(state: &ChromeState, hud: &DevHudState, now: Instant) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>Overlay</title></head><body class="overlay-body">"#,
    );

    let toolbar_bottom = if state.tab_layout == TabLayoutMode::Vertical {
        if state.settings.compact_toolbar {
            42
        } else {
            46
        }
    } else if state.settings.compact_toolbar {
        78
    } else {
        86
    };

    // 1. Omnibox autocomplete dropdown
    if state.omnibox.focused && state.omnibox.dropdown_open {
        let items = state.autocomplete_suggestions(6);
        if !items.is_empty() {
            let _ = write!(
                html,
                r#"<div id="autocomplete-dropdown" class="dropdown" style="top: {toolbar_bottom}px;">"#
            );
            for (idx, item) in items.iter().enumerate() {
                let cls = if state.omnibox.selected_suggestion == Some(idx) {
                    "dropdown-row selected"
                } else {
                    "dropdown-row"
                };
                let icon = if item.is_search {
                    ICON_SEARCH
                } else if item.url.starts_with("rix://") {
                    ICON_SHIELD
                } else {
                    ICON_GLOBE
                };
                let title = escape_html(&item.title);
                let url = escape_html(&item.url);
                let _ = write!(
                    html,
                    r#"<div id="suggest-{idx}" class="{cls}"><div class="dropdown-left"><span class="tab-icon">{icon}</span><span class="dropdown-title">{title}</span></div><span class="dropdown-url">{url}</span></div>"#
                );
            }
            html.push_str("</div>");
        }
    }

    // 2. App menu popover
    if state.app_menu_open {
        let zoom = state.settings.default_zoom_pct;
        let theme_label = state.theme.label();
        let tabs_label = state.tab_layout.label();
        let _ = write!(
            html,
            r#"<div id="app-menu" class="app-menu" style="top: {toolbar_bottom}px;">
  <div id="menu-new-tab" class="menu-item"><span class="menu-item-left">{ICON_PLUS}<span>New tab</span></span><span class="keycap">Ctrl+T</span></div>
  <div class="menu-sep"></div>
  <div class="menu-item">
    <span class="menu-item-left"><span>Zoom</span></span>
    <div class="stepper">
      <div id="set-zoom-out" class="step-btn">-</div>
      <span id="btn-zoom-reset" class="step-val">{zoom}%</span>
      <div id="set-zoom-in" class="step-btn">+</div>
    </div>
  </div>
  <div class="menu-sep"></div>
  <div id="menu-bookmarks" class="menu-item"><span class="menu-item-left">{ICON_BOOKMARK}<span>Bookmarks</span></span><span class="keycap">Ctrl+B</span></div>
  <div id="menu-history" class="menu-item"><span class="menu-item-left">{ICON_HISTORY}<span>History</span></span><span class="keycap">Ctrl+H</span></div>
  <div id="menu-find" class="menu-item"><span class="menu-item-left">{ICON_SEARCH}<span>Find in page</span></span><span class="keycap">Ctrl+F</span></div>
  <div class="menu-sep"></div>
  <div id="menu-theme" class="menu-item"><span class="menu-item-left">{ICON_THEME}<span>Theme</span></span><span class="pill-badge">{theme_label}</span></div>
  <div id="menu-tabs" class="menu-item"><span class="menu-item-left">{ICON_SIDEBAR}<span>Tab strip</span></span><span class="pill-badge">{tabs_label}</span></div>
  <div id="menu-hud" class="menu-item"><span class="menu-item-left">{ICON_HUD}<span>Developer HUD</span></span><span class="keycap">F12</span></div>
  <div class="menu-sep"></div>
  <div id="menu-settings" class="menu-item"><span class="menu-item-left">{ICON_SETTINGS}<span>Settings</span></span><span class="keycap">Ctrl+,</span></div>
</div>"#
        );
    }

    // 3. Find-in-page floating bar (`Ctrl+F`)
    if state.find_bar.open {
        let top = state.top_chrome_height() as i32 + 8;
        let query_html = if state.find_bar.query.is_empty() {
            r#"<span class="find-query omnibox-placeholder">Find in page</span>"#.to_string()
        } else {
            format!(
                r#"<span class="find-query">{}</span>"#,
                escape_html(&state.find_bar.query)
            )
        };
        let count_text = if state.find_bar.total_matches == 0 {
            "0 / 0".to_string()
        } else {
            format!(
                "{} / {}",
                state.find_bar.match_index + 1,
                state.find_bar.total_matches
            )
        };
        let _ = write!(
            html,
            r#"<div id="find-bar" class="find-bar" style="top: {top}px;"><span class="tab-icon">{ICON_SEARCH}</span>{query_html}<span class="find-count">{count_text}</span><div id="find-prev" class="icon-btn sm">{ICON_CHEVRON_UP}</div><div id="find-next" class="icon-btn sm">{ICON_CHEVRON_DOWN}</div><div id="find-close" class="icon-btn sm">{ICON_CLOSE}</div></div>"#
        );
    }

    // 4. Right-click context menu
    if state.context_menu.visible {
        let cx = state.context_menu.x.max(4.0) as i32;
        let cy = state.context_menu.y.max(4.0) as i32;
        let _ = write!(
            html,
            r#"<div id="context-menu" class="context-menu" style="left: {cx}px; top: {cy}px;">"#
        );
        html.push_str(r#"<div id="ctx-back" class="context-item"><span>Back</span><span class="keycap">Alt+Left</span></div>"#);
        html.push_str(r#"<div id="ctx-forward" class="context-item"><span>Forward</span><span class="keycap">Alt+Right</span></div>"#);
        html.push_str(r#"<div id="ctx-reload" class="context-item"><span>Reload</span><span class="keycap">Ctrl+R</span></div>"#);
        if state.context_menu.link_url.is_some() {
            html.push_str(r#"<div id="ctx-open-link-tab" class="context-item"><span>Open link in new tab</span></div>"#);
            html.push_str(r#"<div id="ctx-copy-link" class="context-item"><span>Copy link address</span></div>"#);
        }
        if state.context_menu.has_selection {
            html.push_str(r#"<div id="ctx-copy" class="context-item"><span>Copy</span><span class="keycap">Ctrl+C</span></div>"#);
        }
        html.push_str(r#"<div id="ctx-hud" class="context-item"><span>Toggle Developer HUD</span><span class="keycap">F12</span></div>"#);
        html.push_str("</div>");
    }

    // 5. Hover URL preview pill (bottom-left)
    if let Some(link) = &state.hovered_link {
        let safe = escape_html(link);
        let _ = write!(
            html,
            r#"<div id="hover-pill" class="hover-pill">{safe}</div>"#
        );
    }

    // 6. F12 Developer HUD docked top-right
    if state.hud_open {
        let hud_top = state.top_chrome_height() as i32 + if state.find_bar.open { 56 } else { 12 };
        let _ = write!(
            html,
            r#"<div style="position: absolute; right: 16px; top: {hud_top}px;">"#
        );
        html.push_str(&render_hud_fragment(hud, now));
        html.push_str("</div>");
    }

    html.push_str("</body></html>");
    html
}

/// Renders a standalone `rix://chrome/hud.html` document for headless testing & inspection.
#[must_use]
pub fn render_hud_html(_state: &ChromeState, hud: &DevHudState, now: Instant) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><title>Developer HUD</title></head><body class="page-body" style="padding: 16px;">{}</body></html>"#,
        render_hud_fragment(hud, now)
    )
}

fn render_hud_fragment(hud: &DevHudState, now: Instant) -> String {
    let mut out = String::with_capacity(2048);
    let idle = hud.is_idle(now);
    let fps = hud.current_fps(now);
    let latest = hud.latest_sample();
    let pid_str = hud
        .content_pid
        .map_or_else(|| "none".to_string(), |p| p.to_string());
    let status_html = if idle {
        r#"<span class="hud-idle">IDLE 0 Hz</span>"#.to_string()
    } else {
        format!(r#"<span>{fps} fps</span>"#)
    };

    let _ = write!(
        out,
        r#"<aside id="dev-hud" class="hud-panel">
  <div class="hud-header">
    <span class="hud-title">Developer HUD</span>
    {status_html}
  </div>
  <div class="hud-stats">
    <div class="hud-stat"><div class="hud-stat-label">Frame</div><div class="hud-stat-val">{total:.2} ms</div></div>
    <div class="hud-stat"><div class="hud-stat-label">Content PID</div><div class="hud-stat-val">{pid_str}</div></div>
    <div class="hud-stat"><div class="hud-stat-label">Memory</div><div class="hud-stat-val">{rss:.1} MB</div></div>
  </div>
  <div class="hud-stages">
    <span>Style {style:.2}</span>
    <span>Layout {layout:.2}</span>
    <span>Paint {paint:.2}</span>
    <span>IPC {ipc:.2}</span>
    <span>GPU {gpu:.2}</span>
    <span>Video {vfps:.1}</span>
    <span>A/V {jitter:.1} ms</span>
    <span>DL {dl_rebuilds}</span>
  </div>"#,
        total = latest.total_ms,
        rss = hud.content_rss_mb,
        style = latest.style_ms,
        layout = latest.layout_ms,
        paint = latest.paint_ms,
        ipc = latest.ipc_ms,
        gpu = latest.gpu_ms,
        vfps = hud.video_fps,
        jitter = hud.av_sync_jitter_ms,
        dl_rebuilds = hud.video_display_list_rebuilds,
    );

    // Rolling frame-time bar graph (last 64 samples, single neutral + accent highlight for spikes > 16ms)
    out.push_str(r#"<div class="hud-graph">"#);
    let start = hud.samples.len().saturating_sub(64);
    for sample in hud.samples.iter().skip(start) {
        let h = ((sample.total_ms / 24.0) * 40.0).clamp(2.0, 40.0) as u32;
        let cls = if sample.total_ms > 16.6 {
            "hud-bar accent"
        } else {
            "hud-bar"
        };
        let _ = write!(out, r#"<div class="{cls}" style="height: {h}px;"></div>"#);
    }
    out.push_str("</div>");

    // HUD sub-panel tabs
    let tab_cls = |t: HudTab| {
        if hud.active_tab == t {
            "hud-tab active"
        } else {
            "hud-tab"
        }
    };
    let _ = write!(
        out,
        r#"<div class="hud-tabs">
    <div id="hud-tab-perf" class="{p}">Performance</div>
    <div id="hud-tab-net" class="{n}">Network ({nc})</div>
    <div id="hud-tab-missing" class="{m}">Missing ({mc})</div>
    <div id="hud-tab-console" class="{c}">Console ({cc})</div>
  </div>"#,
        p = tab_cls(HudTab::Performance),
        n = tab_cls(HudTab::Network),
        nc = hud.network.len(),
        m = tab_cls(HudTab::MissingFeatures),
        mc = hud.missing_features.len(),
        c = tab_cls(HudTab::Console),
        cc = hud.console.len(),
    );

    match hud.active_tab {
        HudTab::Performance => {
            out.push_str(r#"<table class="hud-table"><tr><th>Stage</th><th>Time</th></tr>"#);
            let _ = write!(
                out,
                "<tr><td>Style cascade</td><td>{:.2} ms</td></tr>\
                 <tr><td>Layout</td><td>{:.2} ms</td></tr>\
                 <tr><td>Paint</td><td>{:.2} ms</td></tr>\
                 <tr><td>IPC transfer</td><td>{:.2} ms</td></tr>\
                 <tr><td>GPU compose</td><td>{:.2} ms</td></tr>",
                latest.style_ms, latest.layout_ms, latest.paint_ms, latest.ipc_ms, latest.gpu_ms
            );
            out.push_str("</table>");
        }
        HudTab::Network => {
            out.push_str(
                r#"<table class="hud-table"><tr><th>Status</th><th>URL</th><th>Time</th></tr>"#,
            );
            for entry in hud.network.iter().rev().take(8) {
                let short_url =
                    escape_html(entry.url.chars().take(34).collect::<String>().as_str());
                let cache_tag = if entry.cache_hit {
                    "cache"
                } else {
                    &entry.protocol
                };
                let _ = write!(
                    out,
                    "<tr><td>{} {}</td><td>{}</td><td>{:.1} ms</td></tr>",
                    entry.status, cache_tag, short_url, entry.total_ms
                );
            }
            out.push_str("</table>");
        }
        HudTab::MissingFeatures => {
            out.push_str(
                r#"<table class="hud-table"><tr><th>Kind</th><th>Feature</th><th>Count</th></tr>"#,
            );
            if hud.missing_features.is_empty() {
                out.push_str(
                    "<tr><td>None</td><td>No unsupported features recorded</td><td>0</td></tr>",
                );
            } else {
                for feat in hud.missing_features.iter().take(10) {
                    let cat = escape_html(&feat.category);
                    let name = escape_html(&feat.name);
                    let _ = write!(
                        out,
                        "<tr><td>{cat}</td><td>{name}</td><td>{}</td></tr>",
                        feat.count
                    );
                }
            }
            out.push_str("</table>");
        }
        HudTab::Console => {
            out.push_str(r#"<table class="hud-table"><tr><th>Level</th><th>Message</th></tr>"#);
            if hud.console.is_empty() {
                out.push_str("<tr><td>info</td><td>Console is empty</td></tr>");
            } else {
                for line in hud.console.iter().rev().take(8) {
                    let lvl = escape_html(&line.level);
                    let msg = escape_html(&line.message);
                    let _ = write!(out, "<tr><td>{lvl}</td><td>{msg}</td></tr>");
                }
            }
            out.push_str("</table>");
        }
    }

    out.push_str("</aside>");
    out
}

fn render_library_sidebar(html: &mut String, active_page: &str) {
    html.push_str(r#"<aside class="ws-sidebar">"#);
    let _ = write!(
        html,
        r#"<div class="ws-brand"><div class="brand-mark">{ICON_RIX_MARK}</div><span class="ws-brand-title">Library</span></div>"#
    );
    for (id, key, icon, label) in [
        (
            "nav-open-bookmarks",
            "bookmarks",
            ICON_BOOKMARK,
            "Bookmarks",
        ),
        ("nav-open-history", "history", ICON_HISTORY, "History"),
        ("nav-open-settings", "settings", ICON_SETTINGS, "Settings"),
        ("nav-open-newtab", "newtab", ICON_HOME, "New tab"),
    ] {
        let active = active_page == key;
        let cls = if active {
            "ws-nav-item active"
        } else {
            "ws-nav-item"
        };
        let bar = if active {
            r#"<div class="ws-nav-bar"></div>"#
        } else {
            ""
        };
        let _ = write!(
            html,
            r#"<div id="{id}" class="{cls}">{bar}{icon}<span>{label}</span></div>"#
        );
    }
    html.push_str(r#"<div class="ws-sidebar-foot">RixBrowser 0.2.0-alpha.1</div></aside>"#);
}

fn format_omnibox_display(state: &ChromeState) -> String {
    if state.omnibox.text.is_empty() {
        return r#"<span class="omnibox-text omnibox-placeholder">Search or enter address</span>"#
            .to_string();
    }
    if state.omnibox.focused {
        return format!(
            r#"<span class="omnibox-text">{}</span>"#,
            escape_html(&state.omnibox.text)
        );
    }
    if let Ok(parsed) = Url::parse(&state.omnibox.text) {
        if matches!(parsed.scheme(), "http" | "https") {
            if let Some(host) = parsed.host_str() {
                let scheme = if state.settings.show_full_url {
                    format!(
                        r#"<span class="url-muted">{}://</span>"#,
                        escape_html(parsed.scheme())
                    )
                } else {
                    String::new()
                };
                let host_port = match parsed.port() {
                    Some(p) => format!("{host}:{p}"),
                    None => host.to_string(),
                };
                let mut rest = parsed.path().to_string();
                if rest == "/" && parsed.query().is_none() && parsed.fragment().is_none() {
                    rest.clear();
                }
                if let Some(q) = parsed.query() {
                    rest.push('?');
                    rest.push_str(q);
                }
                if let Some(f) = parsed.fragment() {
                    rest.push('#');
                    rest.push_str(f);
                }
                return format!(
                    r#"<span class="omnibox-text">{scheme}<span class="url-host">{}</span><span class="url-muted">{}</span></span>"#,
                    escape_html(&host_port),
                    escape_html(&rest)
                );
            }
        }
    }
    format!(
        r#"<span class="omnibox-text">{}</span>"#,
        escape_html(&state.omnibox.text)
    )
}

fn tab_icon_for(crashed: bool, url: &str) -> &'static str {
    if crashed {
        return ICON_ALERT;
    }
    match url {
        URL_SETTINGS => ICON_SETTINGS,
        URL_BOOKMARKS => ICON_BOOKMARK,
        URL_HISTORY => ICON_HISTORY,
        u if u.starts_with("rix://") => ICON_SHIELD,
        u if u.starts_with("https://") => ICON_LOCK,
        _ => ICON_GLOBE,
    }
}

fn render_toggle(id: &str, on: bool) -> String {
    let cls = if on {
        "toggle-track on"
    } else {
        "toggle-track"
    };
    format!(r#"<div id="{id}" class="{cls}"><div class="toggle-thumb"></div></div>"#)
}

const fn seg_class(active: bool) -> &'static str {
    if active {
        "seg-opt active"
    } else {
        "seg-opt"
    }
}

fn nav_button_class(state: &ChromeState, id: &str, enabled: bool, active: bool) -> String {
    let mut cls = String::from("icon-btn");
    if !enabled {
        cls.push_str(" disabled");
    }
    if active {
        cls.push_str(" active");
    }
    if state.focused_control.as_deref() == Some(id) {
        cls.push_str(" focused");
    }
    cls
}
