//! Design tokens and 16px stroke SVG icons crafted strictly with the `unslop`
//! design system (Precision Optical Instrument direction).
//!
//! Rules enforced:
//! - Tinted neutrals only (cool ink-slate dark ramp, warm paper-stone light ramp)
//! - One restrained cobalt accent reserved for focus rings, active loading bar,
//!   and primary confirmation actions
//! - Strict 4px spacing scale (4, 8, 12, 16, 20, 24, 32)
//! - Nested border radii (`inner = outer - padding`, minimum 4px)
//! - Crisp 16px stroke SVG icons (`stroke-width="1.5"`), zero emoji, zero gradients/glows

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
    pub hairline: &'static str,
    pub hairline_strong: &'static str,
    pub text_primary: &'static str,
    pub text_secondary: &'static str,
    pub text_muted: &'static str,
    pub accent: &'static str,
    pub accent_fg: &'static str,
    pub positive: &'static str,
    pub negative: &'static str,
    pub keycap_bg: &'static str,
    pub clear_rgba: [u8; 4],
}

/// Cool ink-slate dark ramp tuned for low eye fatigue around bright web content.
pub const DARK_PALETTE: ChromePalette = ChromePalette {
    bg_deepest: "#0d1017",
    bg_toolbar: "#12161f",
    bg_card: "#171c26",
    bg_raised: "#202736",
    hairline: "#242b3a",
    hairline_strong: "#313b4f",
    text_primary: "#eef0f5",
    text_secondary: "#a3acbe",
    text_muted: "#778196",
    accent: "#3b82f6",
    accent_fg: "#ffffff",
    positive: "#4cc38a",
    negative: "#e5484d",
    keycap_bg: "#1b2130",
    clear_rgba: [13, 16, 23, 255],
};

/// Warm paper-stone light ramp tuned for calm daytime readability.
pub const LIGHT_PALETTE: ChromePalette = ChromePalette {
    bg_deepest: "#ece9e2",
    bg_toolbar: "#f6f4ef",
    bg_card: "#ffffff",
    bg_raised: "#e6e2d9",
    hairline: "#dad5cb",
    hairline_strong: "#c4beb2",
    text_primary: "#1c1916",
    text_secondary: "#544d44",
    text_muted: "#726a5f",
    accent: "#1d5bd6",
    accent_fg: "#ffffff",
    positive: "#16a34a",
    negative: "#dc2626",
    keycap_bg: "#ebe7df",
    clear_rgba: [236, 233, 226, 255],
};

#[must_use]
pub const fn palette_for(theme: ThemeMode) -> ChromePalette {
    match theme {
        ThemeMode::Dark => DARK_PALETTE,
        ThemeMode::Light => LIGHT_PALETTE,
    }
}

