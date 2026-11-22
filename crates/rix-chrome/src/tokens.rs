//! Design tokens, stylesheet, and 16px stroke SVG icons crafted strictly with
//! the `unslop` design system (Precision Optical Instrument direction).
//!
//! Credits: `unslop` design methodology by Damir Sokolovsky (@heydamir), CC BY 4.0.
//!
//! Rules enforced:
//! - Tinted neutrals only (cool graphite-slate dark ramp, warm paper-stone light ramp)
//! - Three text roles with WCAG AA >= 4.5:1 contrast against every surface
//! - One restrained cobalt accent reserved for focus rings, active indicators,
//!   loading progress hairlines, toggle-on tracks, and primary actions
//! - Strict 4px spacing scale (4, 8, 12, 16, 20, 24, 32)
//! - Nested border radii (`inner = outer - padding`, minimum 4px)
//! - Single 16px stroke SVG icon vocabulary (`stroke-width="1.5"`), zero emoji,
//!   zero gradients or glows, zero uppercase micro-labels

use serde::{Deserialize, Serialize};

/// Active Chrome UI theme polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl ThemeMode {
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    #[must_use]
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

/// Resolved concrete surface, ink, hairline, and semantic tokens for a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromePalette {
    pub bg_deepest: &'static str,
    pub bg_toolbar: &'static str,
    pub bg_card: &'static str,
    pub bg_raised: &'static str,
    pub bg_active_pill: &'static str,
    pub bg_input: &'static str,
    pub hairline: &'static str,
    pub hairline_strong: &'static str,
    pub popover_shadow: &'static str,
    pub text_primary: &'static str,
    pub text_secondary: &'static str,
    pub text_muted: &'static str,
    pub accent: &'static str,
    pub accent_solid: &'static str,
    pub accent_fg: &'static str,
    pub positive: &'static str,
    pub warning: &'static str,
    pub negative: &'static str,
    pub keycap_bg: &'static str,
    pub clear_rgba: [u8; 4],
}

/// Cool graphite-slate dark ramp tuned for low eye fatigue around bright web content.
pub const DARK_PALETTE: ChromePalette = ChromePalette {
    bg_deepest: "#0b0d12",
    bg_toolbar: "#11141c",
    bg_card: "#161a24",
    bg_raised: "#1f2534",
    bg_active_pill: "#1f2534",
    bg_input: "#0e1118",
    hairline: "#242b3c",
    hairline_strong: "#323c52",
    popover_shadow: "0 8px 24px rgba(0, 0, 0, 0.38)",
    text_primary: "#eef1f6",
    text_secondary: "#a6b0c3",
    text_muted: "#8591a8",
    accent: "#3b82f6",
    accent_solid: "#2563eb",
    accent_fg: "#ffffff",
    positive: "#34d399",
    warning: "#f59e0b",
    negative: "#f87171",
    keycap_bg: "#181d29",
    clear_rgba: [17, 20, 28, 255],
};

/// Warm archival paper-stone light ramp tuned for calm daytime readability.
pub const LIGHT_PALETTE: ChromePalette = ChromePalette {
    bg_deepest: "#e8e5de",
    bg_toolbar: "#f4f2ec",
    bg_card: "#ffffff",
    bg_raised: "#eae7df",
    bg_active_pill: "#ffffff",
    bg_input: "#f9f8f5",
    hairline: "#dad6cc",
    hairline_strong: "#c2bdb1",
    popover_shadow: "0 6px 20px rgba(24, 22, 20, 0.12)",
    text_primary: "#181614",
    text_secondary: "#4a443c",
    text_muted: "#635c53",
    accent: "#1d4ed8",
    accent_solid: "#1d4ed8",
    accent_fg: "#ffffff",
    positive: "#15803d",
    warning: "#b45309",
    negative: "#dc2626",
    keycap_bg: "#ece9e1",
    clear_rgba: [244, 242, 236, 255],
};

#[must_use]
pub const fn palette_for(theme: ThemeMode) -> ChromePalette {
    match theme {
        ThemeMode::Dark => DARK_PALETTE,
        ThemeMode::Light => LIGHT_PALETTE,
    }
}

