//! Persistent RixBrowser preferences, search engine definitions, bookmark
//! entries, and Settings workspace navigation state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{percent_encode_query, TabLayoutMode, URL_NEWTAB};
use crate::tokens::ThemeMode;

/// Discrete page zoom levels supported by the browser toolbar and Settings stepper.
pub const ZOOM_STEPS_PCT: &[u16] = &[75, 80, 90, 100, 110, 125, 150];

/// Default search engine used by the omnibox and New Tab search bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SearchEngine {
    #[default]
    DuckDuckGo,
    Google,
    Startpage,
    Kagi,
}

impl SearchEngine {
    pub const ALL: [Self; 4] = [Self::DuckDuckGo, Self::Google, Self::Startpage, Self::Kagi];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Google => "google",
            Self::Startpage => "startpage",
            Self::Kagi => "kagi",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "DuckDuckGo",
            Self::Google => "Google",
            Self::Startpage => "Startpage",
            Self::Kagi => "Kagi",
        }
    }

    #[must_use]
    pub const fn action_url(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "https://duckduckgo.com/",
            Self::Google => "https://www.google.com/search",
            Self::Startpage => "https://www.startpage.com/do/search",
            Self::Kagi => "https://kagi.com/search",
        }
    }

    #[must_use]
    pub fn search_url(self, query: &str) -> String {
        let encoded = percent_encode_query(query.trim());
        match self {
            Self::DuckDuckGo => format!("https://duckduckgo.com/?q={encoded}"),
            Self::Google => format!("https://www.google.com/search?q={encoded}"),
            Self::Startpage => format!("https://www.startpage.com/do/search?q={encoded}"),
            Self::Kagi => format!("https://kagi.com/search?q={encoded}"),
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "duckduckgo" => Some(Self::DuckDuckGo),
            "google" => Some(Self::Google),
            "startpage" => Some(Self::Startpage),
            "kagi" => Some(Self::Kagi),
            _ => None,
        }
    }
}

/// Third-party cookie handling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CookiePolicy {
    #[default]
    BlockThirdParty,
    AllowAll,
    BlockAll,
}

impl CookiePolicy {
    pub const ALL: [Self; 3] = [Self::BlockThirdParty, Self::AllowAll, Self::BlockAll];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::BlockThirdParty => "block-3p",
            Self::AllowAll => "allow-all",
            Self::BlockAll => "block-all",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BlockThirdParty => "Block third-party",
            Self::AllowAll => "Allow all",
            Self::BlockAll => "Block all",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "block-3p" => Some(Self::BlockThirdParty),
            "allow-all" => Some(Self::AllowAll),
            "block-all" => Some(Self::BlockAll),
            _ => None,
        }
    }
}

/// Cranelift JavaScript JIT execution tier policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JitMode {
    #[default]
    FullThreeTier,
    BaselineOnly,
    InterpreterOnly,
}

impl JitMode {
    pub const ALL: [Self; 3] = [
        Self::FullThreeTier,
        Self::BaselineOnly,
        Self::InterpreterOnly,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::FullThreeTier => "full-3tier",
            Self::BaselineOnly => "baseline",
            Self::InterpreterOnly => "interpreter",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FullThreeTier => "3-tier Cranelift",
            Self::BaselineOnly => "Baseline JIT",
            Self::InterpreterOnly => "Interpreter",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "full-3tier" => Some(Self::FullThreeTier),
            "baseline" => Some(Self::BaselineOnly),
            "interpreter" => Some(Self::InterpreterOnly),
            _ => None,
        }
    }
}

/// Active sidebar section inside `rix://chrome/settings.html`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SettingsSection {
    #[default]
    Appearance,
    Search,
    Privacy,
    Performance,
    About,
}

impl SettingsSection {
    pub const ALL: [Self; 5] = [
        Self::Appearance,
        Self::Search,
        Self::Privacy,
        Self::Performance,
        Self::About,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Search => "search",
            Self::Privacy => "privacy",
            Self::Performance => "performance",
            Self::About => "about",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Search => "Search & address bar",
            Self::Privacy => "Privacy & security",
            Self::Performance => "Performance & media",
            Self::About => "About & shortcuts",
        }
    }

    #[must_use]
    pub const fn subtitle(self) -> &'static str {
        match self {
            Self::Appearance => {
                "Theme polarity, tab strip geometry, density, and default typography."
            }
            Self::Search => {
                "Default search engine, address bar formatting, and autocomplete behavior."
            }
            Self::Privacy => {
                "Transport upgrades, cookie isolation, and local browsing data controls."
            }
            Self::Performance => {
                "GPU compositing, Cranelift JavaScript JIT tiers, pure-Rust codecs, and telemetry."
            }
            Self::About => "Build details, active engine modules, and keyboard shortcuts.",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "appearance" => Some(Self::Appearance),
            "search" => Some(Self::Search),
            "privacy" => Some(Self::Privacy),
            "performance" => Some(Self::Performance),
            "about" => Some(Self::About),
            _ => None,
        }
    }
}

