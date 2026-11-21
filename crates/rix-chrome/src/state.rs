//! Parent-process Browser Chrome state machine: tabs, omnibox editing,
//! smart URL/search resolution, history autocomplete, security origin badge,
//! hover URL preview, and context menu.

use rix_core::TabId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::tokens::ThemeMode;

/// Built-in RixBrowser internal chrome page URLs.
pub const URL_NEWTAB: &str = "rix://chrome/newtab.html";
pub const URL_BROWSER: &str = "rix://chrome/browser.html";
pub const URL_ERROR: &str = "rix://chrome/error.html";
pub const URL_CRASHED: &str = "rix://chrome/crashed.html";

/// Tab strip orientation (horizontal top bar or optional vertical sidebar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TabLayoutMode {
    #[default]
    Horizontal,
    Vertical,
}

/// Security classification shown in the omnibox origin badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SecurityState {
    #[default]
    Internal,
    SecureHttps,
    InsecureHttp,
}

impl SecurityState {
    #[must_use]
    pub fn from_url(url: &str) -> Self {
        let lower = url.trim().to_ascii_lowercase();
        if lower.starts_with("https://") {
            Self::SecureHttps
        } else if lower.starts_with("http://") {
            Self::InsecureHttp
        } else {
            Self::Internal
        }
    }

    #[must_use]
    pub const fn badge_label(self) -> &'static str {
        match self {
            Self::Internal => "Internal",
            Self::SecureHttps => "HTTPS",
            Self::InsecureHttp => "HTTP",
        }
    }

    #[must_use]
    pub const fn css_class(self) -> &'static str {
        match self {
            Self::Internal => "origin-badge",
            Self::SecureHttps => "origin-badge secure",
            Self::InsecureHttp => "origin-badge insecure",
        }
    }
}

/// State of a single open browser tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabEntry {
    pub id: TabId,
    pub title: String,
    pub url: String,
    pub history: Vec<String>,
    pub history_index: usize,
    pub is_loading: bool,
    pub load_progress: f32,
    pub crashed_reason: Option<String>,
    pub error_message: Option<String>,
    pub content_pid: Option<u32>,
    pub scroll_y: f32,
}

impl TabEntry {
    #[must_use]
    pub fn new(id: TabId, initial_url: impl Into<String>) -> Self {
        let url = initial_url.into();
        let title = default_title_for_url(&url);
        Self {
            id,
            title,
            url: url.clone(),
            history: vec![url],
            history_index: 0,
            is_loading: false,
            load_progress: 0.0,
            crashed_reason: None,
            error_message: None,
            content_pid: None,
            scroll_y: 0.0,
        }
    }

    #[must_use]
    pub fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    #[must_use]
    pub fn security_state(&self) -> SecurityState {
        SecurityState::from_url(&self.url)
    }

    pub fn navigate_to(&mut self, url: impl Into<String>) {
        let target = url.into();
        self.history.truncate(self.history_index + 1);
        if self.history.last().is_none_or(|prev| prev != &target) {
            self.history.push(target.clone());
            self.history_index = self.history.len() - 1;
        }
        self.url = target;
        self.title = default_title_for_url(&self.url);
        self.crashed_reason = None;
        self.error_message = None;
        self.scroll_y = 0.0;
    }
}

/// Saved browsing history record used for omnibox autocomplete and `newtab.html`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub visits: u32,
}

/// Autocomplete suggestion item displayed in the omnibox dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub url: String,
    pub title: String,
    pub is_search: bool,
}

/// Interactive text buffer state for the omnibox (`Ctrl+L`).
#[derive(Debug, Clone, Default)]
pub struct OmniboxState {
    pub text: String,
    pub focused: bool,
    pub caret: usize,
    pub selection_anchor: Option<usize>,
    pub selected_suggestion: Option<usize>,
    pub dropdown_open: bool,
}