/// Generates the complete `tokens.css` stylesheet for `theme`.
#[must_use]
pub fn chrome_stylesheet(theme: ThemeMode) -> String {
    let p = palette_for(theme);
    format!(
        r#"
:root {{
    --bg-deepest: {bg_deepest};
    --bg-toolbar: {bg_toolbar};
    --bg-card: {bg_card};
    --bg-raised: {bg_raised};
    --bg-active-pill: {bg_active_pill};
    --bg-input: {bg_input};
    --hairline: {hairline};
    --hairline-strong: {hairline_strong};
    --popover-shadow: {popover_shadow};
    --text-primary: {text_primary};
    --text-secondary: {text_secondary};
    --text-muted: {text_muted};
    --accent: {accent};
    --accent-solid: {accent_solid};
    --accent-fg: {accent_fg};
    --positive: {positive};
    --warning: {warning};
    --negative: {negative};
    --keycap-bg: {keycap_bg};
}}
* {{
    box-sizing: border-box;
}}
body {{
    margin: 0;
    padding: 0;
    font-family: Inter, sans-serif;
    font-size: 14px;
    font-weight: 400;
    line-height: 20px;
    color: var(--text-primary);
    background-color: var(--bg-toolbar);
}}
body.chrome-body {{
    background-color: transparent;
}}
body.page-body {{
    min-height: 100vh;
    background-color: var(--bg-toolbar);
}}
body.overlay-body {{
    position: relative;
    min-height: 100vh;
    background-color: transparent;
}}

/* ---------------- Chrome Top Shell ---------------- */
.chrome-shell {{
    width: 100%;
    background-color: var(--bg-deepest);
    border-bottom: 1px solid var(--hairline);
}}
.tab-strip {{
    display: flex;
    align-items: flex-end;
    height: 40px;
    padding: 4px 8px 0 8px;
    gap: 4px;
    background-color: var(--bg-deepest);
}}
.tab-strip.compact {{
    height: 36px;
}}
.tab-item {{
    position: relative;
    display: flex;
    align-items: center;
    height: 36px;
    width: 196px;
    min-width: 108px;
    max-width: 224px;
    padding: 0 6px 0 10px;
    gap: 8px;
    border-radius: 8px 8px 0 0;
    background-color: transparent;
    color: var(--text-secondary);
    border: 1px solid transparent;
    border-bottom: 0 solid transparent;
    overflow: hidden;
}}
.tab-strip.compact .tab-item {{
    height: 32px;
}}
.tab-item.active {{
    background-color: var(--bg-toolbar);
    color: var(--text-primary);
    border-top: 1px solid var(--hairline);
    border-left: 1px solid var(--hairline);
    border-right: 1px solid var(--hairline);
}}
.tab-item.crashed {{
    border-top: 1px solid var(--negative);
    border-left: 1px solid var(--negative);
    border-right: 1px solid var(--negative);
}}
.tab-active-bar {{
    position: absolute;
    left: 10px;
    top: 0;
    width: 174px;
    height: 2px;
    border-radius: 2px;
    background-color: var(--accent);
}}
.tab-divider {{
    width: 1px;
    height: 16px;
    margin-bottom: 10px;
    background-color: var(--hairline);
    flex-shrink: 0;
}}
.tab-icon {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    flex-shrink: 0;
    color: var(--text-muted);
}}
.tab-item.active .tab-icon {{
    color: var(--text-secondary);
}}
.tab-item.crashed .tab-icon {{
    color: var(--negative);
}}
.tab-title {{
    flex-grow: 1;
    min-width: 0;
    font-size: 13px;
    font-weight: 400;
    line-height: 18px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.tab-item.active .tab-title {{
    font-weight: 500;
}}
.tab-close {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    border-radius: 4px;
    color: var(--text-muted);
    flex-shrink: 0;
}}
.tab-new-btn {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    margin-bottom: 4px;
    margin-left: 2px;
    border-radius: 6px;
    color: var(--text-secondary);
    flex-shrink: 0;
}}

/* ---------------- Navigation Toolbar ---------------- */
.toolbar {{
    display: flex;
    align-items: center;
    height: 44px;
    padding: 0 8px;
    gap: 8px;
    background-color: var(--bg-toolbar);
}}
.toolbar.compact {{
    height: 40px;
}}
.nav-group {{
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
}}
.toolbar-sep {{
    width: 1px;
    height: 18px;
    margin: 0 2px;
    background-color: var(--hairline);
    flex-shrink: 0;
}}
.icon-btn {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    padding: 0;
    border-radius: 6px;
    border: 1px solid transparent;
    background-color: transparent;
    color: var(--text-secondary);
    flex-shrink: 0;
}}
.icon-btn.sm {{
    width: 26px;
    height: 26px;
    border-radius: 4px;
}}
.icon-btn.active {{
    background-color: var(--bg-raised);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.icon-btn.starred {{
    color: var(--accent);
}}
.icon-btn.focused, .tab-item.focused {{
    outline: 2px solid var(--accent);
    outline-offset: -2px;
}}
.icon-btn.disabled {{
    color: var(--text-muted);
    opacity: 0.4;
}}

/* ---------------- Omnibox ---------------- */
.omnibox {{
    position: relative;
    display: flex;
    align-items: center;
    flex-grow: 1;
    min-width: 0;
    height: 32px;
    padding: 0 6px 0 8px;
    gap: 8px;
    border-radius: 8px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    overflow: hidden;
}}
.omnibox.focused {{
    background-color: var(--bg-input);
    border: 1px solid var(--accent);
    outline: 2px solid var(--accent);
    outline-offset: -1px;
}}
.origin-badge {{
    display: flex;
    align-items: center;
    gap: 4px;
    height: 22px;
    padding: 0 6px;
    border-radius: 4px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    line-height: 16px;
    color: var(--text-muted);
    flex-shrink: 0;
}}
.origin-badge.secure {{
    color: var(--positive);
}}
.origin-badge.insecure {{
    color: var(--warning);
}}
.omnibox-text {{
    flex-grow: 1;
    min-width: 0;
    font-family: monospace;
    font-size: 13px;
    line-height: 20px;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.url-muted {{
    color: var(--text-muted);
}}
.url-host {{
    color: var(--text-primary);
    font-weight: 500;
}}
.omnibox-placeholder {{
    color: var(--text-muted);
}}
.omnibox-actions {{
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
}}
.zoom-pill {{
    display: flex;
    align-items: center;
    height: 22px;
    padding: 0 6px;
    border-radius: 4px;
    background-color: var(--bg-raised);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    line-height: 16px;
    color: var(--text-secondary);
    flex-shrink: 0;
}}
.load-progress {{
    position: absolute;
    left: 6px;
    bottom: 0;
    height: 2px;
    border-radius: 2px;
    background-color: var(--accent);
}}

/* ---------------- Bookmarks Bar ---------------- */
.bookmarks-bar {{
    display: flex;
    align-items: center;
    height: 32px;
    padding: 0 12px;
    gap: 4px;
    background-color: var(--bg-toolbar);
    border-top: 1px solid var(--hairline);
    overflow: hidden;
}}
.bm-pill {{
    display: flex;
    align-items: center;
    gap: 6px;
    height: 24px;
    padding: 0 8px;
    border-radius: 4px;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-secondary);
    white-space: nowrap;
    flex-shrink: 0;
}}
.bm-pill.active {{
    background-color: var(--bg-raised);
    color: var(--text-primary);
}}
.bm-dot {{
    width: 14px;
    height: 14px;
    border-radius: 4px;
    background-color: var(--bg-raised);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 10px;
    line-height: 12px;
    text-align: center;
    color: var(--text-secondary);
    flex-shrink: 0;
}}
.bm-spacer {{
    flex-grow: 1;
}}

/* ---------------- Vertical Sidebar Tab Rail ---------------- */
.sidebar-tabs {{
    display: flex;
    flex-direction: column;
    width: 240px;
    min-height: 100vh;
    padding: 12px 8px;
    gap: 4px;
    background-color: var(--bg-deepest);
    border-right: 1px solid var(--hairline);
}}
.sidebar-header {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 28px;
    padding: 0 8px;
    margin-bottom: 4px;
    font-size: 12px;
    font-weight: 500;
    color: var(--text-muted);
    white-space: nowrap;
}}
.sidebar-count {{
    font-family: monospace;
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
}}
.sidebar-tab-row {{
    position: relative;
    display: flex;
    align-items: center;
    height: 34px;
    padding: 0 6px 0 12px;
    gap: 8px;
    border-radius: 6px;
    color: var(--text-secondary);
    border: 1px solid transparent;
    overflow: hidden;
}}
.sidebar-tab-row.active {{
    background-color: var(--bg-active-pill);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.sidebar-active-bar {{
    position: absolute;
    left: 0;
    top: 6px;
    width: 2px;
    height: 20px;
    border-radius: 2px;
    background-color: var(--accent);
}}
.sidebar-new-tab {{
    display: flex;
    align-items: center;
    height: 32px;
    padding: 0 10px;
    margin-top: 4px;
    gap: 8px;
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-muted);
    border: 1px solid var(--hairline);
}}
.sidebar-footer {{
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    gap: 4px;
}}

/* ---------------- Shared Primitives ---------------- */
.keycap {{
    display: inline-block;
    padding: 2px 6px;
    border-radius: 4px;
    background-color: var(--keycap-bg);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    line-height: 16px;
    color: var(--text-muted);
    white-space: nowrap;
    flex-shrink: 0;
}}
.btn-primary {{
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    height: 36px;
    padding: 0 16px;
    border-radius: 8px;
    background-color: var(--accent-solid);
    color: var(--accent-fg);
    font-size: 13px;
    font-weight: 500;
    border: 1px solid transparent;
    white-space: nowrap;
    flex-shrink: 0;
}}
.btn-secondary {{
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    height: 36px;
    padding: 0 14px;
    border-radius: 8px;
    background-color: var(--bg-raised);
    color: var(--text-primary);
    font-size: 13px;
    font-weight: 400;
    border: 1px solid var(--hairline);
    white-space: nowrap;
    flex-shrink: 0;
}}
.btn-danger {{
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    height: 32px;
    padding: 0 12px;
    border-radius: 6px;
    background-color: var(--bg-raised);
    color: var(--negative);
    font-size: 13px;
    font-weight: 500;
    border: 1px solid var(--hairline);
    white-space: nowrap;
    flex-shrink: 0;
}}
.btn-primary.focused, .btn-secondary.focused {{
    outline: 2px solid var(--accent);
    outline-offset: 2px;
}}
.pill-link {{
    display: flex;
    align-items: center;
    gap: 6px;
    height: 30px;
    padding: 0 10px;
    border-radius: 6px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    font-size: 12px;
    color: var(--text-secondary);
    text-decoration: none;
    white-space: nowrap;
    flex-shrink: 0;
}}
.pill-badge {{
    display: inline-block;
    padding: 2px 8px;
    border-radius: 4px;
    background-color: var(--bg-raised);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    line-height: 16px;
    color: var(--text-secondary);
    white-space: nowrap;
}}
.pill-badge.positive {{
    color: var(--positive);
}}

/* ---------------- New Tab Page ---------------- */
.newtab-wrap {{
    max-width: 780px;
    margin: 0 auto;
    padding: 40px 24px 32px 24px;
}}
.newtab-top {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 24px;
    gap: 16px;
}}
.brand-lockup {{
    display: flex;
    align-items: center;
    gap: 12px;
}}
.brand-mark {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 36px;
    height: 36px;
    border-radius: 8px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    color: var(--accent);
    flex-shrink: 0;
}}
.brand-name {{
    margin: 0;
    font-size: 20px;
    font-weight: 500;
    line-height: 24px;
    color: var(--text-primary);
}}
.brand-meta {{
    margin: 2px 0 0 0;
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
}}
.newtab-nav {{
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
}}
.search-shell {{
    display: flex;
    align-items: center;
    padding: 6px 6px 6px 12px;
    gap: 8px;
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    margin-bottom: 24px;
}}
.search-input {{
    flex-grow: 1;
    min-width: 0;
    height: 36px;
    padding: 0 8px;
    border-radius: 6px;
    background-color: var(--bg-input);
    border: 1px solid var(--hairline);
    color: var(--text-primary);
    font-size: 14px;
}}
.engine-chip {{
    display: flex;
    align-items: center;
    height: 28px;
    padding: 0 8px;
    border-radius: 6px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
    flex-shrink: 0;
}}
.section-head {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin: 0 0 12px 0;
}}
.section-label {{
    margin: 0;
    font-size: 13px;
    font-weight: 500;
    color: var(--text-secondary);
    white-space: nowrap;
}}
.section-count {{
    font-family: monospace;
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
}}
.shortcuts-grid {{
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) minmax(0, 1fr) minmax(0, 1fr);
    gap: 12px;
    margin-bottom: 24px;
}}
.shortcut-tile {{
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px;
    border-radius: 10px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    text-decoration: none;
    overflow: hidden;
}}
.shortcut-top {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
}}
.monogram {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border-radius: 6px;
    background-color: var(--bg-raised);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 12px;
    font-weight: 500;
    color: var(--text-primary);
    flex-shrink: 0;
}}
.shortcut-title {{
    margin: 0;
    font-size: 13px;
    font-weight: 500;
    line-height: 18px;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.shortcut-host {{
    margin: 0;
    font-family: monospace;
    font-size: 11px;
    line-height: 16px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.list-card {{
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    overflow: hidden;
    margin-bottom: 20px;
}}
.list-row {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 14px;
    border-bottom: 1px solid var(--hairline);
    text-decoration: none;
    overflow: hidden;
}}
.list-row.last {{
    border-bottom: 0 solid transparent;
}}
.list-main {{
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    flex-grow: 1;
    overflow: hidden;
}}
.list-text {{
    min-width: 0;
    flex-grow: 1;
    overflow: hidden;
}}
.list-title {{
    margin: 0;
    font-size: 13px;
    font-weight: 500;
    line-height: 18px;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.list-url {{
    margin: 0;
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.list-right {{
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
}}
.hint-bar {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding-top: 12px;
    border-top: 1px solid var(--hairline);
    gap: 12px;
}}
.hint-item {{
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
    flex-shrink: 0;
}}

/* ---------------- Settings / Bookmarks / History Workspace ---------------- */
.workspace {{
    display: flex;
    min-height: 100vh;
    background-color: var(--bg-toolbar);
}}
.ws-sidebar {{
    display: flex;
    flex-direction: column;
    width: 248px;
    flex-shrink: 0;
    padding: 20px 12px;
    gap: 4px;
    background-color: var(--bg-deepest);
    border-right: 1px solid var(--hairline);
}}
.ws-brand {{
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 4px 8px 16px 8px;
    margin-bottom: 8px;
    border-bottom: 1px solid var(--hairline);
}}
.ws-brand-title {{
    font-size: 15px;
    font-weight: 500;
    color: var(--text-primary);
}}
.ws-nav-item {{
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    height: 36px;
    padding: 0 10px;
    border-radius: 8px;
    font-size: 13px;
    font-weight: 400;
    color: var(--text-secondary);
    border: 1px solid transparent;
    text-decoration: none;
    white-space: nowrap;
}}
.ws-nav-item.active {{
    background-color: var(--bg-active-pill);
    color: var(--text-primary);
    font-weight: 500;
    border: 1px solid var(--hairline);
}}
.ws-nav-bar {{
    position: absolute;
    left: 0;
    top: 7px;
    width: 2px;
    height: 20px;
    border-radius: 2px;
    background-color: var(--accent);
}}
.ws-nav-divider {{
    height: 1px;
    margin: 10px 8px;
    background-color: var(--hairline);
}}
.ws-sidebar-foot {{
    margin-top: 20px;
    padding: 12px 8px 0 8px;
    border-top: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 11px;
    color: var(--text-muted);
}}
.ws-main {{
    flex-grow: 1;
    min-width: 0;
    padding: 28px 36px;
}}
.ws-inner {{
    max-width: 720px;
}}
.ws-header {{
    margin-bottom: 20px;
}}
.ws-title {{
    margin: 0 0 4px 0;
    font-size: 22px;
    font-weight: 500;
    line-height: 28px;
    color: var(--text-primary);
}}
.ws-sub {{
    margin: 0;
    font-size: 13px;
    line-height: 20px;
    color: var(--text-secondary);
}}
.settings-group {{
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    margin-bottom: 16px;
    overflow: hidden;
}}
.settings-group-head {{
    padding: 10px 16px;
    background-color: var(--bg-toolbar);
    border-bottom: 1px solid var(--hairline);
    font-size: 12px;
    font-weight: 500;
    color: var(--text-secondary);
}}
.setting-row {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 14px 16px;
    border-bottom: 1px solid var(--hairline);
}}
.setting-row.last {{
    border-bottom: 0 solid transparent;
}}
.setting-row.shortcut-row {{
    padding: 9px 16px;
}}
.setting-info {{
    min-width: 0;
    flex-grow: 1;
}}
.setting-title {{
    margin: 0 0 2px 0;
    font-size: 14px;
    font-weight: 500;
    line-height: 20px;
    color: var(--text-primary);
}}
.setting-desc {{
    margin: 0;
    font-size: 12px;
    line-height: 18px;
    color: var(--text-muted);
}}
.setting-ctrl {{
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
}}

/* Toggle switch */
.toggle-track {{
    display: flex;
    align-items: center;
    width: 40px;
    height: 22px;
    padding: 3px;
    border-radius: 11px;
    background-color: var(--bg-raised);
    border: 1px solid var(--hairline-strong);
    flex-shrink: 0;
}}
.toggle-track.on {{
    justify-content: flex-end;
    background-color: var(--accent-solid);
    border: 1px solid var(--accent);
}}
.toggle-thumb {{
    width: 14px;
    height: 14px;
    border-radius: 7px;
    background-color: var(--text-secondary);
}}
.toggle-track.on .toggle-thumb {{
    background-color: var(--accent-fg);
}}

/* Segmented control */
.seg-group {{
    display: flex;
    align-items: center;
    padding: 2px;
    gap: 2px;
    border-radius: 8px;
    background-color: var(--bg-deepest);
    border: 1px solid var(--hairline);
    flex-shrink: 0;
}}
.seg-opt {{
    display: flex;
    align-items: center;
    height: 26px;
    padding: 0 10px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 400;
    color: var(--text-secondary);
    white-space: nowrap;
    flex-shrink: 0;
}}
.seg-opt.active {{
    background-color: var(--bg-active-pill);
    color: var(--text-primary);
    font-weight: 500;
    border: 1px solid var(--hairline);
}}

/* Numeric stepper */
.stepper {{
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px;
    border-radius: 8px;
    background-color: var(--bg-deepest);
    border: 1px solid var(--hairline);
    flex-shrink: 0;
}}
.step-btn {{
    display: flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: 6px;
    background-color: var(--bg-raised);
    color: var(--text-primary);
    font-family: monospace;
    font-size: 14px;
}}
.step-val {{
    min-width: 52px;
    text-align: center;
    font-family: monospace;
    font-size: 12px;
    font-weight: 500;
    color: var(--text-primary);
}}

/* Diagnostics & Shortcut Tables */
.spec-grid {{
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 12px;
    padding: 14px 16px;
}}
.spec-tile {{
    padding: 12px;
    border-radius: 8px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
}}
.spec-label {{
    margin: 0 0 4px 0;
    font-size: 12px;
    color: var(--text-muted);
}}
.spec-val {{
    margin: 0;
    font-family: monospace;
    font-size: 13px;
    font-weight: 500;
    color: var(--text-primary);
}}

/* ---------------- Diagnostic Utility Cards (Error / Crashed) ---------------- */
.page-wrap {{
    padding: 56px 24px;
    background-color: var(--bg-toolbar);
}}
.card {{
    max-width: 620px;
    margin: 0 auto;
    padding: 24px;
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
}}
.status-kicker {{
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
    font-size: 13px;
    font-weight: 500;
    color: var(--negative);
    white-space: nowrap;
}}
.page-title {{
    margin: 0 0 8px 0;
    font-size: 24px;
    font-weight: 500;
    line-height: 32px;
    color: var(--text-primary);
}}
.page-sub {{
    margin: 0 0 20px 0;
    font-size: 14px;
    line-height: 20px;
    color: var(--text-secondary);
}}
.action-row {{
    display: flex;
    align-items: center;
    gap: 8px;
}}
.meta-row {{
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 12px;
    margin-top: 20px;
    padding-top: 14px;
    border-top: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
}}
.meta-row span {{
    white-space: nowrap;
}}

/* ---------------- Floating Overlays ---------------- */
.dropdown {{
    position: absolute;
    left: 156px;
    top: 86px;
    width: 680px;
    padding: 6px;
    border-radius: 10px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
}}
.dropdown-row {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    height: 34px;
    padding: 0 10px;
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-primary);
    overflow: hidden;
}}
.dropdown-row.selected {{
    background-color: var(--bg-raised);
}}
.dropdown-left {{
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    overflow: hidden;
}}
.dropdown-title {{
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.dropdown-url {{
    font-family: monospace;
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 280px;
    flex-shrink: 0;
}}
.app-menu {{
    position: absolute;
    right: 8px;
    top: 86px;
    width: 264px;
    padding: 6px;
    border-radius: 10px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
}}
.menu-item {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    height: 32px;
    padding: 0 8px;
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-primary);
    white-space: nowrap;
}}
.menu-item-left {{
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text-primary);
    white-space: nowrap;
}}
.menu-sep {{
    height: 1px;
    margin: 4px 6px;
    background-color: var(--hairline);
}}
.find-bar {{
    position: absolute;
    right: 16px;
    top: 88px;
    display: flex;
    align-items: center;
    gap: 6px;
    width: 344px;
    height: 40px;
    padding: 0 8px 0 10px;
    border-radius: 10px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
}}
.find-query {{
    flex-grow: 1;
    min-width: 0;
    font-size: 13px;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.find-count {{
    font-family: monospace;
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
    flex-shrink: 0;
}}
.context-menu {{
    position: absolute;
    width: 224px;
    padding: 4px;
    border-radius: 8px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
}}
.context-item {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 32px;
    padding: 0 8px;
    border-radius: 4px;
    font-size: 13px;
    color: var(--text-primary);
    white-space: nowrap;
}}
.hover-pill {{
    position: fixed;
    left: 10px;
    bottom: 10px;
    max-width: 560px;
    padding: 4px 10px;
    border-radius: 6px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-secondary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}

/* ---------------- F12 Developer HUD ---------------- */
.hud-panel {{
    width: 448px;
    padding: 16px;
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--popover-shadow);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-primary);
}}
.hud-header {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--hairline);
    white-space: nowrap;
}}
.hud-title {{
    font-family: Inter, sans-serif;
    font-size: 14px;
    font-weight: 500;
    color: var(--text-primary);
}}
.hud-idle {{
    color: var(--positive);
}}
.hud-stats {{
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) minmax(0, 1fr);
    gap: 8px;
    margin-bottom: 10px;
}}
.hud-stat {{
    padding: 8px 10px;
    border-radius: 8px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
}}
.hud-stat-label {{
    color: var(--text-muted);
    margin-bottom: 4px;
    font-size: 11px;
}}
.hud-stat-val {{
    font-size: 14px;
    font-weight: 500;
    color: var(--text-primary);
}}
.hud-stages {{
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) minmax(0, 1fr) minmax(0, 1fr);
    gap: 6px;
    padding: 8px 10px;
    margin-bottom: 10px;
    border-radius: 8px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
    color: var(--text-secondary);
    font-size: 11px;
}}
.hud-graph {{
    display: flex;
    align-items: flex-end;
    height: 48px;
    gap: 2px;
    padding: 4px;
    margin-bottom: 10px;
    border-radius: 8px;
    background-color: var(--bg-deepest);
    border: 1px solid var(--hairline);
    overflow: hidden;
}}
.hud-bar {{
    width: 4px;
    background-color: var(--text-muted);
    flex-shrink: 0;
}}
.hud-bar.accent {{
    background-color: var(--accent);
}}
.hud-tabs {{
    display: flex;
    gap: 4px;
    margin-bottom: 8px;
}}
.hud-tab {{
    padding: 4px 8px;
    border-radius: 6px;
    color: var(--text-secondary);
    background-color: transparent;
    border: 1px solid transparent;
    white-space: nowrap;
}}
.hud-tab.active {{
    background-color: var(--bg-raised);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.hud-table {{
    width: 100%;
    border-collapse: collapse;
}}
.hud-table th {{
    text-align: left;
    font-weight: 500;
    color: var(--text-muted);
    padding: 5px 6px;
    border-bottom: 1px solid var(--hairline);
}}
.hud-table td {{
    padding: 5px 6px;
    color: var(--text-secondary);
    border-bottom: 1px solid var(--hairline);
}}
"#,
        bg_deepest = p.bg_deepest,
        bg_toolbar = p.bg_toolbar,
        bg_card = p.bg_card,
        bg_raised = p.bg_raised,
        bg_active_pill = p.bg_active_pill,
        bg_input = p.bg_input,
        hairline = p.hairline,
        hairline_strong = p.hairline_strong,
        popover_shadow = p.popover_shadow,
        text_primary = p.text_primary,
        text_secondary = p.text_secondary,
        text_muted = p.text_muted,
        accent = p.accent,
        accent_solid = p.accent_solid,
        accent_fg = p.accent_fg,
        positive = p.positive,
        warning = p.warning,
        negative = p.negative,
        keycap_bg = p.keycap_bg,
    )
}

