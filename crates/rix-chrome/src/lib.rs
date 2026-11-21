#![deny(unsafe_code)]
//! Isolated Parent-Process Browser Chrome UI (`rix://chrome/*`) and built-in
//! `F12` Developer HUD for RixBrowser, crafted with the `unslop` design system.

pub mod assets;
pub mod hud;
pub mod renderer;
pub mod state;
pub mod tokens;

pub use assets::{
    chrome_asset_provider, escape_html, render_browser_chrome_html, render_crashed_html,
    render_error_html, render_hud_html, render_newtab_html, render_overlay_html,
};
pub use hud::{
    ConsoleEntry, DevHudState, FrameStageSample, HudTab, MissingFeatureEntry, NetworkEntry,
    MAX_FRAME_SAMPLES,
};
pub use renderer::{
    find_link_href, parse_chrome_hit_id, ChromeHitTarget, ChromeRenderer, RenderedChromeLayer,
};
pub use state::{
    default_title_for_url, looks_like_url, resolve_omnibox_input, AutocompleteItem, ChromeState,
    ClosedTabSnapshot, ContextMenuState, HistoryEntry, OmniboxState, SecurityState, TabEntry,
    TabLayoutMode, URL_BROWSER, URL_CRASHED, URL_ERROR, URL_NEWTAB,
};
pub use tokens::{
    chrome_stylesheet, palette_for, ChromePalette, ThemeMode, DARK_PALETTE, LIGHT_PALETTE,
};
