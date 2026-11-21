//! Embedded `rix://chrome/*` HTML/CSS assets and [`chrome_asset_provider`] for
//! injection into `rix_net::ResourceManager` (zero circular dependencies).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use rix_net::ChromeAssetProvider;

use crate::hud::{DevHudState, HudTab};
use crate::state::{ChromeState, SecurityState, TabLayoutMode};
use crate::tokens::{
    chrome_stylesheet, ThemeMode, ICON_ALERT, ICON_BACK, ICON_CLOSE, ICON_FORWARD, ICON_GLOBE,
    ICON_HUD, ICON_LOCK, ICON_PLUS, ICON_RELOAD, ICON_SHIELD, ICON_SIDEBAR, ICON_STOP, ICON_THEME,
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
/// reflecting active tabs, omnibox, origin badge, loading hairline, and focus rings.
#[must_use]
pub fn render_browser_chrome_html(state: &ChromeState) -> String {
    let active = state.active();
    let mut html = String::with_capacity(4096);
    html.push_str("<!DOCTYPE html><html><head><title>RixBrowser</title></head><body>");
    html.push_str(r#"<div id="chrome-shell" class="chrome-shell">"#);

    if state.tab_layout == TabLayoutMode::Horizontal {
        html.push_str(r#"<div id="tab-strip" class="tab-strip">"#);
        for (idx, tab) in state.tabs.iter().enumerate() {
            let mut cls = String::from("tab-item");
            if idx == state.active_tab {
                cls.push_str(" active");
            }
            if tab.crashed_reason.is_some() {
                cls.push_str(" crashed");
            }
            if state.focused_control.as_deref() == Some(&format!("tab-{idx}")) {
                cls.push_str(" focused");
            }
            let title = escape_html(&tab.title);
            let _ = write!(
                html,
                r#"<div id="tab-{idx}" class="{cls}"><span class="tab-title">{title}</span><div id="tab-close-{idx}" class="icon-btn">{ICON_CLOSE}</div></div>"#
            );
        }
        let _ = write!(
            html,
            r#"<div id="btn-new-tab" class="icon-btn">{ICON_PLUS}</div>"#
        );
        html.push_str(r#"<span class="keycap">Ctrl+T</span>"#);
        html.push_str("</div>");
    }

    // Navigation toolbar
    html.push_str(r#"<div id="toolbar" class="toolbar">"#);
    html.push_str(r#"<div class="nav-group">"#);

    let back_cls = nav_button_class(state, "btn-back", active.can_go_back(), false);
    let fwd_cls = nav_button_class(state, "btn-forward", active.can_go_forward(), false);
    let reload_cls = nav_button_class(state, "btn-reload", true, false);
    let reload_icon = if active.is_loading {
        ICON_STOP
    } else {
        ICON_RELOAD
    };
    let _ = write!(
        html,
        r#"<div id="btn-back" class="{back_cls}">{ICON_BACK}</div>"#
    );
    let _ = write!(
        html,
        r#"<div id="btn-forward" class="{fwd_cls}">{ICON_FORWARD}</div>"#
    );
    let _ = write!(
        html,
        r#"<div id="btn-reload" class="{reload_cls}">{reload_icon}</div>"#
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
    let omni_display = if state.omnibox.text.is_empty() {
        r#"<span class="omnibox-text omnibox-placeholder">Search or enter address</span>"#
            .to_string()
    } else {
        format!(
            r#"<span class="omnibox-text">{}</span>"#,
            escape_html(&state.omnibox.text)
        )
    };
    let _ = write!(
        html,
        r#"<div id="omnibox" class="{omni_cls}"><span class="{sec_cls}">{sec_icon}<span>{sec_label}</span></span>{omni_display}<span class="keycap">Ctrl+L</span>"#
    );
    if active.is_loading && active.load_progress > 0.01 {
        let pct = (active.load_progress.clamp(0.05, 1.0) * 100.0).round() as u32;
        let _ = write!(
            html,
            r#"<div class="load-progress" style="width: {pct}%;"></div>"#
        );
    }
    html.push_str("</div>");

    // Right-hand quiet instrument toggles: sidebar tabs, theme, F12 HUD.
    let sidebar_cls = nav_button_class(
        state,
        "btn-sidebar",
        true,
        state.tab_layout == TabLayoutMode::Vertical,
    );
    let theme_cls = nav_button_class(state, "btn-theme", true, false);
    let hud_cls = nav_button_class(state, "btn-hud", true, state.hud_open);
    let _ = write!(
        html,
        r#"<div class="nav-group"><div id="btn-sidebar" class="{sidebar_cls}">{ICON_SIDEBAR}</div><div id="btn-theme" class="{theme_cls}">{ICON_THEME}</div><div id="btn-hud" class="{hud_cls}">{ICON_HUD}</div><span class="keycap">F12</span></div>"#
    );
    html.push_str("</div></div>");

    if state.tab_layout == TabLayoutMode::Vertical {
        html.push_str(r#"<div id="sidebar-tabs" class="sidebar-tabs">"#);
        for (idx, tab) in state.tabs.iter().enumerate() {
            let cls = if idx == state.active_tab {
                "sidebar-tab-row active"
            } else {
                "sidebar-tab-row"
            };
            let title = escape_html(&tab.title);
            let _ = write!(
                html,
                r#"<div id="tab-{idx}" class="{cls}"><span class="tab-title">{title}</span><div id="tab-close-{idx}" class="icon-btn">{ICON_CLOSE}</div></div>"#
            );
        }
        let _ = write!(
            html,
            r#"<div id="btn-new-tab" class="sidebar-tab-row">{ICON_PLUS}<span>New tab</span></div>"#
        );
        html.push_str("</div>");
    }

    html.push_str("</body></html>");
    html
}

/// Renders the calm, focused New Tab page (`rix://chrome/newtab.html`).
#[must_use]
pub fn render_newtab_html(state: &ChromeState) -> String {
    let mut html = String::with_capacity(2048);
    html.push_str("<!DOCTYPE html><html><head><title>New tab</title></head><body>");
    html.push_str(r#"<main class="page-wrap"><section class="card">"#);
    html.push_str(r#"<h1 class="page-title">RixBrowser</h1>"#);
    html.push_str(
        r#"<p class="page-sub">Multi-process browser with isolated content sandboxes and GPU Vello compositing.</p>"#,
    );
    html.push_str(
        r#"<form action="https://duckduckgo.com/" method="get"><div style="display: flex; gap: 8px; align-items: center;"><input id="newtab-search" type="text" name="q" placeholder="Search or enter address" style="flex-grow: 1; height: 36px; padding: 0 12px; border-radius: 8px;"><button id="newtab-go" type="submit" class="btn-primary">Search</button></div></form>"#,
    );

    if !state.history.is_empty() {
        html.push_str(r#"<div class="history-grid">"#);
        for (i, item) in state.history.iter().take(6).enumerate() {
            let title = escape_html(&item.title);
            let url = escape_html(&item.url);
            let _ = write!(
                html,
                r#"<a id="history-{i}" href="{url}" class="history-item" style="display: block; text-decoration: none;"><p class="history-title">{title}</p><p class="history-url">{url}</p></a>"#
            );
        }
        html.push_str("</div>");
    }

    html.push_str(
        r#"<div class="meta-row"><span class="keycap">Ctrl+L</span><span>Focus address bar</span><span class="keycap">Ctrl+T</span><span>New tab</span><span class="keycap">Ctrl+W</span><span>Close tab</span><span class="keycap">F12</span><span>Developer HUD</span></div>"#,
    );
    html.push_str("</section></main></body></html>");
    html
}

/// Renders clear network/TLS/DNS error diagnostics (`rix://chrome/error.html`).
#[must_use]
pub fn render_error_html(_theme: ThemeMode, url: &str, reason: &str) -> String {
    let safe_url = escape_html(url);
    let safe_reason = escape_html(reason);
    format!(
        r#"<!DOCTYPE html><html><head><title>Connection error</title></head><body>
<main class="page-wrap">
  <section class="card">
    <div style="display: flex; align-items: center; gap: 8px; margin-bottom: 12px; color: var(--negative);">
      {ICON_ALERT}
      <span>Unable to reach page</span>
    </div>
    <h1 class="page-title">Connection failed</h1>
    <p class="page-sub">{safe_reason}</p>
    <div style="display: flex; gap: 8px;">
      <div id="btn-retry" class="btn-primary focused">Try again</div>
      <div id="btn-back-safe" class="btn-secondary">Go back</div>
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
        r#"<!DOCTYPE html><html><head><title>Tab crashed</title></head><body>
<main class="page-wrap">
  <section class="card">
    <div style="display: flex; align-items: center; gap: 8px; margin-bottom: 12px; color: var(--negative);">
      {ICON_ALERT}
      <span>Isolated content process stopped</span>
    </div>
    <h1 class="page-title">This tab stopped responding</h1>
    <p class="page-sub">The browser chrome and your other tabs remain unaffected. Reload this tab to start a fresh content process.</p>
    <div style="display: flex; gap: 8px;">
      <div id="btn-reload-crashed" class="btn-primary focused">Reload tab</div>
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

/// Renders the overlay layer HTML: omnibox autocomplete dropdown, right-click context menu,
/// hover URL preview pill, and docked `F12` Developer HUD.
#[must_use]
pub fn render_overlay_html(state: &ChromeState, hud: &DevHudState, now: Instant) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str(
        r#"<!DOCTYPE html><html><head><title>Overlay</title></head><body style="background-color: transparent;">"#,
    );

    // 1. Omnibox autocomplete dropdown
    if state.omnibox.focused && state.omnibox.dropdown_open {
        let items = state.autocomplete_suggestions(6);
        if !items.is_empty() {
            html.push_str(r#"<div id="autocomplete-dropdown" class="dropdown">"#);
            for (idx, item) in items.iter().enumerate() {
                let cls = if state.omnibox.selected_suggestion == Some(idx) {
                    "dropdown-row selected"
                } else {
                    "dropdown-row"
                };
                let title = escape_html(&item.title);
                let url = escape_html(&item.url);
                let _ = write!(
                    html,
                    r#"<div id="suggest-{idx}" class="{cls}"><span>{title}</span><span style="font-family: monospace; font-size: 12px; color: var(--text-muted);">{url}</span></div>"#
                );
            }
            html.push_str("</div>");
        }
    }

    // 2. Right-click context menu
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

    // 3. Hover URL preview pill (bottom-left)
    if let Some(link) = &state.hovered_link {
        let safe = escape_html(link);
        let _ = write!(
            html,
            r#"<div id="hover-pill" class="hover-pill">{safe}</div>"#
        );
    }

    // 4. F12 Developer HUD docked top-right
    if state.hud_open {
        html.push_str(r#"<div style="position: absolute; right: 16px; top: 96px;">"#);
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
        r#"<!DOCTYPE html><html><head><title>Developer HUD</title></head><body style="padding: 16px;">{}</body></html>"#,
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
  </div>"#,
        total = latest.total_ms,
        rss = hud.content_rss_mb,
        style = latest.style_ms,
        layout = latest.layout_ms,
        paint = latest.paint_ms,
        ipc = latest.ipc_ms,
        gpu = latest.gpu_ms,
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