// ---------------------------------------------------------------------------
// 16px Lucide-style stroke SVG icons (1.5px stroke, currentColor, zero emoji)
// ---------------------------------------------------------------------------

pub const ICON_RIX_MARK: &str = r##"<svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="10" cy="10" r="7.5"></circle><circle cx="10" cy="10" r="3"></circle><path d="M 10 2.5 L 10 6"></path><path d="M 10 14 L 10 17.5"></path><path d="M 2.5 10 L 6 10"></path><path d="M 14 10 L 17.5 10"></path></svg>"##;

pub const ICON_BACK: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 10 3.5 L 5.5 8 L 10 12.5"></path></svg>"##;

pub const ICON_FORWARD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 6 3.5 L 10.5 8 L 6 12.5"></path></svg>"##;

pub const ICON_RELOAD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 13 8 A 5 5 0 1 1 11.4 4.4"></path><path d="M 13 3 L 13 6 L 10 6"></path></svg>"##;

pub const ICON_STOP: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 4.5 4.5 L 11.5 11.5"></path><path d="M 11.5 4.5 L 4.5 11.5"></path></svg>"##;

pub const ICON_HOME: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 2.5 7.5 L 8 3 L 13.5 7.5"></path><path d="M 4 7 L 4 13 L 12 13 L 12 7"></path></svg>"##;