/// Saved bookmark record rendered in the Bookmarks Bar, New Tab shortcuts, and `bookmarks.html`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkItem {
    pub url: String,
    pub title: String,
    pub short_label: String,
    pub pinned: bool,
}

impl BookmarkItem {
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        title: impl Into<String>,
        short_label: impl Into<String>,
        pinned: bool,
    ) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            short_label: short_label.into(),
            pinned,
        }
    }
}

/// Transient Find-in-page bar state (`Ctrl+F`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindInPageState {
    pub open: bool,
    pub query: String,
    pub match_index: usize,
    pub total_matches: usize,
}

impl FindInPageState {
    pub fn open_and_focus(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn insert_str(&mut self, input: &str) {
        let clean: String = input
            .chars()
            .filter(|c| !matches!(c, '\r' | '\n' | '\t'))
            .collect();
        self.query.push_str(&clean);
        self.match_index = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.match_index = 0;
    }

    pub fn step_match(&mut self, forward: bool) {
        if self.total_matches == 0 {
            self.match_index = 0;
            return;
        }
        if forward {
            self.match_index = (self.match_index + 1) % self.total_matches;
        } else if self.match_index == 0 {
            self.match_index = self.total_matches - 1;
        } else {
            self.match_index -= 1;
        }
    }
}

/// Complete serializable RixBrowser user preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserSettings {
    pub theme: ThemeMode,
    pub tab_layout: TabLayoutMode,
    pub show_bookmarks_bar: bool,
    pub compact_toolbar: bool,
    pub default_zoom_pct: u16,
    pub base_font_size_px: u16,
    pub search_engine: SearchEngine,
    pub autocomplete_enabled: bool,
    pub show_full_url: bool,
    pub https_only: bool,
    pub cookie_policy: CookiePolicy,
    pub send_dnt: bool,
    pub jit_mode: JitMode,
    pub hardware_compositing: bool,
    pub open_hud_on_startup: bool,
    pub home_url: String,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            tab_layout: TabLayoutMode::Horizontal,
            show_bookmarks_bar: true,
            compact_toolbar: false,
            default_zoom_pct: 100,
            base_font_size_px: 14,
            search_engine: SearchEngine::DuckDuckGo,
            autocomplete_enabled: true,
            show_full_url: true,
            https_only: true,
            cookie_policy: CookiePolicy::BlockThirdParty,
            send_dnt: true,
            jit_mode: JitMode::FullThreeTier,
            hardware_compositing: true,
            open_hud_on_startup: false,
            home_url: URL_NEWTAB.to_string(),
        }
    }
}

impl BrowserSettings {
    /// Steps `default_zoom_pct` up (`delta > 0`) or down (`delta < 0`) along [`ZOOM_STEPS_PCT`].
    pub fn step_zoom(&mut self, delta: i32) {
        let cur_idx = ZOOM_STEPS_PCT
            .iter()
            .position(|&z| z >= self.default_zoom_pct)
            .unwrap_or(3) as i32;
        let max_idx = (ZOOM_STEPS_PCT.len() - 1) as i32;
        let next_idx = (cur_idx + delta).clamp(0, max_idx) as usize;
        self.default_zoom_pct = ZOOM_STEPS_PCT[next_idx];
    }

    pub fn reset_zoom(&mut self) {
        self.default_zoom_pct = 100;
    }

    /// Steps `base_font_size_px` within `[13, 18]`.
    pub fn step_font_size(&mut self, delta: i32) {
        let next = (i32::from(self.base_font_size_px) + delta).clamp(13, 18);
        self.base_font_size_px = next as u16;
    }

    /// Resolves the default XDG/HOME configuration path for persisting settings.
    #[must_use]
    pub fn default_config_path() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return Some(PathBuf::from(xdg).join("rixbrowser").join("settings.json"));
            }
        }
        std::env::var("HOME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .map(|h| {
                PathBuf::from(h)
                    .join(".config")
                    .join("rixbrowser")
                    .join("settings.json")
            })
    }

    /// Loads preferences from a JSON file on disk.
    pub fn load_from_file(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    /// Persists preferences atomically (write temp + rename) as pretty JSON.
    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
