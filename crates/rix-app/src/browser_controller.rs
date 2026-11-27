//! Parent-process [`BrowserController`] coordinating isolated Chrome UI (`z = 100`),
//! active web tab surfaces (`z = 10`), transient overlays & `F12` Developer HUD (`z = 200`),
//! real HTTP/HTTPS GET & POST navigation via `rix-net`, full parallel subresource loading
//! (external CSS + `@import`, `@font-face`, `<img>`/`srcset`/SVG/`background-image`, external &
//! inline ES/classic scripts via `ContentRuntime` + Cranelift 3-tier JIT), in-page DOM events &
//! form interaction, OS clipboard, and Content Process crash recovery.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rix_chrome::{
    chrome_asset_provider, chrome_stylesheet, find_link_href, palette_for, render_bookmarks_html,
    render_crashed_html, render_error_html, render_history_html, render_newtab_html,
    render_settings_html, BrowserSettings, ChromeHitTarget, ChromeRenderer, ChromeState,
    CookiePolicy, DevHudState, FrameStageSample, JitMode, NetworkEntry, RenderedChromeLayer,
    ThemeMode, URL_BOOKMARKS, URL_CRASHED, URL_ERROR, URL_HISTORY, URL_NEWTAB, URL_SETTINGS,
};
use rix_content::ContentRuntime;
use rix_core::{Point, Rect, RixUrl, Size, TabId, Vec2};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_ipc::{PipelineId, ScrollId, SpatialNodeId};
use rix_layout::{hit_test, FragmentTree, LayoutEngine};
use rix_net::cache::now_secs;
use rix_net::{FetchEngine, HttpCache, HttpClient, ResourceManager};
use rix_render::{Color, Compositor, DisplayItem, DisplayList, LayerRole, Rgba8, SpatialNodeKind};
use rix_storage::StorageEngine;
use rix_text::{FontDatabase, FontFaceSet};

use crate::forms::{
    collect_element_text, resolve_relative_url, ClipboardBridge, FormMethod, FormSubmission,
    InPageInteractionState, PageClickOutcome,
};
use crate::page_loader::{
    load_page_subresources, DecodedAssetCache, PageLoadStats, SpeculativePreconnector,
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

/// Per-tab web document, style context, layout engine, live JS/DOM [`ContentRuntime`],
/// and form interaction state.
pub struct TabDocumentState {
    pub doc: Document,
    pub styles: StyleContext,
    pub layout: LayoutEngine,
    pub fragment_tree: FragmentTree,
    pub interaction: InPageInteractionState,
    pub raw_html: String,
    pub page_css: Vec<String>,
    pub scroll_id: ScrollId,
    pub is_chrome_page: bool,
    pub runtime: Option<ContentRuntime>,
    pub last_load_stats: PageLoadStats,
    pub last_js_errors: Vec<String>,
    doc_snapshot_revision: u64,
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
            is_chrome_page: true,
            runtime: None,
            last_load_stats: PageLoadStats::default(),
            last_js_errors: Vec::new(),
            doc_snapshot_revision: 0,
        }
    }

    /// Synchronizes `self.doc` snapshot from the live `ContentRuntime` document when modified.
    fn sync_snapshot_from_runtime(&mut self) {
        let Some(ref rt) = self.runtime else {
            return;
        };
        let doc_rc = Rc::clone(&rt.host.borrow().document);
        let live_doc = doc_rc.borrow();
        let rev = live_doc.revision();
        if rev != self.doc_snapshot_revision {
            self.doc = live_doc.clone();
            self.doc_snapshot_revision = rev;
        }
    }

    /// Hit-tests `doc_pt` against the live runtime fragment tree (for web tabs) or the
    /// static chrome fragment tree (for internal chrome pages) with zero fragment-tree copying.
    #[must_use]
    pub fn hit_test_point(&self, doc_pt: Point) -> Option<rix_dom::NodeId> {
        if let Some(ref rt) = self.runtime {
            return rt
                .host
                .borrow()
                .cached_fragment_tree()
                .and_then(|ft| hit_test(ft, doc_pt));
        }
        hit_test(&self.fragment_tree, doc_pt)
    }

    /// Computes the CSS Backgrounds & Borders Level 3 §2.11.2 canvas background color
    /// propagated from `<html>` or `<body>` (defaulting to `#ffffff` for unthemed web pages).
    fn page_canvas_background(&self, dark_mode: bool) -> Option<Rgba8> {
        if self.is_chrome_page {
            return None;
        }
        if let Some(ref rt) = self.runtime {
            let host = rt.host.borrow();
            let doc = host.document.borrow();
            return Some(resolve_canvas_bg(&doc, host.style_context(), dark_mode));
        }
        Some(resolve_canvas_bg(&self.doc, &self.styles, dark_mode))
    }
}

/// Complete parent-process browser controller driving `rix-chrome`, `rix-content`, `rix-layout`,
/// `rix-net`, `rix-storage`, and `rix-render::Compositor`.
pub struct BrowserController {
    pub chrome: ChromeState,
    pub hud: DevHudState,
    pub clipboard: ClipboardBridge,
    pub compositor: Compositor,
    chrome_renderer: ChromeRenderer,
    base_font_db: Arc<FontDatabase>,
    storage: StorageEngine,
    http_cache: Arc<HttpCache>,
    http_client: HttpClient,
    fetch_engine: Arc<FetchEngine>,
    resource_manager: ResourceManager,
    asset_cache: DecodedAssetCache,
    jit_code_cache: rix_content::SharedBaselineCache,
    preconnector: SpeculativePreconnector,
    runtime: tokio::runtime::Runtime,
    tabs_doc: HashMap<TabId, TabDocumentState>,
    last_chrome_layer: Option<RenderedChromeLayer>,
    last_overlay_layer: Option<RenderedChromeLayer>,
    settings_path: Option<PathBuf>,
    chrome_dirty: bool,
    overlay_dirty: bool,
    web_dirty: bool,
    frame_seq: u64,
}

impl BrowserController {
    #[must_use]
    pub fn new(viewport: Size, dpr: f32) -> Self {
        Self::with_settings_path(viewport, dpr, None)
    }