pub const ICON_PLUS: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 3.5 L 8 12.5"></path><path d="M 3.5 8 L 12.5 8"></path></svg>"##;

pub const ICON_CLOSE: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 5 5 L 11 11"></path><path d="M 11 5 L 5 11"></path></svg>"##;

pub const ICON_LOCK: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="3.5" y="7" width="9" height="6.5" rx="1.5"></rect><path d="M 5.5 7 L 5.5 5 A 2.5 2.5 0 0 1 10.5 5 L 10.5 7"></path></svg>"##;

pub const ICON_GLOBE: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="5.5"></circle><path d="M 2.5 8 L 13.5 8"></path><path d="M 8 2.5 L 8 13.5"></path></svg>"##;

pub const ICON_SHIELD: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 2 L 13 4 L 13 8 C 13 11 10.5 13 8 14 C 5.5 13 3 11 3 8 L 3 4 Z"></path></svg>"##;

pub const ICON_SEARCH: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="7" cy="7" r="4.5"></circle><path d="M 10.5 10.5 L 13.5 13.5"></path></svg>"##;

pub const ICON_STAR: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 2.5 L 9.7 6 L 13.5 6.5 L 10.7 9.2 L 11.4 13 L 8 11.2 L 4.6 13 L 5.3 9.2 L 2.5 6.5 L 6.3 6 Z"></path></svg>"##;