/// Generates the complete `tokens.css` custom-property & base layout stylesheet for `theme`.
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
    --hairline: {hairline};
    --hairline-strong: {hairline_strong};
    --text-primary: {text_primary};
    --text-secondary: {text_secondary};
    --text-muted: {text_muted};
    --accent: {accent};
    --accent-fg: {accent_fg};
    --positive: {positive};
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
.chrome-shell {{
    width: 100%;
    background-color: var(--bg-deepest);
    border-bottom: 1px solid var(--hairline);
}}
.tab-strip {{
    display: flex;
    align-items: center;
    height: 40px;
    padding: 4px 8px;
    gap: 4px;
    background-color: var(--bg-deepest);
}}
.tab-item {{
    display: flex;
    align-items: center;
    height: 32px;
    min-width: 120px;
    max-width: 220px;
    padding: 0 8px;
    gap: 8px;
    border-radius: 6px;
    background-color: var(--bg-deepest);
    color: var(--text-secondary);
    border: 1px solid transparent;
}}
.tab-item.active {{
    background-color: var(--bg-toolbar);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.tab-item.crashed {{
    border: 1px solid var(--negative);
}}
.tab-title {{
    flex-grow: 1;
    font-size: 13px;
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.tab-item.active .tab-title {{
    font-weight: 500;
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
}}
.icon-btn.active {{
    background-color: var(--bg-raised);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.icon-btn.focused, .tab-item.focused {{
    outline: 2px solid var(--accent);
    outline-offset: -2px;
}}
.icon-btn.disabled {{
    color: var(--text-muted);
    opacity: 0.45;
}}
.toolbar {{
    display: flex;
    align-items: center;
    height: 44px;
    padding: 4px 8px;
    gap: 8px;
    background-color: var(--bg-toolbar);
}}
.nav-group {{
    display: flex;
    align-items: center;
    gap: 4px;
}}
.omnibox {{
    position: relative;
    display: flex;
    align-items: center;
    flex-grow: 1;
    height: 32px;
    padding: 0 12px;
    gap: 8px;
    border-radius: 8px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
}}
.omnibox.focused {{
    background-color: var(--bg-raised);
    border: 1px solid var(--accent);
    outline: 2px solid var(--accent);
    outline-offset: -1px;
}}
.origin-badge {{
    display: flex;
    align-items: center;
    gap: 4px;
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
}}
.origin-badge.secure {{
    color: var(--positive);
}}
.origin-badge.insecure {{
    color: var(--negative);
}}
.omnibox-text {{
    flex-grow: 1;
    font-family: monospace;
    font-size: 13px;
    line-height: 20px;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.omnibox-placeholder {{
    color: var(--text-muted);
}}
.keycap {{
    display: inline-block;
    padding: 2px 6px;
    border-radius: 4px;
    background-color: var(--keycap-bg);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
}}
.load-progress {{
    position: absolute;
    left: 8px;
    bottom: 0;
    height: 2px;
    background-color: var(--accent);
}}
.sidebar-tabs {{
    width: 224px;
    padding: 8px;
    background-color: var(--bg-deepest);
    border-right: 1px solid var(--hairline);
}}
.sidebar-tab-row {{
    display: flex;
    align-items: center;
    height: 32px;
    padding: 0 8px;
    margin-bottom: 4px;
    gap: 8px;
    border-radius: 6px;
    color: var(--text-secondary);
}}
.sidebar-tab-row.active {{
    background-color: var(--bg-raised);
    color: var(--text-primary);
    border: 1px solid var(--hairline);
}}
.page-wrap {{
    padding: 32px 24px;
    background-color: var(--bg-toolbar);
}}
.card {{
    max-width: 680px;
    margin: 0 auto;
    padding: 24px;
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
}}
.page-title {{
    margin: 0 0 8px 0;
    font-size: 28px;
    font-weight: 500;
    line-height: 36px;
    color: var(--text-primary);
}}
.page-sub {{
    margin: 0 0 20px 0;
    font-size: 14px;
    line-height: 20px;
    color: var(--text-secondary);
}}
.btn-primary {{
    display: inline-block;
    height: 36px;
    padding: 8px 16px;
    border-radius: 8px;
    background-color: var(--accent);
    color: var(--accent-fg);
    font-size: 14px;
    font-weight: 500;
    border: 1px solid transparent;
}}
.btn-secondary {{
    display: inline-block;
    height: 36px;
    padding: 8px 16px;
    border-radius: 8px;
    background-color: var(--bg-raised);
    color: var(--text-primary);
    font-size: 14px;
    font-weight: 400;
    border: 1px solid var(--hairline);
}}
.btn-primary.focused, .btn-secondary.focused {{
    outline: 2px solid var(--accent);
    outline-offset: 2px;
}}
.meta-row {{
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 16px;
    padding-top: 16px;
    border-top: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-muted);
}}
.history-grid {{
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
    margin-top: 20px;
}}
.history-item {{
    padding: 12px;
    border-radius: 8px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
}}
.history-title {{
    font-size: 14px;
    font-weight: 500;
    color: var(--text-primary);
    margin: 0 0 4px 0;
}}
.history-url {{
    font-family: monospace;
    font-size: 12px;
    color: var(--text-muted);
    margin: 0;
}}
.dropdown {{
    position: absolute;
    left: 120px;
    top: 84px;
    width: 640px;
    padding: 8px;
    border-radius: 10px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
}}
.dropdown-row {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 32px;
    padding: 0 8px;
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-primary);
}}
.dropdown-row.selected {{
    background-color: var(--bg-raised);
}}
.context-menu {{
    position: absolute;
    width: 216px;
    padding: 4px;
    border-radius: 8px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
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
}}
.hover-pill {{
    position: absolute;
    left: 8px;
    bottom: 8px;
    max-width: 520px;
    padding: 4px 8px;
    border-radius: 6px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline);
    font-family: monospace;
    font-size: 12px;
    line-height: 16px;
    color: var(--text-secondary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}}
.hud-panel {{
    width: 440px;
    padding: 16px;
    border-radius: 12px;
    background-color: var(--bg-card);
    border: 1px solid var(--hairline-strong);
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
    grid-template-columns: 1fr 1fr 1fr;
    gap: 8px;
    margin-bottom: 12px;
}}
.hud-stat {{
    padding: 8px;
    border-radius: 6px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
}}
.hud-stat-label {{
    color: var(--text-muted);
    margin-bottom: 4px;
}}
.hud-stat-val {{
    font-size: 14px;
    font-weight: 500;
    color: var(--text-primary);
}}
.hud-stages {{
    display: flex;
    gap: 8px;
    padding: 8px;
    margin-bottom: 12px;
    border-radius: 6px;
    background-color: var(--bg-toolbar);
    border: 1px solid var(--hairline);
    color: var(--text-secondary);
}}
.hud-graph {{
    display: flex;
    align-items: flex-end;
    height: 48px;
    gap: 2px;
    padding: 4px;
    margin-bottom: 12px;
    border-radius: 6px;
    background-color: var(--bg-deepest);
    border: 1px solid var(--hairline);
}}
.hud-bar {{
    width: 4px;
    background-color: var(--text-muted);
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
    padding: 4px 6px;
    border-bottom: 1px solid var(--hairline);
}}
.hud-table td {{
    padding: 4px 6px;
    color: var(--text-secondary);
    border-bottom: 1px solid var(--hairline);
}}
"#,
        bg_deepest = p.bg_deepest,
        bg_toolbar = p.bg_toolbar,
        bg_card = p.bg_card,
        bg_raised = p.bg_raised,
        hairline = p.hairline,
        hairline_strong = p.hairline_strong,
        text_primary = p.text_primary,
        text_secondary = p.text_secondary,
        text_muted = p.text_muted,
        accent = p.accent,
        accent_fg = p.accent_fg,
        positive = p.positive,
        negative = p.negative,
        keycap_bg = p.keycap_bg,
    )
}

// ---------------------------------------------------------------------------
// 16px Lucide-style stroke SVG icons (1.5px stroke, currentColor, zero emoji)
// ---------------------------------------------------------------------------

pub const ICON_BACK: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 10 3 L 5 8 L 10 13"></path></svg>"##;

pub const ICON_FORWARD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 6 3 L 11 8 L 6 13"></path></svg>"##;

pub const ICON_RELOAD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 13 8 A 5 5 0 1 1 11.5 4.5"></path><path d="M 13 3 L 13 6 L 10 6"></path></svg>"##;

pub const ICON_STOP: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 4 4 L 12 12"></path><path d="M 12 4 L 4 12"></path></svg>"##;

pub const ICON_PLUS: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 3 L 8 13"></path><path d="M 3 8 L 13 8"></path></svg>"##;

pub const ICON_CLOSE: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 5 5 L 11 11"></path><path d="M 11 5 L 5 11"></path></svg>"##;

pub const ICON_LOCK: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="3" y="7" width="10" height="7"></rect><path d="M 5 7 L 5 5 A 3 3 0 0 1 11 5 L 11 7"></path></svg>"##;

pub const ICON_GLOBE: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="5.5"></circle><path d="M 2.5 8 L 13.5 8"></path><path d="M 8 2.5 L 8 13.5"></path></svg>"##;

pub const ICON_SHIELD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 2 L 13 4 L 13 8 C 13 11 10.5 13 8 14 C 5.5 13 3 11 3 8 L 3 4 Z"></path></svg>"##;

pub const ICON_SIDEBAR: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="3" width="12" height="10"></rect><path d="M 6 3 L 6 13"></path></svg>"##;

pub const ICON_HUD: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="3" width="12" height="10"></rect><path d="M 4 6 L 7 8 L 4 10"></path><path d="M 8 10 L 12 10"></path></svg>"##;

pub const ICON_THEME: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="4"></circle><path d="M 8 2 L 8 3"></path><path d="M 8 13 L 8 14"></path><path d="M 2 8 L 3 8"></path><path d="M 13 8 L 14 8"></path></svg>"##;

pub const ICON_ALERT: &str = r##"<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M 8 2 L 14 13 L 2 13 Z"></path><path d="M 8 6 L 8 9"></path><path d="M 8 11 L 8 11.5"></path></svg>"##;