impl OmniboxState {
    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.caret = self.text.len();
        self.selection_anchor = None;
    }

    pub fn focus_and_select_all(&mut self) {
        self.focused = true;
        self.selection_anchor = Some(0);
        self.caret = self.text.len();
        self.dropdown_open = true;
        self.selected_suggestion = None;
    }

    pub fn blur(&mut self) {
        self.focused = false;
        self.selection_anchor = None;
        self.dropdown_open = false;
        self.selected_suggestion = None;
    }

    #[must_use]
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let lo = anchor.min(self.caret).min(self.text.len());
        let hi = anchor.max(self.caret).min(self.text.len());
        (lo < hi).then_some((lo, hi))
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        let (lo, hi) = self.selection_range()?;
        self.text.get(lo..hi)
    }

    pub fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.caret = self.text.len();
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((lo, hi)) = self.selection_range() else {
            return false;
        };
        self.text.replace_range(lo..hi, "");
        self.caret = lo;
        self.selection_anchor = None;
        true
    }

    pub fn insert_str(&mut self, input: &str) {
        let clean: String = input
            .chars()
            .filter(|c| !matches!(c, '\r' | '\n' | '\t'))
            .collect();
        if clean.is_empty() {
            return;
        }
        self.delete_selection();
        let pos = self.caret.min(self.text.len());
        self.text.insert_str(pos, &clean);
        self.caret = pos + clean.len();
        self.dropdown_open = true;
        self.selected_suggestion = None;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            self.dropdown_open = true;
            return;
        }
        if self.caret == 0 || self.text.is_empty() {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.caret);
        self.text.replace_range(prev..self.caret, "");
        self.caret = prev;
        self.dropdown_open = true;
        self.selected_suggestion = None;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.caret);
        self.text.replace_range(self.caret..next, "");
        self.dropdown_open = true;
    }

    pub fn move_left(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.caret);
            }
            self.caret = prev_char_boundary(&self.text, self.caret);
        } else if let Some((lo, _)) = self.selection_range() {
            self.caret = lo;
            self.selection_anchor = None;
        } else {
            self.caret = prev_char_boundary(&self.text, self.caret);
        }
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.caret);
            }
            self.caret = next_char_boundary(&self.text, self.caret);
        } else if let Some((_, hi)) = self.selection_range() {
            self.caret = hi;
            self.selection_anchor = None;
        } else {
            self.caret = next_char_boundary(&self.text, self.caret);
        }
    }

    pub fn cut_selection(&mut self) -> Option<String> {
        let selected = self.selected_text()?.to_string();
        self.delete_selection();
        Some(selected)
    }
}

/// Active right-click context menu overlay state.
#[derive(Debug, Clone, Default)]
pub struct ContextMenuState {
    pub visible: bool,
    pub x: f32,
    pub y: f32,
    pub link_url: Option<String>,
    pub has_selection: bool,
}

/// Closed tab snapshot kept for `Ctrl+Shift+T` restore.
#[derive(Debug, Clone)]
pub struct ClosedTabSnapshot {
    pub url: String,
    pub title: String,
}