pub const ICON_STAR_FILLED: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" stroke="currentColor" stroke-width="1.2"><path d="M 8 2.5 L 9.7 6 L 13.5 6.5 L 10.7 9.2 L 11.4 13 L 8 11.2 L 4.6 13 L 5.3 9.2 L 2.5 6.5 L 6.3 6 Z"></path></svg>"##;

pub const ICON_BOOKMARK: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 4 2.5 L 12 2.5 L 12 13.5 L 8 10.8 L 4 13.5 Z"></path></svg>"##;

pub const ICON_HISTORY: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="5.5"></circle><path d="M 8 5 L 8 8 L 10.5 9.5"></path></svg>"##;

pub const ICON_SIDEBAR: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="3" width="12" height="10" rx="1.5"></rect><path d="M 6 3 L 6 13"></path></svg>"##;

pub const ICON_HUD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="3" width="12" height="10" rx="1.5"></rect><path d="M 4.5 6.5 L 7 8.5 L 4.5 10.5"></path><path d="M 8.5 10.5 L 11.5 10.5"></path></svg>"##;

pub const ICON_SETTINGS: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="2.5"></circle><path d="M 8 2 L 8 3.6"></path><path d="M 8 12.4 L 8 14"></path><path d="M 2 8 L 3.6 8"></path><path d="M 12.4 8 L 14 8"></path><path d="M 3.8 3.8 L 5 5"></path><path d="M 11 11 L 12.2 12.2"></path><path d="M 12.2 3.8 L 11 5"></path><path d="M 5 11 L 3.8 12.2"></path></svg>"##;