    /// Creates a controller that loads and persists [`BrowserSettings`] at `settings_path`.
    #[must_use]
    pub fn with_settings_path(viewport: Size, dpr: f32, settings_path: Option<PathBuf>) -> Self {
        let mut font_db = FontDatabase::new();
        font_db.scan_system_fonts();
        let base_font_db = Arc::new(font_db);

        let loaded_settings = settings_path
            .as_deref()
            .and_then(|p| BrowserSettings::load_from_file(p).ok())
            .unwrap_or_default();
        let chrome = ChromeState::with_settings(loaded_settings);

        let data_dir = settings_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(PathBuf::from);
        let storage = data_dir
            .as_ref()
            .and_then(|dir| StorageEngine::open_persistent(&dir.join("profile.redb")).ok())
            .unwrap_or_else(|| {
                StorageEngine::in_memory().expect("in-memory StorageEngine initializes")
            });
        let http_cache_dir = data_dir
            .as_ref()
            .map(|dir| dir.join("http_cache"))
            .or_else(|| {
                std::env::var("RIX_HTTP_CACHE_DIR")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            });
        let http_cache = Arc::new(HttpCache::new(http_cache_dir));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("browser network runtime initializes");

        let storage_for_http = if chrome.settings.cookie_policy == CookiePolicy::BlockAll {
            None
        } else {
            Some(storage.clone())
        };
        let mut http_client = {
            let _rt_guard = runtime.enter();
            HttpClient::new(storage_for_http)
                .expect("default HTTP client initializes")
                .with_timeout(Duration::from_secs(8))
                .with_http_cache(Some(Arc::clone(&http_cache)))
        };
        http_client.set_preferred_color_scheme(if chrome.theme.is_dark() {
            "dark"
        } else {
            "light"
        });
        let fetch_engine = Arc::new(FetchEngine::new(http_client.clone()));
        let mut resource_manager = ResourceManager::new(http_client.clone())
            .with_concurrency(16)
            .with_chrome_asset_provider(chrome_asset_provider());
        if let Some(ref dir) = data_dir {
            if let Ok(rm) = resource_manager
                .clone()
                .with_disk_cache(dir.join("resource_cache"))
            {
                resource_manager = rm;
            }
        }

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
            base_font_db,
            storage,
            http_cache,
            http_client,
            fetch_engine,
            resource_manager,
            asset_cache: DecodedAssetCache::new(),
            jit_code_cache: rix_content::new_shared_baseline_cache(),
            preconnector: SpeculativePreconnector::new(),
            runtime,
            tabs_doc,
            last_chrome_layer: None,
            last_overlay_layer: None,
            settings_path,
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

    #[must_use]
    pub fn asset_cache(&self) -> &DecodedAssetCache {
        &self.asset_cache
    }

    #[must_use]
    pub fn http_cache(&self) -> &Arc<HttpCache> {
        &self.http_cache
    }

    /// Returns the active zoom scale factor (`1.0` for Chrome internal pages, `default_zoom_pct / 100` for web tabs).
    #[must_use]
    pub fn active_page_zoom(&self) -> f32 {
        let active_id = self.chrome.active().id;
        let is_chrome = self
            .tabs_doc
            .get(&active_id)
            .is_none_or(|t| t.is_chrome_page);
        if is_chrome {
            1.0
        } else {
            (f32::from(self.chrome.settings.default_zoom_pct) / 100.0).clamp(0.5, 2.5)
        }
    }

    /// Evaluates JavaScript directly inside the active web tab's `ContentRuntime` and syncs dirty layers.
    pub fn eval_js_on_active_tab(&mut self, source: &str) -> Result<String, String> {
        let active_id = self.chrome.active().id;
        let active_url = self.chrome.active().url.clone();
        let (result, console_msgs) = {
            let Some(tab_doc) = self.tabs_doc.get_mut(&active_id) else {
                return Err("no active tab".to_string());
            };
            let Some(ref mut rt) = tab_doc.runtime else {
                return Err("active tab is an internal chrome page without JS runtime".to_string());
            };
            let _rt_guard = self.runtime.enter();
            let res = rt
                .eval_script_cached(source)
                .map(|v| rt.vm.value_to_rust_string(v))
                .map_err(|e| format!("{e:?}"));
            let msgs = rt.drain_console_messages();
            (res, msgs)
        };
        for (lvl, msg) in console_msgs {
            self.hud.record_console(lvl, msg, active_url.clone());
        }
        self.web_dirty = true;
        self.sync_layers();
        result
    }

    /// Steps pending async `fetch()`/WebSocket responses, Web Workers, timers, and
    /// `requestAnimationFrame` callbacks on the active web tab.
    ///
    /// Returns the exact delay until the next wakeup is needed, or `None` when the tab is
    /// completely idle (`IDLE 0 Hz` — allowing the window event loop to sleep in `ControlFlow::Wait`).
    pub fn tick_active_tab(&mut self) -> Option<Duration> {
        self.tick_active_tab_inner(true)
    }

    /// Same as [`Self::tick_active_tab`], but defers `sync_layers()` so batched headless settle
    /// loops avoid rebuilding full layout and display lists before presenting the final frame.
    pub fn tick_active_tab_no_sync(&mut self) -> Option<Duration> {
        self.tick_active_tab_inner(false)
    }

    fn tick_active_tab_inner(&mut self, sync_layers: bool) -> Option<Duration> {
        let active_id = self.chrome.active().id;
        let active_url = self.chrome.active().url.clone();
        let (progressed, next_delay, console_msgs, surfaces_updated) = {
            let tab_doc = self.tabs_doc.get_mut(&active_id)?;
            let rt = tab_doc.runtime.as_mut()?;
            let _rt_guard = self.runtime.enter();
            let mut ran = rt.poll_async_and_timers();
            if rt.event_loop.borrow().has_pending_raf() {
                let now_ms = rt.event_loop.borrow().now_ms();
                if rt.fire_vsync(now_ms) > 0 {
                    ran = true;
                }
            }
            let c2d = rt
                .canvas2d
                .borrow_mut()
                .present_dirty_to_compositor(&mut self.compositor, None);
            let wgpu = rt
                .webgpu
                .borrow_mut()
                .present_dirty_to_compositor(&mut self.compositor, None);
            let wgl = rt
                .webgl2
                .borrow_mut()
                .present_dirty_to_compositor(&mut self.compositor, None);
            let vid = rt
                .media_elements
                .borrow_mut()
                .present_dirty_to_compositor(&mut self.compositor, None);
            let surfaces = c2d + wgpu + wgl + vid;
            let msgs = rt.drain_console_messages();
            let delay = rt.next_wakeup_delay();
            (ran, delay, msgs, surfaces)
        };

        for (lvl, msg) in console_msgs {
            self.hud.record_console(lvl, msg, active_url.clone());
            if self.chrome.hud_open {
                self.overlay_dirty = true;
            }
        }
        if sync_layers && (progressed || surfaces_updated > 0) {
            self.sync_layers();
        }
        next_delay
    }

    fn apply_settings_to_engine(&mut self) {
        self.http_client
            .set_preferred_color_scheme(if self.chrome.theme.is_dark() {
                "dark"
            } else {
                "light"
            });
        let storage_for_http = if self.chrome.settings.cookie_policy == CookiePolicy::BlockAll {
            None
        } else {
            Some(self.storage.clone())
        };
        self.http_client.set_storage(storage_for_http);
        let jit_mode = self.chrome.settings.jit_mode;
        let jit_cache = self.jit_code_cache.clone();
        for tab_doc in self.tabs_doc.values_mut() {
            if let Some(ref mut rt) = tab_doc.runtime {
                configure_runtime_jit(rt, jit_mode, &jit_cache);
            }
        }
    }

    /// Marks Chrome, Web, and Overlay layers dirty so direct state mutations repaint on next sync.
    pub fn invalidate_all_layers(&mut self) {
        self.chrome_dirty = true;
        self.web_dirty = true;
        self.overlay_dirty = true;
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
        let dpr = self.compositor.device_pixel_ratio();
        let mut style_ms = 0.0f32;
        let mut layout_ms = 0.0f32;
        let mut paint_ms = 0.0f32;
        let total_start = Instant::now();

        let web_rect = self.web_viewport_rect();
        let active_id = self.chrome.active().id;
        let active_url = self.chrome.active().url.clone();
        let dark_mode = self.chrome.theme.is_dark();
        let scroll_y = self.chrome.active().scroll_y;
        let page_zoom = self.active_page_zoom();
        let logical_web_size = Size::new(
            (web_rect.size.width / page_zoom).max(64.0),
            (web_rect.size.height / page_zoom).max(64.0),
        );

        let has_runtime = self
            .tabs_doc
            .get(&active_id)
            .is_some_and(|t| t.runtime.is_some());

        if has_runtime {
            let mut runtime_console = Vec::new();
            let mut updated_title: Option<String> = None;
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);

            if let Some(tab_doc) = self.tabs_doc.get_mut(&active_id) {
                if let Some(ref mut rt) = tab_doc.runtime {
                    {
                        let mut host = rt.host.borrow_mut();
                        host.set_viewport(logical_web_size);
                        host.set_dark_mode(dark_mode);
                        host.device_pixel_ratio = dpr;
                        host.scroll_offset.y = scroll_y;
                        if self.web_dirty {
                            host.invalidate_paint();
                        }
                    }

                    let painted = rt
                        .host
                        .borrow_mut()
                        .paint_if_dirty_with_metrics(WEB_PIPELINE, &mut self.compositor.resources);

                    let c2d = rt
                        .canvas2d
                        .borrow_mut()
                        .present_dirty_to_compositor(&mut self.compositor, None);
                    let wgpu = rt
                        .webgpu
                        .borrow_mut()
                        .present_dirty_to_compositor(&mut self.compositor, None);
                    let wgl = rt
                        .webgl2
                        .borrow_mut()
                        .present_dirty_to_compositor(&mut self.compositor, None);
                    let vid = rt
                        .media_elements
                        .borrow_mut()
                        .present_dirty_to_compositor(&mut self.compositor, None);
                    let _ = c2d + wgpu + wgl + vid;

                    runtime_console = rt.drain_console_messages();

                    if let Some((dl, s_ms, l_ms, p_ms)) = painted {
                        style_ms += s_ms;
                        layout_ms += l_ms;
                        paint_ms += p_ms;

                        self.hud
                            .ingest_css_missing_features(rt.host.borrow().missing_css_features());
                        let canvas_bg = tab_doc.page_canvas_background(dark_mode);
                        let wrapped = wrap_web_display_list(
                            dl,
                            web_rect,
                            page_zoom,
                            tab_doc.scroll_id,
                            scroll_y,
                            canvas_bg,
                        );
                        self.compositor.submit_display_list(
                            WEB_PIPELINE,
                            LayerRole::WebSurface,
                            seq,
                            wrapped,
                        );
                    }
                    tab_doc.sync_snapshot_from_runtime();
                    updated_title = extract_document_title(&tab_doc.doc);
                }
            }

            if let Some(title) = updated_title {
                if self.chrome.active().title != title {
                    self.chrome.active_mut().title = title;
                    self.chrome_dirty = true;
                }
            }
            for (lvl, msg) in runtime_console {
                self.hud.record_console(lvl, msg, active_url.clone());
                if self.chrome.hud_open {
                    self.overlay_dirty = true;
                }
            }
            self.web_dirty = false;
        } else if self.web_dirty {
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);

            if let Some(tab_doc) = self.tabs_doc.get_mut(&active_id) {
                let t_style = Instant::now();
                let media = MediaContext {
                    viewport_width: logical_web_size.width,
                    viewport_height: logical_web_size.height,
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
                    logical_web_size,
                    &self.base_font_db,
                    WEB_PIPELINE,
                    &mut self.compositor.resources,
                );
                layout_ms += frame.metrics.box_tree_ms + frame.metrics.layout_ms;
                paint_ms += frame.metrics.paint_ms;
                tab_doc.fragment_tree = frame.fragment_tree;

                let canvas_bg = tab_doc.page_canvas_background(dark_mode);
                let wrapped = wrap_web_display_list(
                    frame.display_list,
                    web_rect,
                    page_zoom,
                    tab_doc.scroll_id,
                    scroll_y,
                    canvas_bg,
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
                &self.base_font_db,
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
                &self.base_font_db,
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

    /// Loads HTML into the given tab.
    ///
    /// - Internal `rix://chrome/*` pages (`is_chrome_page == true`) use the lightweight zero-JS
    ///   chrome pipeline with isolated chrome theme stylesheets.
    /// - Real web pages (`is_chrome_page == false`) execute the full engine pipeline: parallel
    ///   subresource fetching (`PreloadScanner` + `ResourceManager` concurrency 16), cached CSS
    ///   compilation + `@import`, `@font-face` web font decoding, parallel `rayon` image/SVG
    ///   decoding, and live [`ContentRuntime`] JavaScript + Cranelift 3-tier JIT execution.
    pub fn load_html_into_tab(
        &mut self,
        tab_id: TabId,
        url: &str,
        html: &str,
        is_chrome_page: bool,
    ) {
        let web_rect = self.web_viewport_rect();
        let dark_mode = self.chrome.theme.is_dark();
        let dpr = self.compositor.device_pixel_ratio();
        let zoom = if is_chrome_page {
            1.0
        } else {
            (f32::from(self.chrome.settings.default_zoom_pct) / 100.0).clamp(0.5, 2.5)
        };
        let logical_size = Size::new(
            (web_rect.size.width / zoom).max(64.0),
            (web_rect.size.height / zoom).max(64.0),
        );
        let media = MediaContext {
            viewport_width: logical_size.width,
            viewport_height: logical_size.height,
            dark_mode,
            ..MediaContext::default()
        };

        if is_chrome_page {
            let mut doc = parse_html(html);
            let page_title = extract_document_title(&doc);
            let inline_styles = extract_style_elements(&doc);

            let mut styles = StyleContext::new(media);
            styles.add_stylesheet(self.chrome_renderer.stylesheet_for(self.chrome.theme));
            for css in &inline_styles {
                styles.add_stylesheet(Stylesheet::parse_cached(css, CascadeOrigin::Author, &media));
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
                .or_insert_with(|| TabDocumentState::new(tab_id, logical_size, dark_mode));
            entry.doc = doc;
            entry.styles = styles;
            entry.layout.clear_caches();
            entry.interaction.clear();
            entry.raw_html = html.to_string();
            entry.page_css = inline_styles;
            entry.is_chrome_page = true;
            entry.runtime = None;
            entry.last_load_stats = PageLoadStats::default();
            entry.doc_snapshot_revision = 0;

            self.hud.clear_page_telemetry();
            self.hud
                .ingest_css_missing_features(entry.styles.missing_features());
            self.web_dirty = true;
            self.chrome_dirty = true;
            if self.chrome.hud_open {
                self.overlay_dirty = true;
            }
            return;
        }

        // Full real-web engine pipeline: parallel subresources + ContentRuntime + JIT + Canvas/Media.
        let t_load_start = Instant::now();
        self.hud.clear_page_telemetry();
        let loaded = load_page_subresources(
            url,
            html,
            &media,
            &self.resource_manager,
            &self.runtime,
            &self.asset_cache,
        );
        let subres_ms = t_load_start.elapsed().as_secs_f32() * 1000.0;
        for net_entry in &loaded.network_entries {
            self.hud.record_network(net_entry.clone());
        }

        let mut web_fonts = FontFaceSet::new();
        let tab_font_db = if loaded.web_fonts.is_empty() {
            Arc::clone(&self.base_font_db)
        } else {
            let mut cloned_db = (*self.base_font_db).clone();
            loaded.register_web_fonts(&mut cloned_db, &mut web_fonts);
            Arc::new(cloned_db)
        };

        let doc_rc = Rc::new(RefCell::new(Document::new()));
        let mut rt = ContentRuntime::with_full_config(
            Rc::clone(&doc_rc),
            logical_size,
            tab_font_db,
            url,
            self.storage.clone(),
            Some(Arc::clone(&self.fetch_engine)),
        );
        configure_runtime_jit(&mut rt, self.chrome.settings.jit_mode, &self.jit_code_cache);
        {
            let mut host = rt.host.borrow_mut();
            host.set_dark_mode(dark_mode);
            host.device_pixel_ratio = dpr;
            loaded.register_into_assets(&mut host.layout_engine.assets);
            host.initial_assets_loaded = true;
            host.layout_engine.web_fonts = web_fonts;
            host.set_external_stylesheets(loaded.external_stylesheets.clone());
        }

        let t_js = Instant::now();
        let script_errors = {
            let _rt_guard = self.runtime.enter();
            rt.load_document_with_external_scripts(html, &loaded.external_scripts)
        };
        let js_ms = t_js.elapsed().as_secs_f32() * 1000.0;
        tracing::info!(
            url = %url,
            subresources = loaded.stats.subresources_fetched,
            cache_hits = loaded.stats.network_cache_hits + loaded.stats.decoded_asset_cache_hits,
            subres_ms = format_args!("{subres_ms:.1}"),
            wave1_ms = format_args!("{:.1}", loaded.stats.wave1_ms),
            wave2_ms = format_args!("{:.1}", loaded.stats.wave2_ms),
            css_ms = format_args!("{:.1}", loaded.stats.css_parse_ms),
            img_decode_ms = format_args!("{:.1}", loaded.stats.image_decode_ms),
            js_ms = format_args!("{js_ms:.1}"),
            js_errors = script_errors.len(),
            "committed real-web document"
        );
        let console_msgs = rt.drain_console_messages();
        for (lvl, msg) in console_msgs {
            self.hud.record_console(lvl, msg, url.to_string());
        }
        if !script_errors.is_empty() && self.chrome.hud_open {
            self.overlay_dirty = true;
        }

        let snapshot_doc = doc_rc.borrow().clone();
        let snapshot_rev = snapshot_doc.revision();
        let page_title = extract_document_title(&snapshot_doc);
        let inline_styles = extract_style_elements(&snapshot_doc);

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
            .or_insert_with(|| TabDocumentState::new(tab_id, logical_size, dark_mode));
        entry.doc = snapshot_doc;
        entry.doc_snapshot_revision = snapshot_rev;
        entry.interaction.clear();
        entry.raw_html = html.to_string();
        entry.page_css = inline_styles;
        entry.is_chrome_page = false;
        let mut load_stats = loaded.stats;
        load_stats.js_ms = js_ms;
        load_stats.js_error_count = script_errors.len();
        entry.last_load_stats = load_stats;
        entry.last_js_errors = script_errors;
        entry.runtime = Some(rt);

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

    fn load_internal_settings(&mut self, tab_id: TabId) {
        let html = render_settings_html(&self.chrome);
        self.load_html_into_tab(tab_id, URL_SETTINGS, &html, true);
    }

    fn load_internal_bookmarks(&mut self, tab_id: TabId) {
        let html = render_bookmarks_html(&self.chrome);
        self.load_html_into_tab(tab_id, URL_BOOKMARKS, &html, true);
    }

    fn load_internal_history(&mut self, tab_id: TabId) {
        let html = render_history_html(&self.chrome);
        self.load_html_into_tab(tab_id, URL_HISTORY, &html, true);
    }

    fn refresh_active_internal_page(&mut self) {
        let id = self.chrome.active().id;
        match self.chrome.active().url.as_str() {
            URL_NEWTAB => self.load_internal_newtab(id),
            URL_SETTINGS => self.load_internal_settings(id),
            URL_BOOKMARKS => self.load_internal_bookmarks(id),
            URL_HISTORY => self.load_internal_history(id),
            _ => {}
        }
    }

    fn persist_settings(&self) {
        if let Some(path) = &self.settings_path {
            let _ = self.chrome.settings.save_to_file(path);
        }
    }

    fn update_find_match_count(&mut self) {
        let query = self.chrome.find_bar.query.trim().to_ascii_lowercase();
        if query.is_empty() {
            self.chrome.find_bar.total_matches = 0;
            self.chrome.find_bar.match_index = 0;
            return;
        }
        let active_id = self.chrome.active().id;
        let count = self.tabs_doc.get(&active_id).map_or(0, |t| {
            let root = t.doc.root();
            let text = collect_element_text(&t.doc, root).to_ascii_lowercase();
            text.matches(&query).count()
        });
        self.chrome.find_bar.total_matches = count;
        if count == 0 || self.chrome.find_bar.match_index >= count {
            self.chrome.find_bar.match_index = 0;
        }
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
        if url == URL_SETTINGS {
            self.load_internal_settings(tab_id);
            self.sync_layers();
            return;
        }
        if url == URL_BOOKMARKS {
            self.load_internal_bookmarks(tab_id);
            self.sync_layers();
            return;
        }
        if url == URL_HISTORY {
            self.load_internal_history(tab_id);
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

        let effective_url = if self.chrome.settings.https_only {
            maybe_upgrade_https_only(url)
        } else {
            url.to_string()
        };

        let start = Instant::now();
        let Ok(parsed_url) = RixUrl::parse(&effective_url) else {
            let html = render_error_html(self.chrome.theme, url, "Malformed URL");
            self.load_html_into_tab(tab_id, URL_ERROR, &html, true);
            self.sync_layers();
            return;
        };

        let mut headers = HashMap::new();
        if let Some(ct) = content_type {
            headers.insert("Content-Type".to_string(), ct.to_string());
        }
        if self.chrome.settings.send_dnt {
            headers.insert("DNT".to_string(), "1".to_string());
        }
        let method_str = match method {
            FormMethod::Get => "GET",
            FormMethod::Post => "POST",
        };
        let (resp_res, cache_hit) = if method == FormMethod::Get && body.is_empty() {
            let now = now_secs();
            if let Some(entry) = self.http_cache.get(parsed_url.as_str()) {
                if entry.meta.is_fresh_or_swr(now) {
                    (Ok(self.http_cache.to_http_response(&entry, now)), true)
                } else {
                    if let Some(ref etag) = entry.meta.etag {
                        headers.insert("If-None-Match".to_string(), etag.clone());
                    }
                    if let Some(ref lm) = entry.meta.last_modified {
                        headers.insert("If-Modified-Since".to_string(), lm.clone());
                    }
                    match self.runtime.block_on(self.http_client.request(
                        &parsed_url,
                        method_str,
                        &headers,
                        body,
                    )) {
                        Ok(resp) if resp.status == 304 => {
                            let updated = self
                                .http_cache
                                .update_revalidated(parsed_url.as_str(), &resp.headers, now)
                                .unwrap_or(entry);
                            (Ok(self.http_cache.to_http_response(&updated, now)), true)
                        }
                        other => (other, false),
                    }
                }
            } else {
                (
                    self.runtime.block_on(self.http_client.request(
                        &parsed_url,
                        method_str,
                        &headers,
                        body,
                    )),
                    false,
                )
            }
        } else {
            (
                self.runtime.block_on(self.http_client.request(
                    &parsed_url,
                    method_str,
                    &headers,
                    body,
                )),
                false,
            )
        };
        let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;

        match resp_res {
            Ok(resp) => {
                if method == FormMethod::Get && !cache_hit && (200..300).contains(&resp.status) {
                    self.http_client.cache_response(parsed_url.as_str(), &resp);
                }
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
                    protocol: if cache_hit {
                        "cache".to_string()
                    } else {
                        format!("{:?}", resp.protocol)
                    },
                    status: resp.status,
                    mime,
                    bytes: resp.body.len(),
                    cache_hit,
                    ttfb_ms: elapsed_ms,
                    total_ms: elapsed_ms,
                });
                self.load_html_into_tab(tab_id, &final_url_str, &html, false);
            }
            Err(err) => {
                self.hud.record_network(NetworkEntry {
                    url: effective_url,
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

        // Dismiss open menus if click landed elsewhere.
        if self.chrome.context_menu.visible || self.chrome.app_menu_open {
            self.chrome.context_menu.visible = false;
            self.chrome.app_menu_open = false;
            self.chrome_dirty = true;
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
        let page_zoom = self.active_page_zoom();
        let doc_pt = Point::new(
            (point.x - web_rect.min_x()) / page_zoom,
            (point.y - web_rect.min_y()) / page_zoom + scroll_y,
        );

        let hit_node = self
            .tabs_doc
            .get(&active_id)
            .and_then(|t| t.hit_test_point(doc_pt));
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

        // Dispatch real DOM `click` event into `ContentRuntime` on web pages (honoring preventDefault!).
        let mut js_handled_dom_change = false;
        let mut default_allowed = true;
        let mut click_console = Vec::new();
        if let Some(t) = self.tabs_doc.get_mut(&active_id) {
            if let Some(ref mut rt) = t.runtime {
                let prev_rev = rt.host.borrow().document.borrow().revision();
                let _rt_guard = self.runtime.enter();
                default_allowed = rt.dispatch_node_event(node, "click", true, true, &[]);
                let _ = rt.poll_async_and_timers();
                let next_rev = rt.host.borrow().document.borrow().revision();
                js_handled_dom_change = next_rev != prev_rev;
                click_console = rt.drain_console_messages();
            }
        }
        for (lvl, msg) in click_console {
            self.hud.record_console(lvl, msg, base_url.clone());
            if self.chrome.hud_open {
                self.overlay_dirty = true;
            }
        }
        if !default_allowed {
            self.web_dirty = true;
            self.sync_layers();
            return true;
        }

        let outcome = match self.tabs_doc.get_mut(&active_id) {
            Some(t) => {
                if let Some(ref mut rt) = t.runtime {
                    let doc_rc = Rc::clone(&rt.host.borrow().document);
                    let out = {
                        let mut live_doc = doc_rc.borrow_mut();
                        t.interaction.handle_click(&mut live_doc, node, &base_url)
                    };
                    if matches!(out, PageClickOutcome::ControlUpdated) {
                        let _rt_guard = self.runtime.enter();
                        let _ = rt.dispatch_node_event(node, "input", true, false, &[]);
                        let _ = rt.dispatch_node_event(node, "change", true, false, &[]);
                    }
                    out
                } else {
                    t.interaction.handle_click(&mut t.doc, node, &base_url)
                }
            }
            None => PageClickOutcome::None,
        };
        match outcome {
            PageClickOutcome::Navigate(url) => {
                self.navigate_active(&url);
                true
            }
            PageClickOutcome::SubmitForm(sub) => {
                let submit_allowed = self
                    .tabs_doc
                    .get_mut(&active_id)
                    .and_then(|t| t.runtime.as_mut())
                    .is_none_or(|rt| {
                        let _rt_guard = self.runtime.enter();
                        rt.dispatch_node_event(node, "submit", true, true, &[])
                    });
                if submit_allowed {
                    self.submit_form(sub);
                } else {
                    self.web_dirty = true;
                    self.sync_layers();
                }
                true
            }
            PageClickOutcome::ControlUpdated => {
                self.web_dirty = true;
                self.sync_layers();
                true
            }
            PageClickOutcome::SelectedText(_) => {
                if js_handled_dom_change {
                    self.web_dirty = true;
                }
                self.sync_layers();
                true
            }
            PageClickOutcome::None => {
                if js_handled_dom_change {
                    self.web_dirty = true;
                    self.sync_layers();
                    true
                } else {
                    self.sync_layers();
                    false
                }
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
            let page_zoom = self.active_page_zoom();
            let doc_pt = Point::new(
                (point.x - web_rect.min_x()) / page_zoom,
                (point.y - web_rect.min_y()) / page_zoom + scroll_y,
            );
            if let Some(t) = self.tabs_doc.get(&active_id) {
                has_selection = !t.interaction.selected_text.is_empty();
                if let Some(hit) = t.hit_test_point(doc_pt) {
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

    /// Updates bottom-left hover URL preview pill when pointer moves over `<a href>` links
    /// and speculatively warms TCP+TLS/QUIC connections to hovered external origins.
    pub fn handle_hover(&mut self, point: Point) -> bool {
        let web_rect = self.web_viewport_rect();
        let next_hover = if web_rect.contains(point) {
            let active_id = self.chrome.active().id;
            let base_url = self.chrome.active().url.clone();
            let scroll_y = self.chrome.active().scroll_y;
            let page_zoom = self.active_page_zoom();
            let doc_pt = Point::new(
                (point.x - web_rect.min_x()) / page_zoom,
                (point.y - web_rect.min_y()) / page_zoom + scroll_y,
            );
            self.tabs_doc.get(&active_id).and_then(|t| {
                let hit = t.hit_test_point(doc_pt)?;
                let href = find_link_href(&t.doc, hit)?;
                resolve_relative_url(&base_url, &href)
            })
        } else {
            None
        };
        if next_hover != self.chrome.hovered_link {
            if let Some(ref url) = next_hover {
                let _ = self
                    .preconnector
                    .maybe_preconnect(url, &self.http_client, &self.runtime);
            }
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
                BrowserKey::Char(',') => {
                    self.navigate_active(URL_SETTINGS);
                    return true;
                }
                BrowserKey::Char('f' | 'F') => {
                    self.chrome.find_bar.open_and_focus();
                    self.chrome.omnibox.blur();
                    self.chrome.app_menu_open = false;
                    self.update_find_match_count();
                    self.chrome_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('d' | 'D') => {
                    self.chrome.toggle_bookmark_active();
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('b' | 'B') => {
                    self.chrome.toggle_bookmarks_bar();
                    self.persist_settings();
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.web_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('h' | 'H') => {
                    self.navigate_active(URL_HISTORY);
                    return true;
                }
                BrowserKey::Char('=' | '+') => {
                    self.chrome.zoom_in();
                    self.persist_settings();
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.web_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('-') => {
                    self.chrome.zoom_out();
                    self.persist_settings();
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.web_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Char('0') => {
                    self.chrome.zoom_reset();
                    self.persist_settings();
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.web_dirty = true;
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                _ => {}
            }
        }

        if key == BrowserKey::Escape {
            self.chrome.omnibox.blur();
            self.chrome.context_menu.visible = false;
            self.chrome.app_menu_open = false;
            self.chrome.find_bar.close();
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }

        if self.chrome.find_bar.open && !self.chrome.omnibox.focused {
            match key {
                BrowserKey::Backspace => {
                    self.chrome.find_bar.backspace();
                    self.update_find_match_count();
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                BrowserKey::Enter => {
                    self.chrome.find_bar.step_match(!mods.shift);
                    self.overlay_dirty = true;
                    self.sync_layers();
                    return true;
                }
                _ => {}
            }
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
            // Forward Backspace / Enter to focused in-page form controls (dispatching live DOM events).
            let active_id = self.chrome.active().id;
            let base_url = self.chrome.active().url.clone();
            match key {
                BrowserKey::Backspace => {
                    let updated = if let Some(t) = self.tabs_doc.get_mut(&active_id) {
                        let focused = t.interaction.focused_control;
                        if let Some(ref mut rt) = t.runtime {
                            let doc_rc = Rc::clone(&rt.host.borrow().document);
                            let changed = {
                                let mut live_doc = doc_rc.borrow_mut();
                                t.interaction.backspace(&mut live_doc)
                            };
                            if changed {
                                if let Some(node) = focused {
                                    let _rt_guard = self.runtime.enter();
                                    let _ = rt.dispatch_node_event(node, "input", true, false, &[]);
                                }
                            }
                            changed
                        } else {
                            t.interaction.backspace(&mut t.doc)
                        }
                    } else {
                        false
                    };
                    if updated {
                        self.web_dirty = true;
                        self.sync_layers();
                        return true;
                    }
                }
                BrowserKey::Enter => {
                    let focused = self
                        .tabs_doc
                        .get(&active_id)
                        .and_then(|t| t.interaction.focused_control);
                    let outcome =
                        self.tabs_doc
                            .get_mut(&active_id)
                            .map_or(PageClickOutcome::None, |t| {
                                if let Some(ref mut rt) = t.runtime {
                                    let doc_rc = Rc::clone(&rt.host.borrow().document);
                                    let mut live_doc = doc_rc.borrow_mut();
                                    t.interaction.press_enter(&mut live_doc, &base_url)
                                } else {
                                    t.interaction.press_enter(&mut t.doc, &base_url)
                                }
                            });
                    match outcome {
                        PageClickOutcome::SubmitForm(sub) => {
                            let submit_allowed = focused
                                .and_then(|node| {
                                    self.tabs_doc
                                        .get_mut(&active_id)
                                        .and_then(|t| t.runtime.as_mut())
                                        .map(|rt| {
                                            let _rt_guard = self.runtime.enter();
                                            rt.dispatch_node_event(node, "submit", true, true, &[])
                                        })
                                })
                                .unwrap_or(true);
                            if submit_allowed {
                                self.submit_form(sub);
                            } else {
                                self.web_dirty = true;
                                self.sync_layers();
                            }
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

    /// Routes typed characters into the focused omnibox, find bar, or in-page `<input>`/`<textarea>`.
    pub fn handle_text_input(&mut self, text: &str) -> bool {
        if self.chrome.omnibox.focused {
            self.chrome.omnibox.insert_str(text);
            self.chrome_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }
        if self.chrome.find_bar.open {
            self.chrome.find_bar.insert_str(text);
            self.update_find_match_count();
            self.overlay_dirty = true;
            self.sync_layers();
            return true;
        }
        let active_id = self.chrome.active().id;
        let updated = if let Some(t) = self.tabs_doc.get_mut(&active_id) {
            let focused = t.interaction.focused_control;
            if let Some(ref mut rt) = t.runtime {
                let doc_rc = Rc::clone(&rt.host.borrow().document);
                let changed = {
                    let mut live_doc = doc_rc.borrow_mut();
                    t.interaction.insert_text(&mut live_doc, text)
                };
                if changed {
                    if let Some(node) = focused {
                        let _rt_guard = self.runtime.enter();
                        let _ = rt.dispatch_node_event(node, "input", true, false, &[]);
                    }
                }
                changed
            } else {
                t.interaction.insert_text(&mut t.doc, text)
            }
        } else {
            false
        };
        if updated {
            self.web_dirty = true;
            self.sync_layers();
            return true;
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
            ChromeHitTarget::Home => {
                let home = self.chrome.settings.home_url.clone();
                self.navigate_active(&home);
                true
            }
            ChromeHitTarget::Omnibox => {
                self.chrome.omnibox.focus_and_select_all();
                self.chrome.app_menu_open = false;
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleBookmarkStar => {
                self.chrome.toggle_bookmark_active();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ResetZoom => {
                self.chrome.zoom_reset();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleSidebar => {
                self.chrome.toggle_tab_layout();
                self.chrome.app_menu_open = false;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleTheme => {
                self.chrome.toggle_theme();
                self.chrome.app_menu_open = false;
                self.apply_settings_to_engine();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::ToggleHud | ChromeHitTarget::ContextToggleHud => {
                self.chrome.context_menu.visible = false;
                self.chrome.app_menu_open = false;
                self.chrome.toggle_hud();
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::OpenSettings => {
                self.chrome.app_menu_open = false;
                self.navigate_active(URL_SETTINGS);
                true
            }
            ChromeHitTarget::OpenBookmarks => {
                self.chrome.app_menu_open = false;
                self.navigate_active(URL_BOOKMARKS);
                true
            }
            ChromeHitTarget::OpenHistory => {
                self.chrome.app_menu_open = false;
                self.navigate_active(URL_HISTORY);
                true
            }
            ChromeHitTarget::OpenNewTabPage => {
                self.chrome.app_menu_open = false;
                self.navigate_active(URL_NEWTAB);
                true
            }
            ChromeHitTarget::ToggleAppMenu => {
                self.chrome.toggle_app_menu();
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::OpenFindBar => {
                self.chrome.app_menu_open = false;
                self.chrome.find_bar.open_and_focus();
                self.update_find_match_count();
                self.chrome_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::FindPrev => {
                self.chrome.find_bar.step_match(false);
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::FindNext => {
                self.chrome.find_bar.step_match(true);
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::FindClose => {
                self.chrome.find_bar.close();
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
            ChromeHitTarget::BookmarkBarItem(idx) => {
                if let Some(bm) = self.chrome.bookmarks.get(idx) {
                    let url = bm.url.clone();
                    self.navigate_active(&url);
                }
                true
            }
            ChromeHitTarget::DeleteBookmark(idx) => {
                if self.chrome.remove_bookmark(idx) {
                    self.refresh_active_internal_page();
                    self.chrome_dirty = true;
                    self.sync_layers();
                }
                true
            }
            ChromeHitTarget::DeleteHistory(idx) => {
                if self.chrome.remove_history(idx) {
                    self.refresh_active_internal_page();
                    self.sync_layers();
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
            ChromeHitTarget::SettingsSelectSection(section) => {
                self.chrome.settings_section = section;
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsSetTheme(theme) => {
                self.chrome.set_theme(theme);
                self.apply_settings_to_engine();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsSetTabLayout(layout) => {
                self.chrome.set_tab_layout(layout);
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleBookmarksBar => {
                self.chrome.toggle_bookmarks_bar();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleCompactToolbar => {
                self.chrome.toggle_compact_toolbar();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsZoomIn => {
                self.chrome.zoom_in();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsZoomOut => {
                self.chrome.zoom_out();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsFontInc => {
                self.chrome.settings.step_font_size(1);
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsFontDec => {
                self.chrome.settings.step_font_size(-1);
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsSetSearchEngine(engine) => {
                self.chrome.set_search_engine(engine);
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleAutocomplete => {
                self.chrome.settings.autocomplete_enabled =
                    !self.chrome.settings.autocomplete_enabled;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleFullUrl => {
                self.chrome.settings.show_full_url = !self.chrome.settings.show_full_url;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleHttpsOnly => {
                self.chrome.settings.https_only = !self.chrome.settings.https_only;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsSetCookiePolicy(policy) => {
                self.chrome.set_cookie_policy(policy);
                self.apply_settings_to_engine();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleDnt => {
                self.chrome.settings.send_dnt = !self.chrome.settings.send_dnt;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsClearHistory => {
                self.chrome.clear_history();
                self.refresh_active_internal_page();
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsResetDefaults => {
                self.chrome.reset_settings();
                self.apply_settings_to_engine();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.chrome_dirty = true;
                self.web_dirty = true;
                self.overlay_dirty = true;
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsSetJitMode(mode) => {
                self.chrome.set_jit_mode(mode);
                self.apply_settings_to_engine();
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleGpu => {
                self.chrome.settings.hardware_compositing =
                    !self.chrome.settings.hardware_compositing;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
                true
            }
            ChromeHitTarget::SettingsToggleHudOnStartup => {
                self.chrome.settings.open_hud_on_startup =
                    !self.chrome.settings.open_hud_on_startup;
                self.persist_settings();
                self.refresh_active_internal_page();
                self.sync_layers();
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
            self.chrome.set_theme(theme);
            self.apply_settings_to_engine();
            self.persist_settings();
            self.refresh_active_internal_page();
            self.chrome_dirty = true;
            self.web_dirty = true;
            self.overlay_dirty = true;
            self.sync_layers();
        }
    }
}

/// Wraps a document `DisplayList` into a clipped, zoom-scaled APZ scroll frame positioned at
/// `web_rect`, painting a full-viewport canvas backdrop when `canvas_bg` is provided.
fn wrap_web_display_list(
    inner: DisplayList,
    web_rect: Rect,
    zoom: f32,
    scroll_id: ScrollId,
    scroll_y: f32,
    canvas_bg: Option<Rgba8>,
) -> DisplayList {
    let zoom = zoom.clamp(0.5, 2.5);
    let logical_w = (web_rect.size.width / zoom).max(1.0);
    let logical_h = (web_rect.size.height / zoom).max(1.0);
    let mut out = DisplayList::new(Rect::new(0.0, 0.0, web_rect.max_x(), web_rect.max_y()));
    let translate_node = out.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::Transform {
            matrix: [1.0, 0.0, 0.0, 1.0, web_rect.min_x(), web_rect.min_y()],
            origin: Point::ZERO,
            animation_id: None,
        },
    );
    let content_parent = if (zoom - 1.0).abs() > 1e-4 {
        out.spatial_tree.add_node(
            Some(translate_node),
            SpatialNodeKind::Transform {
                matrix: [zoom, 0.0, 0.0, zoom, 0.0, 0.0],
                origin: Point::ZERO,
                animation_id: None,
            },
        )
    } else {
        translate_node
    };
    let content_h = inner.content_bounds.size.height.max(logical_h);
    let content_w = inner.content_bounds.size.width.max(logical_w);
    let scroll_node = out.spatial_tree.add_node(
        Some(content_parent),
        SpatialNodeKind::ScrollFrame {
            external_scroll_id: scroll_id,
            content_rect: Rect::new(0.0, 0.0, content_w, content_h),
            viewport_rect: Rect::new(0.0, 0.0, logical_w, logical_h),
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
    if let Some(bg) = canvas_bg {
        out.push(DisplayItem::PushStackingContext {
            spatial: translate_node,
            bounds: Rect::new(0.0, 0.0, web_rect.size.width, web_rect.size.height),
            opacity: 1.0,
            mix_blend: rix_render::BlendMode::Normal,
            filters: rix_render::smallvec![],
        });
        out.push(DisplayItem::Rect {
            bounds: Rect::new(0.0, 0.0, web_rect.size.width, web_rect.size.height),
            color: bg,
            radii: [0.0; 4],
        });
        out.push(DisplayItem::PopStackingContext);
    }
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
            | DisplayItem::PushClip { spatial, .. }
            | DisplayItem::PushClipPath { spatial, .. } => {
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

fn configure_runtime_jit(
    rt: &mut ContentRuntime,
    mode: JitMode,
    shared_cache: &rix_content::SharedBaselineCache,
) {
    // Browser content sessions use cost-balanced JIT tier thresholds (128 calls / 384 loop edges /
    // 2,000 calls for Tier-3) plus a session-scoped W^X code cache so warm reloads incur 0 ms
    // Cranelift recompilation while avoiding compiling cold hydration glue functions.
    const BROWSER_BASELINE_THRESHOLD: u32 = 128;
    const BROWSER_OSR_THRESHOLD: u32 = 384;
    const BROWSER_OPT_THRESHOLD: u32 = 2_000;
    match mode {
        JitMode::FullThreeTier => rt.configure_jit_with_cache(
            true,
            true,
            BROWSER_BASELINE_THRESHOLD,
            BROWSER_OSR_THRESHOLD,
            BROWSER_OPT_THRESHOLD,
            Some(shared_cache.clone()),
        ),
        JitMode::BaselineOnly => rt.configure_jit_with_cache(
            true,
            false,
            BROWSER_BASELINE_THRESHOLD,
            BROWSER_OSR_THRESHOLD,
            u32::MAX,
            Some(shared_cache.clone()),
        ),
        JitMode::InterpreterOnly => rt.configure_jit(false, false),
    }
}

/// Upgrades `http://` navigations to `https://` when HTTPS-Only mode is active,
/// exempting loopback addresses (`localhost`, `127.0.0.1`, `[::1]`) for local dev servers.
fn maybe_upgrade_https_only(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.scheme() != "http" {
        return url.to_string();
    }
    let is_loopback = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    );
    if !is_loopback {
        let _ = parsed.set_scheme("https");
    }
    parsed.to_string()
}

/// Implements W3C CSS Backgrounds & Borders Level 3 §2.11.2 Canvas Background Propagation:
/// prefers `<html>` background if non-transparent, else `<body>` background if non-transparent,
/// else crisp white (`#ffffff`) so unthemed web pages stay readable even in Dark Chrome mode.
fn resolve_canvas_bg(doc: &Document, styles: &StyleContext, dark_mode: bool) -> Rgba8 {
    let mut html_node = None;
    let mut body_node = None;
    for n in doc.descendants(doc.root()) {
        if let Some(NodeData::Element(e)) = doc.get(n) {
            if html_node.is_none() && e.name.local.eq_str_ignore_ascii_case("html") {
                html_node = Some(n);
            } else if body_node.is_none() && e.name.local.eq_str_ignore_ascii_case("body") {
                body_node = Some(n);
                break;
            }
        }
    }
    for candidate in [html_node, body_node].into_iter().flatten() {
        if let Some(st) = styles.style_for(candidate) {
            let bg = st.visual.background_color;
            if bg.a > 0 {
                return Rgba8::new(bg.r, bg.g, bg.b, bg.a);
            }
        }
    }
    if let Some(html_id) = html_node {
        if let Some(st) = styles.style_for(html_id) {
            if st.color_scheme_dark && dark_mode {
                return Rgba8::new(18, 18, 22, 255);
            }
        }
    }
    Rgba8::new(255, 255, 255, 255)
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