/// Complete isolated Parent-Process Chrome UI state.
#[derive(Debug, Clone)]
pub struct ChromeState {
    pub theme: ThemeMode,
    pub tab_layout: TabLayoutMode,
    pub tabs: Vec<TabEntry>,
    pub active_tab: usize,
    pub closed_tabs: Vec<ClosedTabSnapshot>,
    pub omnibox: OmniboxState,
    pub history: Vec<HistoryEntry>,
    pub hovered_link: Option<String>,
    pub context_menu: ContextMenuState,
    pub hud_open: bool,
    pub focused_control: Option<String>,
    next_tab_id: u64,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChromeState {
    #[must_use]
    pub fn new() -> Self {
        let first = TabEntry::new(TabId::new(1), URL_NEWTAB);
        let mut omnibox = OmniboxState::default();
        omnibox.set_text(URL_NEWTAB);
        Self {
            theme: ThemeMode::Dark,
            tab_layout: TabLayoutMode::Horizontal,
            tabs: vec![first],
            active_tab: 0,
            closed_tabs: Vec::new(),
            omnibox,
            history: vec![
                HistoryEntry {
                    url: "https://en.wikipedia.org/wiki/Rust_(programming_language)".to_string(),
                    title: "Rust (programming language) - Wikipedia".to_string(),
                    visits: 3,
                },
                HistoryEntry {
                    url: "https://developer.mozilla.org/en-US/docs/Web/CSS".to_string(),
                    title: "CSS: Cascading Style Sheets - MDN".to_string(),
                    visits: 2,
                },
                HistoryEntry {
                    url: "https://doc.rust-lang.org/std/".to_string(),
                    title: "std - Rust Documentation".to_string(),
                    visits: 2,
                },
                HistoryEntry {
                    url: "https://news.ycombinator.com/".to_string(),
                    title: "Hacker News".to_string(),
                    visits: 1,
                },
            ],
            hovered_link: None,
            context_menu: ContextMenuState::default(),
            hud_open: false,
            focused_control: None,
            next_tab_id: 2,
        }
    }

    #[must_use]
    pub fn active(&self) -> &TabEntry {
        &self.tabs[self.active_tab.min(self.tabs.len().saturating_sub(1))]
    }

    pub fn active_mut(&mut self) -> &mut TabEntry {
        let idx = self.active_tab.min(self.tabs.len().saturating_sub(1));
        &mut self.tabs[idx]
    }

    /// Creates a new tab (`Ctrl+T`), switches to it, and returns its [`TabId`].
    pub fn new_tab(&mut self, url: impl Into<String>) -> TabId {
        let id = TabId::new(self.next_tab_id);
        self.next_tab_id += 1;
        let url_str = url.into();
        self.tabs.push(TabEntry::new(id, url_str.clone()));
        self.active_tab = self.tabs.len() - 1;
        self.omnibox.set_text(url_str);
        self.context_menu.visible = false;
        id
    }

    /// Closes tab at `index` (`Ctrl+W` / middle-click); opens a fresh New Tab if last tab closed.
    pub fn close_tab(&mut self, index: usize) -> Option<TabId> {
        if index >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(index);
        self.closed_tabs.push(ClosedTabSnapshot {
            url: removed.url,
            title: removed.title,
        });
        if self.closed_tabs.len() > 32 {
            self.closed_tabs.remove(0);
        }
        if self.tabs.is_empty() {
            self.new_tab(URL_NEWTAB);
        } else {
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
            let active_url = self.active().url.clone();
            self.omnibox.set_text(active_url);
        }
        self.context_menu.visible = false;
        Some(removed.id)
    }

    /// Reopens the most recently closed tab (`Ctrl+Shift+T`).
    pub fn reopen_closed_tab(&mut self) -> Option<TabId> {
        let snap = self.closed_tabs.pop()?;
        let id = self.new_tab(snap.url);
        self.active_mut().title = snap.title;
        Some(id)
    }

    /// Switches active tab by index (`Ctrl+1..9` or tab click).
    pub fn switch_to_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || index == self.active_tab {
            return false;
        }
        self.active_tab = index;
        let url = self.active().url.clone();
        self.omnibox.set_text(url);
        self.omnibox.blur();
        self.context_menu.visible = false;
        true
    }

