//! Parent-process [`BrowserController`] coordinating isolated Chrome UI (`z = 100`),
//! active web tab surfaces (`z = 10`), transient overlays & `F12` Developer HUD (`z = 200`),
//! real HTTP/HTTPS GET & POST navigation via `rix-net`, in-page form interaction,
//! OS clipboard, and Content Process crash recovery.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rix_chrome::{
    chrome_asset_provider, chrome_stylesheet, find_link_href, palette_for, render_crashed_html,
    render_error_html, render_newtab_html, ChromeHitTarget, ChromeRenderer, ChromeState,
    DevHudState, FrameStageSample, NetworkEntry, RenderedChromeLayer, ThemeMode, URL_CRASHED,
    URL_ERROR, URL_NEWTAB,
};
use rix_core::{Point, Rect, RixUrl, Size, TabId, Vec2};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_ipc::{PipelineId, ScrollId, SpatialNodeId};
use rix_layout::{hit_test, FragmentTree, LayoutEngine};
use rix_net::{HttpClient, ResourceManager};
use rix_render::{Color, Compositor, DisplayItem, DisplayList, LayerRole, SpatialNodeKind};
use rix_text::FontDatabase;

use crate::forms::{
    collect_element_text, resolve_relative_url, ClipboardBridge, FormMethod, FormSubmission,
    InPageInteractionState, PageClickOutcome,
};

pub const WEB_PIPELINE: PipelineId = PipelineId(1);
pub const CHROME_PIPELINE: PipelineId = PipelineId(2);
pub const OVERLAY_PIPELINE: PipelineId = PipelineId(3);

/// Modifier key snapshot decoupled from windowing backends for deterministic testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// High-level semantic key codes handled by [`BrowserController::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKey {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    F12,
}

/// Per-tab web document, style context, layout engine, and form interaction state.
pub struct TabDocumentState {
    pub doc: Document,
    pub styles: StyleContext,
    pub layout: LayoutEngine,
    pub fragment_tree: FragmentTree,
    pub interaction: InPageInteractionState,
    pub raw_html: String,
    pub page_css: Vec<String>,
    pub scroll_id: ScrollId,
}

impl TabDocumentState {
    fn new(tab_id: TabId, viewport: Size, dark_mode: bool) -> Self {
        let media = MediaContext {
            viewport_width: viewport.width,
            viewport_height: viewport.height,
            dark_mode,
            ..MediaContext::default()
        };
        Self {
            doc: Document::new(),
            styles: StyleContext::new(media),
            layout: LayoutEngine::new(),
            fragment_tree: FragmentTree::default(),
            interaction: InPageInteractionState::default(),
            raw_html: String::new(),
            page_css: Vec::new(),
            scroll_id: ScrollId(100 + tab_id.0),
        }
    }
}

/// Complete parent-process browser controller driving `rix-chrome`, `rix-layout`,
/// `rix-net`, and `rix-render::Compositor`.
pub struct BrowserController {
    pub chrome: ChromeState,
    pub hud: DevHudState,
    pub clipboard: ClipboardBridge,
    pub compositor: Compositor,
    chrome_renderer: ChromeRenderer,
    font_db: FontDatabase,
    http_client: HttpClient,
    resource_manager: ResourceManager,
    runtime: tokio::runtime::Runtime,
    tabs_doc: HashMap<TabId, TabDocumentState>,
    last_chrome_layer: Option<RenderedChromeLayer>,
    last_overlay_layer: Option<RenderedChromeLayer>,
    chrome_dirty: bool,
    overlay_dirty: bool,
    web_dirty: bool,
    frame_seq: u64,
}

impl BrowserController {
    #[must_use]
    pub fn new(viewport: Size, dpr: f32) -> Self {
        let mut font_db = FontDatabase::new();
        font_db.scan_system_fonts();

        let http_client = HttpClient::new(None).expect("default HTTP client initializes");
        let resource_manager = ResourceManager::new(http_client.clone())
            .with_chrome_asset_provider(chrome_asset_provider());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("browser network runtime initializes");

        let chrome = ChromeState::new();
        let initial_tab_id = chrome.active().id;
        let mut tabs_doc = HashMap::new();
        tabs_doc.insert(
            initial_tab_id,
            TabDocumentState::new(initial_tab_id, viewport, chrome.theme.is_dark()),
        );

        let mut controller = Self {
            chrome,
            hud: DevHudState::new(),
            clipboard: ClipboardBridge::new(),
            compositor: Compositor::new(viewport, dpr),
            chrome_renderer: ChromeRenderer::new(),
            font_db,
            http_client,
            resource_manager,
            runtime,
            tabs_doc,
            last_chrome_layer: None,
            last_overlay_layer: None,
            chrome_dirty: true,
            overlay_dirty: true,
            web_dirty: true,
            frame_seq: 1,
        };
        controller.load_internal_newtab(initial_tab_id);
        controller.sync_layers();
        controller
    }