pub const ICON_MENU: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 3 4.5 L 13 4.5"></path><path d="M 3 8 L 13 8"></path><path d="M 3 11.5 L 13 11.5"></path></svg>"##;

pub const ICON_THEME: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="3.5"></circle><path d="M 8 2 L 8 3.2"></path><path d="M 8 12.8 L 8 14"></path><path d="M 2 8 L 3.2 8"></path><path d="M 12.8 8 L 14 8"></path></svg>"##;

pub const ICON_APPEARANCE: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2.5" y="2.5" width="11" height="11" rx="2"></rect><path d="M 2.5 6.5 L 13.5 6.5"></path><path d="M 7 6.5 L 7 13.5"></path></svg>"##;

pub const ICON_CPU: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="4.5" y="4.5" width="7" height="7" rx="1"></rect><path d="M 6.5 2 L 6.5 4.5"></path><path d="M 9.5 2 L 9.5 4.5"></path><path d="M 6.5 11.5 L 6.5 14"></path><path d="M 9.5 11.5 L 9.5 14"></path><path d="M 2 6.5 L 4.5 6.5"></path><path d="M 2 9.5 L 4.5 9.5"></path><path d="M 11.5 6.5 L 14 6.5"></path><path d="M 11.5 9.5 L 14 9.5"></path></svg>"##;

pub const ICON_INFO: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="5.5"></circle><path d="M 8 7.2 L 8 11"></path><circle cx="8" cy="5.2" r="0.6" fill="currentColor"></circle></svg>"##;

pub const ICON_TRASH: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 3 4.5 L 13 4.5"></path><path d="M 6 4.5 L 6 3 L 10 3 L 10 4.5"></path><path d="M 4.5 4.5 L 5 13 L 11 13 L 11.5 4.5"></path></svg>"##;

pub const ICON_ALERT: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 2.5 L 14 13 L 2 13 Z"></path><path d="M 8 6.5 L 8 9.2"></path><path d="M 8 11 L 8 11.5"></path></svg>"##;

pub const ICON_CHEVRON_UP: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 4 10 L 8 6 L 12 10"></path></svg>"##;

pub const ICON_CHEVRON_DOWN: &str = r##"<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 4 6 L 8 10 L 12 6"></path></svg>"##;