    /// Cycles to the next tab (`Ctrl+Tab`) or previous tab (`Ctrl+Shift+Tab`).
    pub fn cycle_tab(&mut self, reverse: bool) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        let next = if reverse {
            if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            }
        } else {
            (self.active_tab + 1) % self.tabs.len()
        };
        self.switch_to_tab(next)
    }

    /// Toggles between horizontal top tab bar and vertical left tab sidebar.
    pub fn toggle_tab_layout(&mut self) {
        self.tab_layout = match self.tab_layout {
            TabLayoutMode::Horizontal => TabLayoutMode::Vertical,
            TabLayoutMode::Vertical => TabLayoutMode::Horizontal,
        };
    }

    pub fn toggle_theme(&mut self) {
        self.theme = self.theme.toggled();
    }

    pub fn toggle_hud(&mut self) {
        self.hud_open = !self.hud_open;
    }

    /// Navigates the active tab to a raw omnibox string (auto-resolving URL vs search).
    pub fn navigate_active(&mut self, raw_input: &str) -> String {
        let resolved = resolve_omnibox_input(raw_input);
        self.active_mut().navigate_to(resolved.clone());
        self.omnibox.set_text(resolved.clone());
        self.omnibox.blur();
        self.context_menu.visible = false;
        self.record_history(&resolved, &default_title_for_url(&resolved));
        resolved
    }

    pub fn go_back(&mut self) -> Option<String> {
        let tab = self.active_mut();
        if !tab.can_go_back() {
            return None;
        }
        tab.history_index -= 1;
        let url = tab.history[tab.history_index].clone();
        tab.url = url.clone();
        tab.title = default_title_for_url(&url);
        tab.crashed_reason = None;
        tab.error_message = None;
        self.omnibox.set_text(url.clone());
        Some(url)
    }

    pub fn go_forward(&mut self) -> Option<String> {
        let tab = self.active_mut();
        if !tab.can_go_forward() {
            return None;
        }
        tab.history_index += 1;
        let url = tab.history[tab.history_index].clone();
        tab.url = url.clone();
        tab.title = default_title_for_url(&url);
        tab.crashed_reason = None;
        tab.error_message = None;
        self.omnibox.set_text(url.clone());
        Some(url)
    }

    /// Marks the tab associated with `tab_id` as crashed, preserving its URL for Reload.
    pub fn mark_tab_crashed(&mut self, tab_id: TabId, reason: impl Into<String>) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
            return false;
        };
        tab.is_loading = false;
        tab.load_progress = 0.0;
        tab.crashed_reason = Some(reason.into());
        true
    }

    pub fn record_history(&mut self, url: &str, title: &str) {
        if url.starts_with("rix://chrome/") {
            return;
        }
        if let Some(entry) = self.history.iter_mut().find(|h| h.url == url) {
            entry.visits = entry.visits.saturating_add(1);
            if !title.is_empty() {
                entry.title = title.to_string();
            }
        } else {
            self.history.push(HistoryEntry {
                url: url.to_string(),
                title: title.to_string(),
                visits: 1,
            });
        }
        self.history.sort_by_key(|h| std::cmp::Reverse(h.visits));
        if self.history.len() > 256 {
            self.history.truncate(256);
        }
    }

    /// Returns ranked omnibox autocomplete suggestions matching the current omnibox input.
    #[must_use]
    pub fn autocomplete_suggestions(&self, limit: usize) -> Vec<AutocompleteItem> {
        let query = self.omnibox.text.trim();
        let lower = query.to_ascii_lowercase();
        let mut out = Vec::new();
        for entry in &self.history {
            if lower.is_empty()
                || entry.url.to_ascii_lowercase().contains(&lower)
                || entry.title.to_ascii_lowercase().contains(&lower)
            {
                out.push(AutocompleteItem {
                    url: entry.url.clone(),
                    title: entry.title.clone(),
                    is_search: false,
                });
                if out.len() >= limit.saturating_sub(1).max(1) {
                    break;
                }
            }
        }
        if !query.is_empty() && !looks_like_url(query) && out.len() < limit {
            out.push(AutocompleteItem {
                url: resolve_omnibox_input(query),
                title: format!("Search for {query}"),
                is_search: true,
            });
        }
        out
    }

    /// Height in logical pixels consumed by top Chrome UI bars.
    #[must_use]
    pub const fn top_chrome_height(&self) -> f32 {
        match self.tab_layout {
            TabLayoutMode::Horizontal => 85.0,
            TabLayoutMode::Vertical => 45.0,
        }
    }

    /// Width in logical pixels consumed by vertical tab sidebar (0 in horizontal mode).
    #[must_use]
    pub const fn sidebar_width(&self) -> f32 {
        match self.tab_layout {
            TabLayoutMode::Horizontal => 0.0,
            TabLayoutMode::Vertical => 224.0,
        }
    }
}

/// Resolves raw omnibox text into a concrete navigable URL or search query URL.
#[must_use]
pub fn resolve_omnibox_input(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return URL_NEWTAB.to_string();
    }
    if trimmed.starts_with("rix://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
        || trimmed.starts_with("about:")
    {
        return trimmed.to_string();
    }
    if !trimmed.contains(char::is_whitespace) {
        if trimmed.starts_with("localhost")
            || trimmed.starts_with("127.0.0.1")
            || trimmed.starts_with("[::1]")
        {
            return format!("http://{trimmed}");
        }
        if looks_like_url(trimmed) {
            return format!("https://{trimmed}");
        }
    }
    let encoded = percent_encode_query(trimmed);
    format!("https://duckduckgo.com/?q={encoded}")
}

#[must_use]
pub fn looks_like_url(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return false;
    }
    if Url::parse(trimmed).is_ok() && (trimmed.contains("://") || trimmed.starts_with("about:")) {
        return true;
    }
    if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
        return true;
    }
    let host_part = trimmed.split('/').next().unwrap_or("");
    let host_only = host_part.split(':').next().unwrap_or("");
    host_only.contains('.')
        && !host_only.starts_with('.')
        && !host_only.ends_with('.')
        && host_only
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

#[must_use]
pub fn default_title_for_url(url: &str) -> String {
    match url {
        URL_NEWTAB => "New tab".to_string(),
        URL_CRASHED => "Tab crashed".to_string(),
        URL_ERROR => "Connection error".to_string(),
        other => Url::parse(other)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| other.to_string()),
    }
}

fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 {
        i -= 1;
        if s.is_char_boundary(i) {
            return i;
        }
    }
    0
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() {
        i += 1;
        if s.is_char_boundary(i) {
            return i;
        }
    }
    s.len()
}