    #[must_use]
    pub fn clear_color(&self) -> Color {
        let rgba = palette_for(self.chrome.theme).clear_rgba;
        Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3])
    }

    #[must_use]
    pub fn web_viewport_rect(&self) -> Rect {
        let vp = self.compositor.viewport_size();
        let x = self.chrome.sidebar_width();
        let y = self.chrome.top_chrome_height();
        Rect::new(x, y, (vp.width - x).max(64.0), (vp.height - y).max(64.0))
    }

    #[must_use]
    pub fn active_tab_doc(&self) -> Option<&TabDocumentState> {
        self.tabs_doc.get(&self.chrome.active().id)
    }

    #[must_use]
    pub fn resource_manager(&self) -> &ResourceManager {
        &self.resource_manager
    }

    pub fn set_viewport(&mut self, logical: Size, dpr: f32) -> bool {
        if self.compositor.set_viewport(logical, dpr) {
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.web_dirty = true;
            self.sync_layers();
            true
        } else {
            false
        }
    }

    /// Synchronizes only dirty layers (`z = 10` web, `z = 100` chrome, `z = 200` overlay/HUD)
    /// into [`Compositor`], keeping Chrome UI updates independent from Web content layers.
    pub fn sync_layers(&mut self) {
        let vp = self.compositor.viewport_size();
        let mut style_ms = 0.0f32;
        let mut layout_ms = 0.0f32;
        let mut paint_ms = 0.0f32;
        let total_start = Instant::now();

        if self.web_dirty {
            let web_rect = self.web_viewport_rect();
            let active_id = self.chrome.active().id;
            let dark_mode = self.chrome.theme.is_dark();
            let scroll_y = self.chrome.active().scroll_y;
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);

            if let Some(tab_doc) = self.tabs_doc.get_mut(&active_id) {
                let t_style = Instant::now();
                let media = MediaContext {
                    viewport_width: web_rect.size.width,
                    viewport_height: web_rect.size.height,
                    dark_mode,
                    ..MediaContext::default()
                };
                tab_doc.styles.set_media(media);
                let _ = tab_doc.styles.restyle_dirty(&mut tab_doc.doc);
                style_ms += t_style.elapsed().as_secs_f32() * 1000.0;

                tab_doc.layout.dark_mode = dark_mode;
                let frame = tab_doc.layout.layout_and_paint(
                    &tab_doc.doc,
                    &tab_doc.styles,
                    web_rect.size,
                    &self.font_db,
                    WEB_PIPELINE,
                    &mut self.compositor.resources,
                );
                layout_ms += frame.metrics.box_tree_ms + frame.metrics.layout_ms;
                paint_ms += frame.metrics.paint_ms;
                tab_doc.fragment_tree = frame.fragment_tree;

                let wrapped = wrap_web_display_list(
                    frame.display_list,
                    web_rect,
                    tab_doc.scroll_id,
                    scroll_y,
                );
                self.compositor.submit_display_list(
                    WEB_PIPELINE,
                    LayerRole::WebSurface,
                    seq,
                    wrapped,
                );
                self.hud
                    .ingest_css_missing_features(tab_doc.styles.missing_features());
            }
            self.web_dirty = false;
        }

        if self.chrome_dirty {
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);
            let layer = self.chrome_renderer.render_chrome(
                &self.chrome,
                vp,
                &self.font_db,
                CHROME_PIPELINE,
                &mut self.compositor.resources,
            );
            self.compositor.submit_display_list(
                CHROME_PIPELINE,
                LayerRole::Chrome,
                seq,
                layer.display_list.clone(),
            );
            self.last_chrome_layer = Some(layer);
            self.chrome_dirty = false;
        }

        if self.overlay_dirty {
            let now = Instant::now();
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);
            let layer = self.chrome_renderer.render_overlay(
                &self.chrome,
                &self.hud,
                now,
                vp,
                &self.font_db,
                OVERLAY_PIPELINE,
                &mut self.compositor.resources,
            );
            self.compositor.submit_display_list(
                OVERLAY_PIPELINE,
                LayerRole::Overlay,
                seq,
                layer.display_list.clone(),
            );
            self.last_overlay_layer = Some(layer);
            self.overlay_dirty = false;
        }

        let total_ms = total_start.elapsed().as_secs_f32() * 1000.0;
        if style_ms > 0.0 || layout_ms > 0.0 || paint_ms > 0.0 || total_ms > 0.05 {
            self.hud.record_frame(FrameStageSample {
                style_ms,
                layout_ms,
                paint_ms,
                ipc_ms: 0.05,
                gpu_ms: 0.0,
                total_ms,
            });
        }
    }

    /// Loads HTML into the given tab, compiles `<style>` tags (and chrome theme CSS for internal
    /// pages), performs full style cascade, and marks the web surface dirty.
    pub fn load_html_into_tab(
        &mut self,
        tab_id: TabId,
        url: &str,
        html: &str,
        is_chrome_page: bool,
    ) {
        let web_rect = self.web_viewport_rect();
        let dark_mode = self.chrome.theme.is_dark();
        let media = MediaContext {
            viewport_width: web_rect.size.width,
            viewport_height: web_rect.size.height,
            dark_mode,
            ..MediaContext::default()
        };

        let mut doc = parse_html(html);
        let page_title = extract_document_title(&doc);
        let inline_styles = extract_style_elements(&doc);

        let mut styles = StyleContext::new(media);
        if is_chrome_page {
            styles.add_stylesheet(self.chrome_renderer.stylesheet_for(self.chrome.theme));
        }
        for css in &inline_styles {
            styles.add_stylesheet(Arc::new(Stylesheet::parse(
                css,
                CascadeOrigin::Author,
                &media,
            )));
        }
        styles.cascade_full(&mut doc);

        if let Some(tab) = self.chrome.tabs.iter_mut().find(|t| t.id == tab_id) {
            if let Some(title) = page_title {
                tab.title = title;
            }
            tab.is_loading = false;
            tab.load_progress = 1.0;
            tab.scroll_y = 0.0;
        }
        self.chrome.record_history(
            url,
            &self
                .chrome
                .tabs
                .iter()
                .find(|t| t.id == tab_id)
                .map_or_else(String::new, |t| t.title.clone()),
        );

        let entry = self
            .tabs_doc
            .entry(tab_id)
            .or_insert_with(|| TabDocumentState::new(tab_id, web_rect.size, dark_mode));
        entry.doc = doc;
        entry.styles = styles;
        entry.layout.clear_caches();
        entry.interaction.clear();
        entry.raw_html = html.to_string();
        entry.page_css = inline_styles;

        self.hud.clear_page_telemetry();
        self.hud
            .ingest_css_missing_features(entry.styles.missing_features());
        self.web_dirty = true;
        self.chrome_dirty = true;
        if self.chrome.hud_open {
            self.overlay_dirty = true;
        }
    }

    fn load_internal_newtab(&mut self, tab_id: TabId) {
        let html = render_newtab_html(&self.chrome);
        self.load_html_into_tab(tab_id, URL_NEWTAB, &html, true);
    }

    /// Navigates the active tab to a URL (HTTP/HTTPS, `rix://chrome/*`, `file://`, or search).
    pub fn navigate_active(&mut self, raw_input: &str) {
        let target_url = self.chrome.navigate_active(raw_input);
        let tab_id = self.chrome.active().id;
        self.fetch_and_commit(tab_id, &target_url, FormMethod::Get, None, Vec::new());
    }

    /// Submits an HTML `<form>` via GET or POST and renders the server response.
    pub fn submit_form(&mut self, submission: FormSubmission) {
        let tab_id = self.chrome.active().id;
        self.chrome
            .active_mut()
            .navigate_to(submission.action_url.clone());
        self.chrome.omnibox.set_text(submission.action_url.clone());
        self.chrome.omnibox.blur();
        self.fetch_and_commit(
            tab_id,
            &submission.action_url,
            submission.method,
            submission.content_type.as_deref(),
            submission.body,
        );
    }

    /// Reloads the active tab (recovering cleanly if the tab previously crashed or errored).
    pub fn reload_active_tab(&mut self) {
        let tab_id = self.chrome.active().id;
        let url = self.chrome.active().url.clone();
        self.chrome.active_mut().crashed_reason = None;
        self.chrome.active_mut().error_message = None;
        self.fetch_and_commit(tab_id, &url, FormMethod::Get, None, Vec::new());
    }

    /// Handles a Content Process crash (`SIGKILL` / unexpected EOF): isolates failure to the
    /// affected tab by rendering `rix://chrome/crashed.html` while Chrome UI stays live.
    pub fn handle_content_process_crash(
        &mut self,
        tab_id: TabId,
        pid: Option<u32>,
        reason: impl Into<String>,
    ) {
        let reason_str = reason.into();
        if !self.chrome.mark_tab_crashed(tab_id, reason_str.clone()) {
            return;
        }
        let url = self
            .chrome
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map_or_else(|| URL_CRASHED.to_string(), |t| t.url.clone());
        let html = render_crashed_html(self.chrome.theme, &url, pid, &reason_str);
        self.load_html_into_tab(tab_id, URL_CRASHED, &html, true);
        if let Some(tab) = self.chrome.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.url = url;
            tab.title = "Tab crashed".to_string();
            tab.crashed_reason = Some(reason_str);
        }
        self.sync_layers();
    }

    fn fetch_and_commit(
        &mut self,
        tab_id: TabId,
        url: &str,
        method: FormMethod,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) {
        if url == URL_NEWTAB {
            self.load_internal_newtab(tab_id);
            self.sync_layers();
            return;
        }
        if url.starts_with("rix://chrome/") {
            let start = Instant::now();
            let parsed = RixUrl::parse(url);
            match parsed
                .ok()
                .and_then(|u| self.runtime.block_on(self.resource_manager.load(&u)).ok())
            {
                Some(res) => {
                    let html = String::from_utf8_lossy(&res.data).to_string();
                    self.hud.record_network(NetworkEntry {
                        url: url.to_string(),
                        method: "GET".to_string(),
                        protocol: "rix".to_string(),
                        status: 200,
                        mime: res.mime_type,
                        bytes: res.data.len(),
                        cache_hit: true,
                        ttfb_ms: start.elapsed().as_secs_f32() * 1000.0,
                        total_ms: start.elapsed().as_secs_f32() * 1000.0,
                    });
                    self.load_html_into_tab(tab_id, url, &html, true);
                }
                None => {
                    let html = render_error_html(
                        self.chrome.theme,
                        url,
                        "Internal chrome resource not found",
                    );
                    self.load_html_into_tab(tab_id, URL_ERROR, &html, true);
                }
            }
            self.sync_layers();
            return;
        }

        if let Some(path) = url.strip_prefix("file://") {
            let start = Instant::now();
            match std::fs::read_to_string(path) {
                Ok(html) => {
                    self.hud.record_network(NetworkEntry {
                        url: url.to_string(),
                        method: "GET".to_string(),
                        protocol: "file".to_string(),
                        status: 200,
                        mime: "text/html".to_string(),
                        bytes: html.len(),
                        cache_hit: false,
                        ttfb_ms: start.elapsed().as_secs_f32() * 1000.0,
                        total_ms: start.elapsed().as_secs_f32() * 1000.0,
                    });
                    self.load_html_into_tab(tab_id, url, &html, false);
                }
                Err(err) => {
                    let html = render_error_html(self.chrome.theme, url, &err.to_string());
                    self.load_html_into_tab(tab_id, URL_ERROR, &html, true);
                }
            }
            self.sync_layers();
            return;
        }

        let start = Instant::now();
        let Ok(parsed_url) = RixUrl::parse(url) else {
            let html = render_error_html(self.chrome.theme, url, "Malformed URL");
            self.load_html_into_tab(tab_id, URL_ERROR, &html, true);
            self.sync_layers();
            return;
        };

        let mut headers = HashMap::new();
        if let Some(ct) = content_type {
            headers.insert("Content-Type".to_string(), ct.to_string());
        }
        let method_str = match method {
            FormMethod::Get => "GET",
            FormMethod::Post => "POST",
        };
        let resp_res = self.runtime.block_on(self.http_client.request(
            &parsed_url,
            method_str,
            &headers,
            body,
        ));
        let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;

        match resp_res {
            Ok(resp) => {
                let final_url_str = resp.final_url.as_str().to_string();
                let mime = if resp.content_type.is_empty() {
                    "text/html".to_string()
                } else {
                    resp.content_type.clone()
                };
                let html = String::from_utf8_lossy(&resp.body).to_string();
                self.hud.record_network(NetworkEntry {
                    url: final_url_str.clone(),
                    method: method_str.to_string(),
                    protocol: format!("{:?}", resp.protocol),
                    status: resp.status,
                    mime,
                    bytes: resp.body.len(),
                    cache_hit: false,
                    ttfb_ms: elapsed_ms,
                    total_ms: elapsed_ms,
                });
                self.load_html_into_tab(tab_id, &final_url_str, &html, false);
            }
            Err(err) => {
                self.hud.record_network(NetworkEntry {
                    url: url.to_string(),
                    method: method_str.to_string(),
                    protocol: "net".to_string(),
                    status: 0,
                    mime: "text/plain".to_string(),
                    bytes: 0,
                    cache_hit: false,
                    ttfb_ms: elapsed_ms,
                    total_ms: elapsed_ms,
                });
                let html = render_error_html(self.chrome.theme, url, &err.to_string());
                self.load_html_into_tab(tab_id, URL_ERROR, &html, true);
            }
        }
        self.sync_layers();
    }

    /// Handles left mouse click across Overlay -> Chrome -> Web Content layers.
    pub fn handle_left_click(&mut self, point: Point) -> bool {
        // 1. Overlay layer hit-testing (omnibox dropdown, context menu, HUD tabs).
        if let Some(target) = self
            .last_overlay_layer
            .as_ref()
            .and_then(|l| l.hit_test_target(point))
        {
            return self.activate_chrome_target(target);
        }

        // Dismiss open context menu if click landed elsewhere.
        if self.chrome.context_menu.visible {
            self.chrome.context_menu.visible = false;
            self.overlay_dirty = true;
        }

        // 2. Chrome UI layer hit-testing (tabs, toolbar buttons, omnibox, sidebar).
        if let Some(target) = self
            .last_chrome_layer
            .as_ref()
            .and_then(|l| l.hit_test_target(point))
        {
            return self.activate_chrome_target(target);
        }

        // 3. Web surface hit-testing (links, form controls, crash/error page buttons, text selection).
        let web_rect = self.web_viewport_rect();
        if !web_rect.contains(point) {
            return false;
        }
        if self.chrome.omnibox.focused {
            self.chrome.omnibox.blur();
            self.chrome_dirty = true;
            self.overlay_dirty = true;
        }

        let active_id = self.chrome.active().id;
        let base_url = self.chrome.active().url.clone();
        let scroll_y = self.chrome.active().scroll_y;
        let doc_pt = Point::new(
            point.x - web_rect.min_x(),
            point.y - web_rect.min_y() + scroll_y,
        );

        let hit_node = self
            .tabs_doc
            .get(&active_id)
            .and_then(|t| hit_test(&t.fragment_tree, doc_pt));
        let Some(node) = hit_node else {
            self.sync_layers();
            return false;
        };

        // Check internal chrome page action buttons (`Reload tab`, `Try again`, `Open new tab`).
        if let Some(target) = self.tabs_doc.get(&active_id).and_then(|t| {
            let mut cur = Some(node);
            while let Some(n) = cur {
                if let Some(NodeData::Element(e)) = t.doc.get(n) {
                    if let Some(id) = e.id() {
                        if let Some(hit) = rix_chrome::parse_chrome_hit_id(id) {
                            return Some(hit);
                        }
                    }
                }
                cur = t.doc.parent(n);
            }
            None
        }) {
            return self.activate_chrome_target(target);
        }

        let outcome = match self.tabs_doc.get_mut(&active_id) {
            Some(t) => t.interaction.handle_click(&mut t.doc, node, &base_url),
            None => PageClickOutcome::None,
        };
        match outcome {
            PageClickOutcome::Navigate(url) => {
                self.navigate_active(&url);
                true
            }
            PageClickOutcome::SubmitForm(sub) => {
                self.submit_form(sub);
                true
            }
            PageClickOutcome::ControlUpdated => {
                self.web_dirty = true;
                self.sync_layers();
                true
            }
            PageClickOutcome::SelectedText(_) => {
                self.sync_layers();
                true
            }
            PageClickOutcome::None => {
                self.sync_layers();
                false
            }
        }
    }

    /// Handles middle-click (closes hovered tab in tab strip).
    pub fn handle_middle_click(&mut self, point: Point) -> bool {
        if let Some(ChromeHitTarget::Tab(idx)) = self
            .last_chrome_layer
            .as_ref()
            .and_then(|l| l.hit_test_target(point))
        {
            return self.close_tab_at(idx);
        }
        false
    }

    /// Handles right-click (opens clean context menu overlay with link/selection actions).
    pub fn handle_right_click(&mut self, point: Point) -> bool {
        let web_rect = self.web_viewport_rect();
        let mut link_url = None;
        let mut has_selection = false;
        if web_rect.contains(point) {
            let active_id = self.chrome.active().id;
            let base_url = self.chrome.active().url.clone();
            let scroll_y = self.chrome.active().scroll_y;
            let doc_pt = Point::new(
                point.x - web_rect.min_x(),
                point.y - web_rect.min_y() + scroll_y,
            );
            if let Some(t) = self.tabs_doc.get(&active_id) {
                has_selection = !t.interaction.selected_text.is_empty();
                if let Some(hit) = hit_test(&t.fragment_tree, doc_pt) {
                    if let Some(href) = find_link_href(&t.doc, hit) {
                        link_url = resolve_relative_url(&base_url, &href);
                    }
                }
            }
        }
        self.chrome.context_menu.visible = true;
        self.chrome.context_menu.x = point.x;
        self.chrome.context_menu.y = point.y;
        self.chrome.context_menu.link_url = link_url;
        self.chrome.context_menu.has_selection = has_selection;
        self.overlay_dirty = true;
        self.sync_layers();
        true
    }

    /// Updates bottom-left hover URL preview pill when pointer moves over `<a href>` links.
    pub fn handle_hover(&mut self, point: Point) -> bool {
        let web_rect = self.web_viewport_rect();
        let next_hover = if web_rect.contains(point) {
            let active_id = self.chrome.active().id;
            let base_url = self.chrome.active().url.clone();
            let scroll_y = self.chrome.active().scroll_y;
            let doc_pt = Point::new(
                point.x - web_rect.min_x(),
                point.y - web_rect.min_y() + scroll_y,
            );
            self.tabs_doc.get(&active_id).and_then(|t| {
                let hit = hit_test(&t.fragment_tree, doc_pt)?;
                let href = find_link_href(&t.doc, hit)?;
                resolve_relative_url(&base_url, &href)
            })
        } else {
            None
        };
        if next_hover != self.chrome.hovered_link {
            self.chrome.hovered_link = next_hover;
            self.overlay_dirty = true;
            self.sync_layers();
            true
        } else {
            false
        }
    }

    /// Smooth APZ wheel scroll routed directly to [`Compositor::handle_wheel`].
    pub fn handle_wheel(&mut self, point: Point, dx: f32, dy: f32) -> bool {
        if self.compositor.handle_wheel(point, dx, dy) {
            for msg in self.compositor.drain_scroll_syncs() {
                if let rix_ipc::CompositorToContent::ScrollOffsetSync { offset, .. } = msg {
                    self.chrome.active_mut().scroll_y = offset.y.max(0.0);
                }
            }
            true
        } else {
            false
        }
    }

    /// Handles keyboard shortcuts and navigation/editing keys.
    pub fn handle_key(&mut self, mods: KeyModifiers, key: BrowserKey) -> bool {
        // Global F12 / Ctrl+Shift+I toggles the Developer HUD.
        if key == BrowserKey::F12
            || (mods.ctrl && mods.shift && matches!(key, BrowserKey::Char('i' | 'I')))
        {
            self.chrome.toggle_hud();
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }

        // Alt+Left / Alt+Right Back & Forward.
        if mods.alt && key == BrowserKey::ArrowLeft {
            if let Some(url) = self.chrome.go_back() {
                let id = self.chrome.active().id;
                self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
            }
            return true;
        }
        if mods.alt && key == BrowserKey::ArrowRight {
            if let Some(url) = self.chrome.go_forward() {
                let id = self.chrome.active().id;
                self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
            }
            return true;
        }

        if mods.ctrl {
            match key {
                BrowserKey::Char('t' | 'T') if mods.shift => {
                    if let Some(id) = self.chrome.reopen_closed_tab() {
                        let url = self.chrome.active().url.clone();
                        self.ensure_tab_doc(id);
                        self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
                    }
                    return true;
                }
                BrowserKey::Char('t' | 'T') => {
                    let id = self.chrome.new_tab(URL_NEWTAB);
                    self.ensure_tab_doc(id);
                    self.load_internal_newtab(id);
                    self.chrome.omnibox.focus_and_select_all();
                    self.chrome_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('w' | 'W') => {
                    let idx = self.chrome.active_tab;
                    return self.close_tab_at(idx);
                }
                BrowserKey::Tab => {
                    if self.chrome.cycle_tab(mods.shift) {
                        self.chrome_dirty = true;
                        self.web_dirty = true;
                        self.overlay_dirty = true;
                        self.sync_layers();
                    }
                    return true;
                }
                BrowserKey::Char(d @ '1'..='9') => {
                    let idx = (d as u8 - b'1') as usize;
                    if self.chrome.switch_to_tab(idx) {
                        self.chrome_dirty = true;
                        self.web_dirty = true;
                        self.overlay_dirty = true;
                        self.sync_layers();
                    }
                    return true;
                }
                BrowserKey::Char('l' | 'L') => {
                    self.chrome.omnibox.focus_and_select_all();
                    self.chrome_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('r' | 'R') => {
                    self.reload_active_tab();
                    return true;
                }
                BrowserKey::Char('a' | 'A') => {
                    if self.chrome.omnibox.focused {
                        self.chrome.omnibox.select_all();
                        self.chrome_dirty = true;
                        self.sync_layers();
                        return true;
                    }
                    let active_id = self.chrome.active().id;
                    if let Some(t) = self.tabs_doc.get_mut(&active_id) {
                        let root = t.doc.root();
                        t.interaction.selected_text = collect_element_text(&t.doc, root);
                        return true;
                    }
                }
                BrowserKey::Char('c' | 'C') => {
                    if self.chrome.omnibox.focused {
                        let text = self
                            .chrome
                            .omnibox
                            .selected_text()
                            .unwrap_or(&self.chrome.omnibox.text)
                            .to_string();
                        self.clipboard.set_text(text);
                        return true;
                    }
                    let active_id = self.chrome.active().id;
                    if let Some(t) = self.tabs_doc.get(&active_id) {
                        if !t.interaction.selected_text.is_empty() {
                            self.clipboard.set_text(t.interaction.selected_text.clone());
                            return true;
                        }
                    }
                }
                BrowserKey::Char('x' | 'X') => {
                    if self.chrome.omnibox.focused {
                        if let Some(cut) = self.chrome.omnibox.cut_selection() {
                            self.clipboard.set_text(cut);
                            self.chrome_dirty = true;
                            self.overlay_dirty = true;
                            self.sync_layers();
                            return true;
                        }
                    }
                }
                BrowserKey::Char('v' | 'V') => {
                    let pasted = self.clipboard.get_text();
                    if !pasted.is_empty() {
                        return self.handle_text_input(&pasted);
                    }
                }
                _ => {}
            }
        }

        if key == BrowserKey::Escape {
            self.chrome.omnibox.blur();
            self.chrome.context_menu.visible = false;
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }

        if self.chrome.omnibox.focused {
            match key {
                BrowserKey::Enter => {
                    let raw = if let Some(idx) = self.chrome.omnibox.selected_suggestion {
                        let suggestions = self.chrome.autocomplete_suggestions(6);
                        suggestions
                            .get(idx)
                            .map_or_else(|| self.chrome.omnibox.text.clone(), |s| s.url.clone())
                    } else {
                        self.chrome.omnibox.text.clone()
                    };
                    self.navigate_active(&raw);
                    return true;
                }
                BrowserKey::Backspace => {
                    self.chrome.omnibox.backspace();
                    self.chrome_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Delete => {
                    self.chrome.omnibox.delete_forward();
                    self.chrome_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::ArrowLeft => {
                    self.chrome.omnibox.move_left(mods.shift);
                    self.chrome_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::ArrowRight => {
                    self.chrome.omnibox.move_right(mods.shift);
                    self.chrome_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::ArrowDown => {
                    let len = self.chrome.autocomplete_suggestions(6).len();
                    if len > 0 {
                        let next = self
                            .chrome
                            .omnibox
                            .selected_suggestion
                            .map_or(0, |i| (i + 1) % len);
                        self.chrome.omnibox.selected_suggestion = Some(next);
                        self.overlay_dirty = true;
                        self.sync_layers();
                    }
                    return true;
                }
                BrowserKey::ArrowUp => {
                    let len = self.chrome.autocomplete_suggestions(6).len();
                    if len > 0 {
                        let prev = match self.chrome.omnibox.selected_suggestion {
                            None | Some(0) => len - 1,
                            Some(i) => i - 1,
                        };
                        self.chrome.omnibox.selected_suggestion = Some(prev);
                        self.overlay_dirty = true;
                        self.sync_layers();
                    }
                    return true;
                }
                _ => {}
            }
        } else {
            // Forward Backspace / Enter to focused in-page form controls.
            let active_id = self.chrome.active().id;
            let base_url = self.chrome.active().url.clone();
            match key {
                BrowserKey::Backspace => {
                    if let Some(t) = self.tabs_doc.get_mut(&active_id) {
                        if t.interaction.backspace(&mut t.doc) {
                            self.web_dirty = true;
                            self.sync_layers();
                            return true;
                        }
                    }
                }
                BrowserKey::Enter => {
                    let outcome = self
                        .tabs_doc
                        .get_mut(&active_id)
                        .map_or(PageClickOutcome::None, |t| {
                            t.interaction.press_enter(&mut t.doc, &base_url)
                        });
                    match outcome {
                        PageClickOutcome::SubmitForm(sub) => {
                            self.submit_form(sub);
                            return true;
                        }
                        PageClickOutcome::ControlUpdated => {
                            self.web_dirty = true;
                            self.sync_layers();
                            return true;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        false
    }

    /// Routes typed characters into the focused omnibox or focused in-page `<input>`/`<textarea>`.
    pub fn handle_text_input(&mut self, text: &str) -> bool {
        if self.chrome.omnibox.focused {
            self.chrome.omnibox.insert_str(text);
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }
        let active_id = self.chrome.active().id;
        if let Some(t) = self.tabs_doc.get_mut(&active_id) {
            if t.interaction.insert_text(&mut t.doc, text) {
                self.web_dirty = true;
                self.sync_layers();
                return true;
            }
        }
        false
    }

    fn activate_chrome_target(&mut self, target: ChromeHitTarget) -> bool {
        match target {
            ChromeHitTarget::Tab(idx) => {
                if self.chrome.switch_to_tab(idx) {
                    self.chrome_dirty = true;
                    self.web_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                }
                true
            }
            ChromeHitTarget::CloseTab(idx) => self.close_tab_at(idx),
            ChromeHitTarget::NewTab | ChromeHitTarget::OpenNewTabFromCrashed => {
                let id = self.chrome.new_tab(URL_NEWTAB);
                self.ensure_tab_doc(id);
                self.load_internal_newtab(id);
                self.sync_layers();
                true
            }
            ChromeHitTarget::Back | ChromeHitTarget::ContextBack => {
                self.chrome.context_menu.visible = false;
                if let Some(url) = self.chrome.go_back() {
                    let id = self.chrome.active().id;
                    self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
                }
                true
            }
            ChromeHitTarget::Forward | ChromeHitTarget::ContextForward => {
                self.chrome.context_menu.visible = false;
                if let Some(url) = self.chrome.go_forward() {
                    let id = self.chrome.active().id;
                    self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
                }
                true
            }
            ChromeHitTarget::Reload
            | ChromeHitTarget::ContextReload
            | ChromeHitTarget::ReloadCrashedTab
            | ChromeHitTarget::RetryError => {
                self.chrome.context_menu.visible = false;
                self.reload_active_tab();
                true
            }
            ChromeHitTarget::Omnibox => {
                self.chrome.omnibox.focus_and_select_all();
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleSidebar => {
                self.chrome.toggle_tab_layout();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleTheme => {
                self.chrome.toggle_theme();
                let is_newtab = self.chrome.active().url == URL_NEWTAB;
                let id = self.chrome.active().id;
                if is_newtab {
                    self.load_internal_newtab(id);
                }
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleHud | ChromeHitTarget::ContextToggleHud => {
                self.chrome.context_menu.visible = false;
                self.chrome.toggle_hud();
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::Suggestion(idx) => {
                let suggestions = self.chrome.autocomplete_suggestions(6);
                if let Some(item) = suggestions.get(idx) {
                    let url = item.url.clone();
                    self.navigate_active(&url);
                }
                true
            }
            ChromeHitTarget::HudTab(tab) => {
                self.hud.active_tab = tab;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ContextOpenLinkNewTab => {
                self.chrome.context_menu.visible = false;
                if let Some(url) = self.chrome.context_menu.link_url.clone() {
                    let id = self.chrome.new_tab(url.clone());
                    self.ensure_tab_doc(id);
                    self.fetch_and_commit(id, &url, FormMethod::Get, None, Vec::new());
                }
                true
            }
            ChromeHitTarget::ContextCopyLink => {
                self.chrome.context_menu.visible = false;
                if let Some(url) = self.chrome.context_menu.link_url.clone() {
                    self.clipboard.set_text(url);
                }
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ContextCopy => {
                self.chrome.context_menu.visible = false;
                let active_id = self.chrome.active().id;
                if let Some(t) = self.tabs_doc.get(&active_id) {
                    if !t.interaction.selected_text.is_empty() {
                        self.clipboard.set_text(t.interaction.selected_text.clone());
                    }
                }
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::HistoryLink(idx) => {
                if let Some(entry) = self.chrome.history.get(idx) {
                    let url = entry.url.clone();
                    self.navigate_active(&url);
                }
                true
            }
        }
    }

    fn close_tab_at(&mut self, idx: usize) -> bool {
        if let Some(removed_id) = self.chrome.close_tab(idx) {
            self.tabs_doc.remove(&removed_id);
            let active_id = self.chrome.active().id;
            let is_new = !self.tabs_doc.contains_key(&active_id);
            self.ensure_tab_doc(active_id);
            if is_new {
                self.load_internal_newtab(active_id);
            }
            self.chrome_dirty = true;
            self.web_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            true
        } else {
            false
        }
    }

    fn ensure_tab_doc(&mut self, id: TabId) {
        let web_rect = self.web_viewport_rect();
        let dark = self.chrome.theme.is_dark();
        self.tabs_doc
            .entry(id)
            .or_insert_with(|| TabDocumentState::new(id, web_rect.size, dark));
    }

    /// Re-themes active chrome & internal pages (useful for headless dark/light screenshot renders).
    pub fn set_theme(&mut self, theme: ThemeMode) {
        if self.chrome.theme != theme {
            self.chrome.theme = theme;
            let id = self.chrome.active().id;
            if self.chrome.active().url == URL_NEWTAB {
                self.load_internal_newtab(id);
            }
            self.chrome_dirty = true;
            self.web_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
        }
    }
}

/// Wraps a document `DisplayList` into a clipped APZ scroll frame positioned at `web_rect`.
fn wrap_web_display_list(
    inner: DisplayList,
    web_rect: Rect,
    scroll_id: ScrollId,
    scroll_y: f32,
) -> DisplayList {
    let mut out = DisplayList::new(Rect::new(0.0, 0.0, web_rect.max_x(), web_rect.max_y()));
    let translate_node = out.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::Transform {
            matrix: [1.0, 0.0, 0.0, 1.0, web_rect.min_x(), web_rect.min_y()],
            origin: Point::ZERO,
            animation_id: None,
        },
    );
    let content_h = inner.content_bounds.size.height.max(web_rect.size.height);
    let content_w = inner.content_bounds.size.width.max(web_rect.size.width);
    let scroll_node = out.spatial_tree.add_node(
        Some(translate_node),
        SpatialNodeKind::ScrollFrame {
            external_scroll_id: scroll_id,
            content_rect: Rect::new(0.0, 0.0, content_w, content_h),
            viewport_rect: Rect::new(0.0, 0.0, web_rect.size.width, web_rect.size.height),
            current_offset: Vec2::new(0.0, scroll_y),
        },
    );

    // Remap inner spatial tree nodes beneath `scroll_node`.
    let base_idx = out.spatial_tree.nodes.len() as u32;
    for node in inner.spatial_tree.nodes.iter().skip(1) {
        let remapped_parent = match node.parent {
            None | Some(SpatialNodeId(0)) => Some(scroll_node),
            Some(SpatialNodeId(p)) => Some(SpatialNodeId(base_idx + p - 1)),
        };
        out.spatial_tree
            .add_node(remapped_parent, node.kind.clone());
    }

    out.push(DisplayItem::PushClip {
        spatial: translate_node,
        rect: Rect::new(0.0, 0.0, web_rect.size.width, web_rect.size.height),
        radii: [0.0; 4],
    });
    out.push(DisplayItem::PushStackingContext {
        spatial: scroll_node,
        bounds: Rect::new(0.0, 0.0, content_w, content_h),
        opacity: 1.0,
        mix_blend: rix_render::BlendMode::Normal,
        filters: rix_render::smallvec![],
    });
    out.glyphs = inner.glyphs;
    for mut item in inner.items {
        match &mut item {
            DisplayItem::PushStackingContext { spatial, .. }
            | DisplayItem::PushClip { spatial, .. } => {
                *spatial = if spatial.0 == 0 {
                    scroll_node
                } else {
                    SpatialNodeId(base_idx + spatial.0 - 1)
                };
            }
            _ => {}
        }
        out.push(item);
    }
    out.push(DisplayItem::PopStackingContext);
    out.push(DisplayItem::PopClip);
    out
}

fn extract_document_title(doc: &Document) -> Option<String> {
    for node in doc.descendants(doc.root()) {
        if let Some(NodeData::Element(e)) = doc.get(node) {
            if e.name.local.eq_str_ignore_ascii_case("title") {
                let text = collect_element_text(doc, node);
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn extract_style_elements(doc: &Document) -> Vec<String> {
    let mut out = Vec::new();
    for node in doc.descendants(doc.root()) {
        if let Some(NodeData::Element(e)) = doc.get(node) {
            if e.name.local.eq_str_ignore_ascii_case("style") {
                let mut css = String::new();
                for ch in doc.children(node) {
                    if let Some(NodeData::Text(t)) = doc.get(ch) {
                        css.push_str(t);
                    }
                }
                if !css.trim().is_empty() {
                    out.push(css);
                }
            }
        }
    }
    out
}

/// Ensures dark/light chrome stylesheets compile cleanly (re-exported for convenience).
#[must_use]
pub fn default_chrome_css(theme: ThemeMode) -> String {
    chrome_stylesheet(theme)
}
