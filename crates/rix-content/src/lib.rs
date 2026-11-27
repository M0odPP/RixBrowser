#![deny(unsafe_code)]
//! Content-process runtime integrating `rix-js`, `rix-dom`, `rix-html`,
//! `rix-css`, `rix-layout`, `rix-net`, and `rix-storage` with a reactive
//! WHATWG Event Loop, zero-leak Weak DOM Bridge (`WeakTable<NodeId>`),
//! Fetch CORS/preflight, RFC 6455 WebSocket, Streams, and Web Storage.
//!
//! Reference: Spec 05 §2.2, Spec 06 §1–§3, Issues #16 & #17.

pub mod canvas2d;
mod css_animations_api;
pub mod cssom;
pub mod cssom_geometry;
pub mod cssom_view;
pub mod custom_elements;
pub mod dom_bridge;
pub mod dom_collections;
pub mod dom_interfaces;
pub mod dom_parsing;
pub mod dom_sanitizer;
pub mod element_internals;
pub mod event_loop;
pub mod events;
mod fetch_body;
mod fetch_headers;
pub mod fetch_ws;
pub mod frames;
pub mod indexeddb;
pub mod indexeddb_engine;
pub(crate) mod indexeddb_runtime;
pub mod media_elements;
pub mod media_source;
pub(crate) mod media_source_worker;
pub mod observers;
pub mod selection;
pub mod service_worker;
pub mod shadow_dom;
pub mod streams;
mod streams_bytes;
mod streams_common;
mod streams_pipe;
mod streams_transform;
mod streams_writable;
pub(crate) mod structured_clone;
pub mod trusted_types;
pub mod url_pattern;
pub mod web_animations;
mod web_animations_core;
mod web_animations_timing;
pub mod web_apis;
pub mod webaudio;
pub mod webgl2;
pub mod webgpu;
pub mod webgpu_types;
mod websocket_stream;
pub mod workers;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_core::Size;
use rix_dom::{Document, NodeData, NodeId};
use rix_html::{attach_declarative_shadow_roots, HtmlParserStream, ParserStreamStep};
use rix_ipc::{ContentToBrowser, MessagePortId, PipelineId};
use rix_js::{
    BytecodeCache, EvalError, GcCycleStats, ImportMap, JsValue, JsVm, ModuleLoader,
    ModuleSourceProvider, SourceType,
};
use rix_net::FetchEngine;
use rix_render::{Compositor, DisplayList, LayerRole, ResourceRegistry, VelloRenderer};
use rix_storage::StorageEngine;
use rix_text::FontDatabase;
use url::Url;

pub use canvas2d::{install_canvas2d, Canvas2DSystem, SharedCanvas2D};
pub use frames::{
    bind_child_frame, deliver_ipc_port_message, deliver_ipc_post_message,
    install_frame_and_messaging_apis, mark_child_frame_crashed, sync_all_iframes_in_document,
    FrameSystem, SandboxFlags, SharedFrames,
};
pub use rix_jit::{new_shared_baseline_cache, SharedBaselineCache};

pub use dom_bridge::{
    install_dom_bridge, wrap_node, CustomElementDef, DomMutation, DomPrototypes, HostState,
    MutationSink, SharedHost,
};
pub use event_loop::{
    drain_ready_tasks, drain_ready_tasks_with_budget, fire_vsync, install_event_loop_globals,
    run_microtask_checkpoint, step_event_loop_once, wait_for_next_event_or_timer, EventLoopState,
    ExternalEvent, MacrotaskAction, MicrotaskCheckpointHook, SharedEventLoop, TaskPriority,
};
pub use events::{
    fire_dom_node_event, fire_object_event, install_event_system, EventSystem, EventTargetKey,
    SharedEvents,
};
pub use fetch_ws::{
    deliver_async_net_event, install_fetch_and_websocket, AsyncNetEvent, FetchFailure,
    NetworkBridge, SharedNetBridge,
};
pub use indexeddb::{install_indexeddb, IndexedDbSystem, SharedIndexedDb};
pub use media_elements::{
    install_media_elements, MediaElementSystem, MediaPlaybackTelemetry, MediaPlayerState,
    SharedMediaElements,
};
pub use media_source::{install_media_source, MediaSourceSystem, SharedMediaSource};
pub use observers::{
    deliver_layout_observers, deliver_mutation_observers, install_observers, ObserverSystem,
    SharedObservers,
};
pub use service_worker::{
    deliver_async_sw_event, install_cache_storage, install_service_worker_and_cache_apis,
    AsyncSwEvent, ServiceWorkerSystem, SharedServiceWorkers,
};
pub use streams::{
    create_readable_stream_from_bytes, install_streams, SharedStreams, StreamSystem,
};
pub use web_animations::{
    install_web_animations, update_animations_and_send_events, SharedWebAnimations,
    WebAnimationSystem,
};
pub use web_apis::{install_web_apis, SharedWebApis, WebApiState, DEFAULT_DOCUMENT_URL};
pub use webaudio::{install_webaudio, SharedWebAudio, WebAudioSystem};
pub use webgl2::{install_webgl2, SharedWebGl2, WebGl2System};
pub use webgpu::{install_webgpu, SharedWebGpu, WebGpuSystem};
pub use workers::{
    deliver_async_worker_event, install_worker_and_sse_apis, AsyncWorkerEvent, SharedWorkers,
    WorkerSystem,
};

/// Complete single-origin Content Process execution context owning a `JsVm`,
/// Weak DOM Bridge (`HostState`), Event System, Observers, Core Web APIs,
/// WHATWG Streams, reactive Fetch/WebSocket `NetworkBridge`, and `EventLoopState`.
pub struct ContentRuntime {
    pub vm: JsVm,
    pub host: SharedHost,
    pub event_loop: SharedEventLoop,
    pub events: SharedEvents,
    pub observers: SharedObservers,
    pub web_apis: SharedWebApis,
    pub web_animations: SharedWebAnimations,
    pub streams: SharedStreams,
    pub net: SharedNetBridge,
    pub canvas2d: SharedCanvas2D,
    pub frames: SharedFrames,
    pub workers: SharedWorkers,
    pub service_workers: SharedServiceWorkers,
    pub indexeddb: SharedIndexedDb,
    pub media_source: SharedMediaSource,
    pub media_elements: SharedMediaElements,
    pub webaudio: SharedWebAudio,
    pub webgpu: SharedWebGpu,
    pub webgl2: SharedWebGl2,
    pub jit: Rc<rix_jit::TieredJit>,
    pub wasm: rix_jit::SharedWasmHost,
}

/// Document-scoped ES Module source provider backed by preloaded Wave-1 `<script>` /
/// `<link rel="modulepreload">` sources, inline module scripts, and live [`FetchEngine`]
/// network fallback for transitive static `import`s and dynamic `import(...)` expressions.
struct DocumentModuleProvider {
    sources: Rc<RefCell<HashMap<String, String>>>,
    fetch_engine: Option<Arc<FetchEngine>>,
    doc_origin: String,
    doc_url: String,
}

impl ModuleSourceProvider for DocumentModuleProvider {
    fn prefetch_sources(&self, urls: &[String]) {
        let Some(ref engine) = self.fetch_engine else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let missing: Vec<String> = {
            let cache = self.sources.borrow();
            urls.iter()
                .filter(|u| {
                    (u.starts_with("http://") || u.starts_with("https://"))
                        && !cache.contains_key(u.as_str())
                })
                .cloned()
                .collect()
        };
        if missing.len() <= 1 {
            return;
        }
        let count = missing.len();
        let (tx, rx) = std::sync::mpsc::channel();
        for url in missing {
            let eng = Arc::clone(engine);
            let tx_c = tx.clone();
            let doc_url = self.doc_url.clone();
            let doc_origin = self.doc_origin.clone();
            handle.spawn(async move {
                let mut headers = HashMap::new();
                headers.insert("referer".to_string(), doc_url);
                headers.insert("accept".to_string(), "*/*".to_string());
                let req = rix_net::FetchRequest {
                    url: url.clone(),
                    method: "GET".to_string(),
                    headers,
                    body: Vec::new(),
                    mode: rix_net::FetchMode::Cors,
                    credentials: rix_net::FetchCredentials::SameOrigin,
                    cache: rix_net::FetchCache::Default,
                    redirect: rix_net::FetchRedirect::Follow,
                    origin: doc_origin,
                    abort_signal: None,
                };
                let fetched = eng.fetch(req).await.ok().and_then(|resp| {
                    if !(200..300).contains(&resp.status) {
                        return None;
                    }
                    let ct = resp
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.to_ascii_lowercase())
                        .unwrap_or_default();
                    if ct.contains("text/html") || ct.contains("application/xhtml+xml") {
                        return None;
                    }
                    let text = String::from_utf8_lossy(&resp.body).into_owned();
                    let trimmed = text.trim_start();
                    if trimmed.starts_with("<!DOCTYPE")
                        || trimmed.starts_with("<!doctype")
                        || trimmed.starts_with("<html")
                    {
                        return None;
                    }
                    Some(text)
                });
                let _ = tx_c.send((url, fetched));
            });
        }
        drop(tx);
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let mut received = 0usize;
        while received < count {
            let Some(rem) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            match rx.recv_timeout(rem) {
                Ok((url, Some(text))) => {
                    self.sources.borrow_mut().insert(url, text);
                    received += 1;
                }
                Ok((_, None)) => {
                    received += 1;
                }
                Err(_) => break,
            }
        }
    }

    fn load_source(&self, url: &str) -> Result<String, String> {
        if let Some(code) = self.sources.borrow().get(url).cloned() {
            return Ok(code);
        }
        if let Some(file_path) = url.strip_prefix("file://") {
            let clean = file_path.split(['?', '#']).next().unwrap_or(file_path);
            let bytes = std::fs::read(clean)
                .map_err(|e| format!("failed to read module file '{url}': {e}"))?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            self.sources
                .borrow_mut()
                .insert(url.to_string(), text.clone());
            return Ok(text);
        }
        if let Some(ref engine) = self.fetch_engine {
            if url.starts_with("http://") || url.starts_with("https://") {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let mut headers = HashMap::new();
                    headers.insert("referer".to_string(), self.doc_url.clone());
                    headers.insert("accept".to_string(), "*/*".to_string());
                    let req = rix_net::FetchRequest {
                        url: url.to_string(),
                        method: "GET".to_string(),
                        headers,
                        body: Vec::new(),
                        mode: rix_net::FetchMode::Cors,
                        credentials: rix_net::FetchCredentials::SameOrigin,
                        cache: rix_net::FetchCache::Default,
                        redirect: rix_net::FetchRedirect::Follow,
                        origin: self.doc_origin.clone(),
                        abort_signal: None,
                    };
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let eng = Arc::clone(engine);
                    handle.spawn(async move {
                        let _ = tx.send(eng.fetch(req).await);
                    });
                    let resp = rx
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| format!("module fetch timed out: {url}"))?
                        .map_err(|e| format!("module fetch failed for '{url}': {e}"))?;
                    if !(200..300).contains(&resp.status) {
                        return Err(format!("module HTTP {}: {url}", resp.status));
                    }
                    let text = String::from_utf8_lossy(&resp.body).into_owned();
                    self.sources
                        .borrow_mut()
                        .insert(url.to_string(), text.clone());
                    return Ok(text);
                }
            }
        }
        Err(format!("module not found: {url}"))
    }
}

impl ContentRuntime {
    /// Creates a fresh runtime over `document` with default origin and in-memory storage.
    #[must_use]
    pub fn new(
        document: Rc<RefCell<Document>>,
        viewport: Size,
        font_db: Arc<FontDatabase>,
    ) -> Self {
        let storage = StorageEngine::in_memory().expect("in-memory StorageEngine");
        Self::with_origin_and_storage(document, viewport, font_db, DEFAULT_DOCUMENT_URL, storage)
    }

    /// Creates a runtime configured with an explicit origin URL and shared [`FetchEngine`]
    /// over in-memory storage.
    #[must_use]
    pub fn with_fetch_engine(
        document: Rc<RefCell<Document>>,
        viewport: Size,
        font_db: Arc<FontDatabase>,
        origin_url: &str,
        fetch_engine: Arc<FetchEngine>,
    ) -> Self {
        let storage = StorageEngine::in_memory().expect("in-memory StorageEngine");
        Self::with_full_config(
            document,
            viewport,
            font_db,
            origin_url,
            storage,
            Some(fetch_engine),
        )
    }

    /// Creates a runtime configured with an explicit document origin URL and persistent or
    /// in-memory [`StorageEngine`].
    #[must_use]
    pub fn with_origin_and_storage(
        document: Rc<RefCell<Document>>,
        viewport: Size,
        font_db: Arc<FontDatabase>,
        origin_url: &str,
        storage: StorageEngine,
    ) -> Self {
        Self::with_full_config(document, viewport, font_db, origin_url, storage, None)
    }

    /// Applies an HTTP `Content-Security-Policy` or `Content-Security-Policy-Report-Only`
    /// response header to the main document's CSP policy set.
    pub fn apply_response_csp_header(&self, header_name: &str, header_value: &str) {
        if header_name.eq_ignore_ascii_case("cross-origin-opener-policy") {
            let v = header_value.split(';').next().unwrap_or("").trim();
            if v.eq_ignore_ascii_case("same-origin") {
                let mut wa = self.web_apis.borrow_mut();
                wa.coop_same_origin = true;
                wa.cross_origin_isolated = wa.coop_same_origin && wa.coep_require_corp;
            }
            return;
        }
        if header_name.eq_ignore_ascii_case("cross-origin-embedder-policy") {
            let v = header_value.split(';').next().unwrap_or("").trim();
            if v.eq_ignore_ascii_case("require-corp") || v.eq_ignore_ascii_case("credentialless") {
                let mut wa = self.web_apis.borrow_mut();
                wa.coep_require_corp = true;
                wa.cross_origin_isolated = wa.coop_same_origin && wa.coep_require_corp;
            }
            return;
        }
        let report_only = if header_name.eq_ignore_ascii_case("content-security-policy-report-only")
        {
            true
        } else if header_name.eq_ignore_ascii_case("content-security-policy") {
            false
        } else {
            return;
        };
        let root = self.host.borrow().document.borrow().root();
        self.host
            .borrow_mut()
            .doc_csp
            .entry(root)
            .or_default()
            .apply_header(header_value, report_only);
    }

    /// Creates a runtime configured with an explicit origin URL, [`StorageEngine`],
    /// and optional shared [`FetchEngine`].
    #[must_use]
    pub fn with_full_config(
        document: Rc<RefCell<Document>>,
        viewport: Size,
        font_db: Arc<FontDatabase>,
        origin_url: &str,
        storage: StorageEngine,
        fetch_engine: Option<Arc<FetchEngine>>,
    ) -> Self {
        let mut vm = JsVm::new();
        let jit = rix_jit::TieredJit::attach_to(&mut vm);
        let wasm = rix_jit::install_webassembly(&mut vm);
        let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
        install_event_loop_globals(&mut vm, &event_loop);
        let host = install_dom_bridge(&mut vm, document, viewport, font_db);
        let events = install_event_system(&mut vm, &host, &event_loop);
        let observers = install_observers(&mut vm, &host, &event_loop);
        shadow_dom::install_shadow_dom_apis(&mut vm, &host, &events, &event_loop);
        custom_elements::install_custom_elements(&mut vm, &host, &events, &event_loop);
        let web_apis = install_web_apis(&mut vm, &host, &events, &event_loop, origin_url, storage);
        trusted_types::install_trusted_types(&mut vm, &host, &events);
        dom_parsing::install_dom_parsing(&mut vm, &host, &events, &event_loop, &web_apis);
        let web_animations = install_web_animations(&mut vm, &host, &events);
        cssom::install_cssom(&mut vm, &host, &web_animations);
        cssom_geometry::install_cssom_geometry(&mut vm, &host);
        cssom_view::install_cssom_view(&mut vm, &host, &events, &event_loop);
        let streams = install_streams(&mut vm, &web_apis);
        let net = install_fetch_and_websocket(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &streams,
            fetch_engine,
        );
        dom_sanitizer::install_dom_sanitizer_and_streaming(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &streams,
            &net,
        );
        let canvas2d = install_canvas2d(&mut vm, &host, &web_apis);
        let frames =
            install_frame_and_messaging_apis(&mut vm, &host, &events, &event_loop, &web_apis, &net);
        {
            let mut fs = frames.borrow_mut();
            fs.canvas2d = Some(Rc::clone(&canvas2d));
            fs.net = Some(Rc::clone(&net));
        }
        let workers = install_worker_and_sse_apis(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &net,
            &frames,
        );
        frames.borrow_mut().workers = Some(Rc::clone(&workers));
        let service_workers =
            install_service_worker_and_cache_apis(&mut vm, &host, &web_apis, &streams, &net);
        let indexeddb = install_indexeddb(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &net,
            &canvas2d,
            &frames,
            &workers,
        );
        let media_elements = Rc::new(RefCell::new(MediaElementSystem::new()));
        media_elements.borrow_mut().net = Some(Rc::clone(&net));
        let media_source = media_source::install_media_source_with_elements(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &media_elements,
        );
        media_source.borrow_mut().realm_async_tx = Some(workers.borrow().async_tx_clone());
        frames.borrow_mut().media_source = Some(Rc::clone(&media_source));
        media_elements::install_media_elements_with_system(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &media_source,
            &media_elements,
        );
        let webaudio = install_webaudio(&mut vm, &host, &media_elements);
        let webgpu = install_webgpu(&mut vm, &host, &canvas2d);
        let webgl2 = install_webgl2(&mut vm, &host, &canvas2d);

        // Register all host subsystem GC root collectors so automatic minor/major GC
        // inside `JsVm` never sweeps live DOM wrappers, event listeners, or Web API objects.
        {
            let event_loop_r = Rc::clone(&event_loop);
            let events_r = Rc::clone(&events);
            let observers_r = Rc::clone(&observers);
            let web_apis_r = Rc::clone(&web_apis);
            let web_anims_r = Rc::clone(&web_animations);
            let streams_r = Rc::clone(&streams);
            let net_r = Rc::clone(&net);
            let canvas2d_r = Rc::clone(&canvas2d);
            let webgpu_r = Rc::clone(&webgpu);
            let webgl2_r = Rc::clone(&webgl2);
            let frames_r = Rc::clone(&frames);
            let workers_r = Rc::clone(&workers);
            let sw_r = Rc::clone(&service_workers);
            let idb_r = Rc::clone(&indexeddb);
            let mse_r = Rc::clone(&media_source);
            let me_r = Rc::clone(&media_elements);
            let audio_r = Rc::clone(&webaudio);
            let host_r = Rc::clone(&host);
            vm.add_host_root_collector(move |roots| {
                if let Ok(g) = event_loop_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = events_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = observers_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = web_apis_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = web_anims_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = streams_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = net_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = canvas2d_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = webgpu_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = webgl2_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = frames_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = workers_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = sw_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = idb_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = mse_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = me_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = audio_r.try_borrow() {
                    g.collect_roots(roots);
                }
                if let Ok(g) = host_r.try_borrow() {
                    g.collect_roots(roots);
                }
            });
        }

        Self {
            vm,
            host,
            event_loop,
            events,
            observers,
            web_apis,
            web_animations,
            streams,
            net,
            canvas2d,
            frames,
            workers,
            service_workers,
            indexeddb,
            media_source,
            media_elements,
            webaudio,
            webgpu,
            webgl2,
            jit,
            wasm,
        }
    }

    /// Creates a runtime backed by an empty document and default `800x600` viewport.
    #[must_use]
    pub fn with_empty_document() -> Self {
        let doc = Rc::new(RefCell::new(Document::new()));
        let font_db = Arc::new(FontDatabase::new());
        Self::new(doc, Size::new(800.0, 600.0), font_db)
    }

    /// Updates the current document URL used by `location`, `history`, `localStorage`,
    /// `fetch()` CORS origin checks, and `WebSocket` `Origin` headers.
    pub fn set_document_url(&mut self, url: &str) {
        self.web_apis.borrow_mut().set_document_url(url);
    }

    /// Parses `html` incrementally through [`HtmlParserStream`], pausing on every classic
    /// inline `<script>` to execute it synchronously (supporting live `document.write()`
    /// injection at the exact tokenizer insertion point!) and running a microtask checkpoint
    /// after each script.
    pub fn load_html_with_scripts(&mut self, html: &str) -> Result<(), EvalError> {
        let doc_rc = Rc::clone(&self.host.borrow().document);
        let parser = Rc::new(RefCell::new(HtmlParserStream::with_document(doc_rc)));
        parser.borrow_mut().feed(html);
        self.host.borrow_mut().active_parser = Some(Rc::clone(&parser));
        self.host.borrow_mut().ready_state = "loading".to_string();

        loop {
            let step = parser.borrow_mut().step();
            match step {
                ParserStreamStep::Script(script_id) => {
                    // Any `<style>` elements parsed since the previous script must invalidate stylesheets.
                    self.host.borrow_mut().mark_sheets_dirty();
                    let (is_classic, code) = {
                        let doc_cell = Rc::clone(&self.host.borrow().document);
                        let doc = doc_cell.borrow();
                        let classic = match doc.get(script_id) {
                            Some(NodeData::Element(e)) => {
                                let ty = e.get_attribute("type").unwrap_or("").trim();
                                let has_src = e.has_attribute("src");
                                !has_src
                                    && (ty.is_empty()
                                        || ty.eq_ignore_ascii_case("text/javascript")
                                        || ty.eq_ignore_ascii_case("application/javascript"))
                            }
                            _ => false,
                        };
                        let text = doc.inner_text(script_id);
                        (classic, text)
                    };
                    let scripts_allowed = self.frames.borrow().sandbox.scripts_allowed();
                    if scripts_allowed && is_classic && !code.trim().is_empty() {
                        let doc_root = {
                            let doc_cell = Rc::clone(&self.host.borrow().document);
                            let mut doc = doc_cell.borrow_mut();
                            attach_declarative_shadow_roots(&mut doc);
                            doc.root()
                        };
                        custom_elements::upgrade_subtree(&mut self.vm, &self.host, doc_root, false);
                        self.eval_script(&code)?;
                        let has_ce_defs = self
                            .host
                            .borrow()
                            .custom_elements_state
                            .borrow()
                            .has_any_definitions();
                        parser
                            .borrow_mut()
                            .set_pause_on_custom_elements(has_ce_defs);
                    }
                }
                ParserStreamStep::CustomElement(ce_id) => {
                    custom_elements::upgrade_subtree(&mut self.vm, &self.host, ce_id, true);
                    parser.borrow_mut().finish_pending_custom_element_insert();
                }
                ParserStreamStep::Done => break,
            }
        }

        parser.borrow_mut().finish();
        let finished_root = {
            let doc_cell = Rc::clone(&self.host.borrow().document);
            let mut doc = doc_cell.borrow_mut();
            attach_declarative_shadow_roots(&mut doc);
            doc.root()
        };
        custom_elements::upgrade_subtree(&mut self.vm, &self.host, finished_root, false);
        self.host.borrow_mut().active_parser = None;
        self.host.borrow_mut().ready_state = "complete".to_string();
        sync_all_iframes_in_document(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.web_apis,
            &self.net,
            &self.frames,
        );
        self.microtask_checkpoint();
        Ok(())
    }

    /// Sets the `PipelineId` owned by this `ContentRuntime` (used for OOPIF IPC routing).
    pub fn set_pipeline_id(&mut self, pipeline: PipelineId) {
        self.frames.borrow_mut().pipeline_id = pipeline;
    }

    /// Applies an HTML `<iframe sandbox="...">` attribute policy to this runtime.
    pub fn set_sandbox_attr(&mut self, sandbox: Option<&str>) {
        self.frames.borrow_mut().sandbox = SandboxFlags::parse(sandbox);
    }

    /// Drains all outbound OOPIF / cross-process messaging IPC events emitted by this runtime.
    pub fn drain_outbound_ipc(&mut self) -> Vec<ContentToBrowser> {
        self.frames.borrow_mut().drain_outbound_ipc()
    }

    /// Binds a spawned cross-origin OOPIF child `PipelineId` to an `<iframe>` node.
    pub fn bind_child_frame(
        &mut self,
        frame_node_id: u64,
        child_pipeline: PipelineId,
        origin: String,
    ) {
        bind_child_frame(&self.frames, frame_node_id, child_pipeline, origin);
    }

    /// Marks a bound cross-origin OOPIF pipeline as crashed inside this parent runtime.
    pub fn mark_child_frame_crashed(&mut self, child_pipeline: PipelineId) -> Vec<NodeId> {
        mark_child_frame_crashed(&self.frames, child_pipeline)
    }

    /// Delivers an inbound cross-process `window.postMessage` IPC payload into this runtime.
    pub fn deliver_ipc_post_message(
        &mut self,
        source_pipeline: PipelineId,
        data_json: &str,
        origin: &str,
        transfer_ports: &[MessagePortId],
    ) {
        deliver_ipc_post_message(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.web_apis,
            &self.frames,
            source_pipeline,
            data_json,
            origin,
            transfer_ports,
        );
    }

    /// Delivers an inbound cross-process `MessagePort` IPC payload into this runtime.
    pub fn deliver_ipc_port_message(
        &mut self,
        source_pipeline: PipelineId,
        port_id: MessagePortId,
        data_json: &str,
        transfer_ports: &[MessagePortId],
    ) {
        deliver_ipc_port_message(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.frames,
            source_pipeline,
            port_id,
            data_json,
            transfer_ports,
        );
    }

    /// Builds and returns the current `DisplayList` for `pipeline`.
    pub fn build_display_list(&mut self, pipeline: PipelineId) -> DisplayList {
        let mut resources = ResourceRegistry::default();
        self.host.borrow_mut().paint_now(pipeline, &mut resources)
    }

    /// Parses `html` incrementally through [`HtmlParserStream`], executing classic inline and
    /// external `<script>` elements in document order (using [`BytecodeCache`] backed by
    /// `StorageEngine`), executing `defer` and `type="module"` scripts (including `<script
    /// type="importmap">`) after parsing completes, firing `DOMContentLoaded` and `load`, and
    /// draining microtasks/due timers.
    ///
    /// JavaScript compilation or runtime errors are recorded to `console.error` and returned
    /// without aborting document parsing or subsequent scripts.
    pub fn load_document_with_external_scripts(
        &mut self,
        html: &str,
        external_scripts: &HashMap<String, String>,
    ) -> Vec<String> {
        self.load_document_with_external_scripts_and_charset(html, None, external_scripts)
    }

    pub fn load_document_with_external_scripts_and_charset(
        &mut self,
        html: &str,
        charset_override: Option<&str>,
        external_scripts: &HashMap<String, String>,
    ) -> Vec<String> {
        struct PendingScript {
            node_id: NodeId,
            url: String,
            code: String,
            is_module: bool,
        }

        let doc_url = self.web_apis.borrow().location_url.clone();
        let doc_origin = doc_url.origin().ascii_serialization();
        let doc_rc = Rc::clone(&self.host.borrow().document);
        let detected_charset = charset_override
            .map(str::to_string)
            .unwrap_or_else(|| rix_html::detect_html_charset(html.as_bytes()).to_string());
        doc_rc.borrow_mut().set_character_set(detected_charset);
        let parser = Rc::new(RefCell::new(HtmlParserStream::with_document(doc_rc)));
        parser.borrow_mut().feed(html);
        self.host.borrow_mut().active_parser = Some(Rc::clone(&parser));
        self.host.borrow_mut().ready_state = "loading".to_string();

        // Install a live FetchEngine-backed ES ModuleLoader BEFORE executing any inline scripts
        // so dynamic `import(...)` inside classic scripts (SvelteKit, Vite, webpack chunks) and
        // transitive static ES module imports resolve seamlessly.
        let module_sources: Rc<RefCell<HashMap<String, String>>> =
            Rc::new(RefCell::new(HashMap::new()));
        {
            let mut src_map = module_sources.borrow_mut();
            for (k, v) in external_scripts {
                src_map.insert(k.clone(), v.clone());
                if let Ok(resolved) = Url::parse(doc_url.as_str()).and_then(|b| b.join(k)) {
                    src_map.insert(resolved.to_string(), v.clone());
                }
            }
        }
        let initial_import_map = ImportMap {
            base_url: doc_url.as_str().to_string(),
            ..ImportMap::default()
        };
        let fetch_engine = self.net.borrow().fetch_engine.clone();
        let module_loader = Rc::new(RefCell::new(ModuleLoader::new(
            Box::new(DocumentModuleProvider {
                sources: Rc::clone(&module_sources),
                fetch_engine: Some(fetch_engine),
                doc_origin,
                doc_url: doc_url.as_str().to_string(),
            }),
            initial_import_map,
        )));
        ModuleLoader::install_on_vm(&module_loader, &mut self.vm);

        let mut errors = Vec::new();
        let mut deferred_scripts: Vec<PendingScript> = Vec::new();
        let mut inline_module_idx: usize = 0;
        let html_has_dsd = html
            .as_bytes()
            .windows(14)
            .any(|w| w.eq_ignore_ascii_case(b"shadowrootmode"));
        let html_has_iframe = html
            .as_bytes()
            .windows(7)
            .any(|w| w.eq_ignore_ascii_case(b"<iframe"));

        loop {
            let step = parser.borrow_mut().step();
            match step {
                ParserStreamStep::Script(script_id) => {
                    // Any `<style>` elements parsed since the previous script must invalidate stylesheets.
                    self.host.borrow_mut().mark_sheets_dirty();
                    let info = {
                        let doc_cell = Rc::clone(&self.host.borrow().document);
                        let doc = doc_cell.borrow();
                        match doc.get(script_id) {
                            Some(NodeData::Element(e)) => {
                                let ty = e.get_attribute("type").unwrap_or("").trim().to_string();
                                let src = if &*e.name.ns == crate::dom_interfaces::SVG_NS {
                                    e.get_attribute("href")
                                        .or_else(|| e.get_attribute("xlink:href"))
                                        .map(|s| s.trim().to_string())
                                } else {
                                    e.get_attribute("src").map(|s| s.trim().to_string())
                                };
                                let is_defer = e.has_attribute("defer");
                                let is_nomodule = e.has_attribute("nomodule");
                                let text = doc.inner_text(script_id);
                                Some((ty, src, is_defer, is_nomodule, text))
                            }
                            _ => None,
                        }
                    };
                    let Some((ty, src, is_defer, is_nomodule, inline_text)) = info else {
                        continue;
                    };
                    // HTML5 §4.12.1.1: module-capable user agents MUST NOT execute `<script nomodule>`.
                    if is_nomodule && !ty.eq_ignore_ascii_case("module") {
                        continue;
                    }
                    if ty.eq_ignore_ascii_case("importmap") {
                        match ImportMap::from_json(&inline_text, doc_url.as_str()) {
                            Ok(parsed) => module_loader.borrow_mut().set_import_map(parsed),
                            Err(err) => {
                                let msg = format!("ImportMap error: {err}");
                                self.web_apis
                                    .borrow_mut()
                                    .record_console("error", msg.clone());
                                errors.push(msg);
                            }
                        }
                        continue;
                    }
                    let scripts_allowed = self.frames.borrow().sandbox.scripts_allowed();
                    if !scripts_allowed {
                        continue;
                    }

                    let is_module = ty.eq_ignore_ascii_case("module");
                    let is_classic = ty.is_empty()
                        || ty.eq_ignore_ascii_case("text/javascript")
                        || ty.eq_ignore_ascii_case("application/javascript")
                        || ty.eq_ignore_ascii_case("text/ecmascript")
                        || ty.eq_ignore_ascii_case("application/ecmascript");
                    if !is_module && !is_classic {
                        continue;
                    }

                    let (script_url, code) = if let Some(ref raw_src) = src {
                        if raw_src.starts_with("data:") {
                            match crate::fetch_ws::decode_data_url(raw_src) {
                                Some((_, bytes)) => (
                                    raw_src.clone(),
                                    String::from_utf8_lossy(&bytes).into_owned(),
                                ),
                                None => continue,
                            }
                        } else {
                            let resolved = doc_url
                                .join(raw_src)
                                .map(|u| u.to_string())
                                .unwrap_or_else(|_| raw_src.clone());
                            let fetched = external_scripts
                                .get(&resolved)
                                .or_else(|| external_scripts.get(raw_src))
                                .cloned();
                            match fetched {
                                Some(src_code) => (resolved, src_code),
                                None => continue,
                            }
                        }
                    } else {
                        inline_module_idx += 1;
                        let synthetic =
                            format!("{}#inline-script-{inline_module_idx}", doc_url.as_str());
                        (synthetic, inline_text)
                    };

                    if code.trim().is_empty() {
                        continue;
                    }

                    if is_module || (is_defer && src.is_some()) {
                        if is_module {
                            module_sources
                                .borrow_mut()
                                .insert(script_url.clone(), code.clone());
                        }
                        deferred_scripts.push(PendingScript {
                            node_id: script_id,
                            url: script_url,
                            code,
                            is_module,
                        });
                    } else {
                        // Attach any declarative shadow roots and upgrade parser-created
                        // custom elements parsed prior to this inline script (HTML §13.2.6.1).
                        if html_has_dsd {
                            let doc_cell = Rc::clone(&self.host.borrow().document);
                            let mut doc = doc_cell.borrow_mut();
                            attach_declarative_shadow_roots(&mut doc);
                        }
                        if self
                            .host
                            .borrow()
                            .custom_elements_state
                            .borrow()
                            .has_any_definitions()
                        {
                            let doc_root = self.host.borrow().document.borrow().root();
                            custom_elements::upgrade_subtree(
                                &mut self.vm,
                                &self.host,
                                doc_root,
                                false,
                            );
                        }
                        if html_has_iframe {
                            sync_all_iframes_in_document(
                                &mut self.vm,
                                &self.host,
                                &self.events,
                                &self.event_loop,
                                &self.web_apis,
                                &self.net,
                                &self.frames,
                            );
                        }
                        self.host.borrow_mut().current_script = Some(script_id);
                        let t_sc = std::time::Instant::now();
                        let (sr0, ly0) = {
                            let h = self.host.borrow();
                            (h.style_recalc_count, h.layout_count)
                        };
                        let res = self.eval_script_cached(&code);
                        let sc_ms = t_sc.elapsed().as_secs_f32() * 1000.0;
                        if sc_ms >= 25.0 {
                            let (sr1, ly1) = {
                                let h = self.host.borrow();
                                (h.style_recalc_count, h.layout_count)
                            };
                            let jst = self.jit.stats();
                            tracing::debug!(
                                phase = "sync_script",
                                url = %script_url,
                                bytes = code.len(),
                                ms = format_args!("{sc_ms:.1}"),
                                style_recalcs = sr1.saturating_sub(sr0),
                                layouts = ly1.saturating_sub(ly0),
                                jit_compiled = jst.compiled_functions,
                                jit_unsup = jst.unsupported_functions,
                                jit_entries = jst.baseline_entries,
                                jit_osr = jst.osr_entries,
                                gc_minor = self.vm.heap.minor_collections(),
                                gc_major = self.vm.heap.major_collections(),
                                heap_alloc = self.vm.heap.total_allocated(),
                                "script profile"
                            );
                        }
                        self.host.borrow_mut().current_script = None;
                        let has_ce_defs = self
                            .host
                            .borrow()
                            .custom_elements_state
                            .borrow()
                            .has_any_definitions();
                        parser
                            .borrow_mut()
                            .set_pause_on_custom_elements(has_ce_defs);
                        if let Err(err) = res {
                            let msg = format_eval_error(&self.vm, &err);
                            self.web_apis
                                .borrow_mut()
                                .record_console("error", msg.clone());
                            errors.push(msg);
                        }
                    }
                }
                ParserStreamStep::CustomElement(ce_id) => {
                    custom_elements::upgrade_subtree(&mut self.vm, &self.host, ce_id, true);
                    parser.borrow_mut().finish_pending_custom_element_insert();
                }
                ParserStreamStep::Done => break,
            }
        }

        parser.borrow_mut().finish();
        let finished_root = {
            let doc_cell = Rc::clone(&self.host.borrow().document);
            let mut doc = doc_cell.borrow_mut();
            attach_declarative_shadow_roots(&mut doc);
            doc.root()
        };
        custom_elements::upgrade_subtree(&mut self.vm, &self.host, finished_root, false);
        self.host.borrow_mut().active_parser = None;

        // Execute deferred classic scripts (with `document.currentScript` bound per HTML §4.12.1.1)
        // and ES modules in document order before DOMContentLoaded.
        let had_deferred = !deferred_scripts.is_empty();
        for script in deferred_scripts {
            let t_sc = std::time::Instant::now();
            let (sr0, ly0) = {
                let h = self.host.borrow();
                (h.style_recalc_count, h.layout_count)
            };
            if script.is_module {
                self.host.borrow_mut().current_script = None;
                match ModuleLoader::load_and_evaluate(&module_loader, &mut self.vm, &script.url) {
                    Ok(_) => self.microtask_checkpoint(),
                    Err(err) => {
                        let msg = format!("Module error ({}): {err}", script.url);
                        self.web_apis
                            .borrow_mut()
                            .record_console("error", msg.clone());
                        errors.push(msg);
                    }
                }
            } else {
                self.host.borrow_mut().current_script = Some(script.node_id);
                let res = self.eval_script_cached(&script.code);
                self.host.borrow_mut().current_script = None;
                if let Err(err) = res {
                    let msg = format_eval_error(&self.vm, &err);
                    self.web_apis
                        .borrow_mut()
                        .record_console("error", msg.clone());
                    errors.push(msg);
                }
            }
            let sc_ms = t_sc.elapsed().as_secs_f32() * 1000.0;
            if sc_ms >= 25.0 {
                let (sr1, ly1) = {
                    let h = self.host.borrow();
                    (h.style_recalc_count, h.layout_count)
                };
                let jst = self.jit.stats();
                tracing::debug!(
                    phase = if script.is_module { "module" } else { "defer_script" },
                    url = %script.url,
                    bytes = script.code.len(),
                    ms = format_args!("{sc_ms:.1}"),
                    style_recalcs = sr1.saturating_sub(sr0),
                    layouts = ly1.saturating_sub(ly0),
                    jit_compiled = jst.compiled_functions,
                    jit_unsup = jst.unsupported_functions,
                    jit_entries = jst.baseline_entries,
                    jit_osr = jst.osr_entries,
                    gc_minor = self.vm.heap.minor_collections(),
                    gc_major = self.vm.heap.major_collections(),
                    heap_alloc = self.vm.heap.total_allocated(),
                    "script profile"
                );
            }
        }

        if had_deferred {
            // WHATWG HTML §13.2.7: DOMContentLoaded is queued as a task after deferred scripts run,
            // so zero-delay tasks queued synchronously by a defer script execute before it.
            let seq_watermark = self.event_loop.borrow().next_seq();
            crate::event_loop::drain_due_timers_before_seq(
                &self.event_loop,
                &mut self.vm,
                seq_watermark,
            );
        }

        let t_dcl = std::time::Instant::now();
        let (sr0, ly0) = {
            let h = self.host.borrow();
            (h.style_recalc_count, h.layout_count)
        };
        self.host.borrow_mut().ready_state = "interactive".to_string();
        let doc_root = self.host.borrow().document.borrow().root();
        let _ = self.dispatch_node_event(doc_root, "DOMContentLoaded", true, false, &[]);
        let dcl_ms = t_dcl.elapsed().as_secs_f32() * 1000.0;
        if dcl_ms >= 25.0 {
            let (sr1, ly1) = {
                let h = self.host.borrow();
                (h.style_recalc_count, h.layout_count)
            };
            tracing::debug!(
                phase = "DOMContentLoaded",
                ms = format_args!("{dcl_ms:.1}"),
                style_recalcs = sr1.saturating_sub(sr0),
                layouts = ly1.saturating_sub(ly0),
                "lifecycle profile"
            );
        }

        sync_all_iframes_in_document(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.web_apis,
            &self.net,
            &self.frames,
        );

        let t_load = std::time::Instant::now();
        let (sr0, ly0) = {
            let h = self.host.borrow();
            (h.style_recalc_count, h.layout_count)
        };
        self.host.borrow_mut().ready_state = "complete".to_string();
        crate::events::fire_window_document_event(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            doc_root,
            "load",
            false,
            false,
            "Event",
            &[],
        );
        crate::events::fire_window_document_event(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            doc_root,
            "pageshow",
            true,
            true,
            "PageTransitionEvent",
            &[("persisted", JsValue::FALSE)],
        );
        let load_ms = t_load.elapsed().as_secs_f32() * 1000.0;
        if load_ms >= 25.0 {
            let (sr1, ly1) = {
                let h = self.host.borrow();
                (h.style_recalc_count, h.layout_count)
            };
            tracing::debug!(
                phase = "load",
                ms = format_args!("{load_ms:.1}"),
                style_recalcs = sr1.saturating_sub(sr0),
                layouts = ly1.saturating_sub(ly0),
                "lifecycle profile"
            );
        }

        // Execute `<body onload="...">` attribute if present.
        let body_onload = {
            let doc_cell = Rc::clone(&self.host.borrow().document);
            let doc = doc_cell.borrow();
            let found = doc
                .descendants(doc.root())
                .find(|&n| {
                    matches!(
                        doc.get(n),
                        Some(NodeData::Element(e)) if e.name.local.as_str().eq_ignore_ascii_case("body")
                    )
                })
                .and_then(|body_id| match doc.get(body_id) {
                    Some(NodeData::Element(e)) => e.get_attribute("onload").map(str::to_string),
                    _ => None,
                });
            found
        };
        if let Some(code) = body_onload {
            if !code.trim().is_empty() {
                if let Err(err) = self.eval_script_cached(&code) {
                    let msg = format!("onload error: {err:?}");
                    self.web_apis
                        .borrow_mut()
                        .record_console("error", msg.clone());
                    errors.push(msg);
                }
            }
        }

        let t_drain = std::time::Instant::now();
        let (sr0, ly0) = {
            let h = self.host.borrow();
            (h.style_recalc_count, h.layout_count)
        };
        self.microtask_checkpoint();
        let tasks_ran = drain_ready_tasks_with_budget(
            &self.event_loop,
            &mut self.vm,
            512,
            Duration::from_millis(120),
        );
        let drain_ms = t_drain.elapsed().as_secs_f32() * 1000.0;
        if drain_ms >= 25.0 {
            let (sr1, ly1) = {
                let h = self.host.borrow();
                (h.style_recalc_count, h.layout_count)
            };
            let jst = self.jit.stats();
            tracing::debug!(
                phase = "drain_ready_tasks",
                tasks_ran,
                ms = format_args!("{drain_ms:.1}"),
                style_recalcs = sr1.saturating_sub(sr0),
                layouts = ly1.saturating_sub(ly0),
                jit_compiled = jst.compiled_functions,
                jit_t2l = jst.mem_backed_compiled,
                jit_b_ms = format_args!("{:.1}", jst.baseline_compile_us as f64 / 1000.0),
                jit_opt = jst.opt_compiled_functions,
                jit_o_ms = format_args!("{:.1}", jst.opt_compile_us as f64 / 1000.0),
                jit_entries = jst.baseline_entries,
                jit_opt_entries = jst.opt_entries,
                jit_deopts = jst.deopt_exits,
                gc_minor = self.vm.heap.minor_collections(),
                gc_major = self.vm.heap.major_collections(),
                "lifecycle profile"
            );
        }
        errors
    }

    /// Evaluates a JavaScript script using the persistent `StorageEngine`-backed
    /// [`BytecodeCache`] for medium/large scripts (`>= 8 KiB`) while compiling small
    /// inline scripts directly in RAM (matching SpiderMonkey JSBC / V8 CodeCache thresholds).
    pub fn eval_script_cached(&mut self, source: &str) -> Result<JsValue, EvalError> {
        let program = if source.len() < 2_048 {
            rix_js::compile(source, SourceType::Script).map_err(EvalError::Compile)?
        } else {
            let compiled = {
                let wa = self.web_apis.borrow();
                let cache = BytecodeCache::new(&wa.storage);
                cache.compile_or_load_with_ram(source, SourceType::Script)
            };
            let (prog, _status) = compiled.map_err(EvalError::Compile)?;
            prog
        };
        let val = self
            .vm
            .execute_program(program)
            .map_err(EvalError::Runtime)?;
        self.microtask_checkpoint();
        Ok(val)
    }

    /// Dispatches a real DOM event (`click`, `input`, `change`, `submit`, `keydown`, etc.)
    /// on `node`, invoking inline `on<type>` handlers, JS property handlers, and bubbling
    /// `EventTarget` listeners. Returns `true` if `preventDefault()` was NOT called.
    pub fn dispatch_node_event(
        &mut self,
        node: NodeId,
        ev_type: &str,
        bubbles: bool,
        cancelable: bool,
        props: &[(&str, JsValue)],
    ) -> bool {
        let res = fire_dom_node_event(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            node,
            ev_type,
            bubbles,
            cancelable,
            props,
        );
        self.microtask_checkpoint();
        match res {
            Ok(not_prevented) => not_prevented,
            Err(exc) => {
                let msg = format!("Uncaught exception in '{ev_type}' handler: {exc:?}");
                self.web_apis.borrow_mut().record_console("error", msg);
                true
            }
        }
    }

    /// Configures the Cranelift multi-tier JIT engine attached to this runtime:
    /// - `(true, true)`: full 3-tier JIT (Interpreter -> Tier-2 Baseline/OSR -> Tier-3 Optimizing)
    /// - `(true, false)`: Tier-2 Baseline/OSR only (disables Tier-3 speculation)
    /// - `(false, _)`: pure bytecode interpreter (JIT detached for strict security hardening)
    pub fn configure_jit(&mut self, enable_baseline: bool, enable_opt: bool) {
        self.configure_jit_with_cache(
            enable_baseline,
            enable_opt,
            rix_js::BASELINE_THRESHOLD,
            rix_js::OSR_THRESHOLD,
            rix_js::OPT_THRESHOLD,
            None,
        );
    }

    /// Configures the Cranelift multi-tier JIT engine with custom tier thresholds and an optional
    /// browser-scoped [`rix_jit::SharedBaselineCache`] for zero-recompile warm reloads.
    pub fn configure_jit_with_cache(
        &mut self,
        enable_baseline: bool,
        enable_opt: bool,
        baseline_threshold: u32,
        osr_threshold: u32,
        opt_threshold: u32,
        shared_cache: Option<rix_jit::SharedBaselineCache>,
    ) {
        if !enable_baseline {
            self.vm.detach_jit();
            return;
        }
        let eff_opt = if enable_opt { opt_threshold } else { u32::MAX };
        let jit = rix_jit::TieredJit::with_tier_thresholds_and_cache(
            baseline_threshold,
            osr_threshold,
            eff_opt,
            shared_cache,
        );
        self.vm.attach_jit(jit.clone());
        self.jit = jit;
    }

    /// Drains all captured JavaScript `console.*` messages (`(level, text)`) for DevHUD display.
    pub fn drain_console_messages(&mut self) -> Vec<(String, String)> {
        self.web_apis.borrow_mut().drain_console_messages()
    }

    /// Performs a non-blocking poll of pending async `fetch()`/WebSocket, Web Worker, and
    /// Service Worker completions, plus any due timers and queued macrotasks.
    /// Returns `true` if any task or event ran.
    pub fn poll_async_and_timers(&mut self) -> bool {
        let mut progressed = false;

        let net_events: Vec<AsyncNetEvent> = {
            let nb = self.net.borrow();
            let mut slot = nb.async_rx.borrow_mut();
            let mut out = Vec::new();
            if let Some(rx) = slot.as_mut() {
                while let Ok(ev) = rx.try_recv() {
                    out.push(ev);
                }
            }
            out
        };
        for ev in net_events {
            deliver_async_net_event(
                &mut self.vm,
                &self.host,
                &self.events,
                &self.event_loop,
                &self.web_apis,
                &self.streams,
                &self.net,
                ev,
            );
            progressed = true;
        }

        let worker_events: Vec<AsyncWorkerEvent> = {
            let ws = self.workers.borrow();
            let mut slot = ws.async_rx.borrow_mut();
            let mut out = Vec::new();
            if let Some(rx) = slot.as_mut() {
                while let Ok(ev) = rx.try_recv() {
                    out.push(ev);
                }
            }
            out
        };
        for ev in worker_events {
            deliver_async_worker_event(
                &mut self.vm,
                &self.host,
                &self.events,
                &self.event_loop,
                &self.web_apis,
                &self.frames,
                &self.workers,
                ev,
            );
            progressed = true;
        }

        let sw_events: Vec<AsyncSwEvent> = {
            let sw = self.service_workers.borrow();
            let mut slot = sw.async_rx.borrow_mut();
            let mut out = Vec::new();
            if let Some(rx) = slot.as_mut() {
                while let Ok(ev) = rx.try_recv() {
                    out.push(ev);
                }
            }
            out
        };
        for ev in sw_events {
            deliver_async_sw_event(
                &mut self.vm,
                &self.event_loop,
                &self.web_apis,
                &self.service_workers,
                ev,
            );
            progressed = true;
        }

        let ran = drain_ready_tasks_with_budget(
            &self.event_loop,
            &mut self.vm,
            256,
            Duration::from_millis(40),
        );
        if ran > 0 {
            progressed = true;
        }
        progressed
    }

    /// Returns the exact delay until this runtime next needs CPU time, or `None` when
    /// completely idle (`IDLE 0 Hz` — zero timers, zero rAF callbacks, zero queued tasks,
    /// and zero in-flight async `fetch()` / `WebSocket` I/O).
    #[must_use]
    pub fn next_wakeup_delay(&self) -> Option<Duration> {
        if self.vm.has_pending_microtasks() || self.event_loop.borrow().has_queued_macrotasks() {
            return Some(Duration::ZERO);
        }
        if self.event_loop.borrow().has_pending_raf() {
            return Some(Duration::from_millis(16));
        }
        let timer_delay = self.event_loop.borrow_mut().time_until_next_timer();
        if self.net.borrow().has_pending_io() {
            let io_poll = Duration::from_millis(8);
            return Some(match timer_delay {
                Some(t) => t.min(io_poll),
                None => io_poll,
            });
        }
        timer_delay
    }

    /// Evaluates a JavaScript script and immediately executes a WHATWG microtask checkpoint.
    pub fn eval_script(&mut self, source: &str) -> Result<JsValue, EvalError> {
        let val = self.vm.eval_script(source)?;
        self.microtask_checkpoint();
        Ok(val)
    }

    /// Runs a strict WHATWG microtask checkpoint (`vm.drain_microtasks` + observer hooks).
    pub fn microtask_checkpoint(&mut self) {
        run_microtask_checkpoint(&self.event_loop, &mut self.vm);
    }

    /// Executes one highest-priority ready macrotask or due timer, followed by a checkpoint.
    pub fn step_once(&mut self) -> bool {
        step_event_loop_once(&self.event_loop, &mut self.vm)
    }

    /// Advances virtual clock by `delta` and drains all newly due timers and microtasks.
    pub fn advance_and_drain(&mut self, delta: Duration) -> u64 {
        self.event_loop.borrow_mut().advance_clock(delta);
        drain_ready_tasks(&self.event_loop, &mut self.vm)
    }

    /// Fires a compositor VSync frame signal at `timestamp_ms`, executing W3C Web Animations
    /// `update animations and send events`, rAF callbacks, and `ResizeObserver`/`IntersectionObserver`
    /// layout observations in HTML §8.1.7.3 rendering order.
    pub fn fire_vsync(&mut self, timestamp_ms: f64) -> usize {
        self.host.borrow_mut().run_focus_fixup();
        update_animations_and_send_events(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.web_animations,
            timestamp_ms,
        );
        let ran = fire_vsync(&self.event_loop, &mut self.vm, timestamp_ms);
        media_elements::tick_playback_with_events(
            &mut self.vm,
            &self.host,
            &self.events,
            &self.event_loop,
            &self.media_source,
            &self.media_elements,
            timestamp_ms,
        );
        self.flush_layout_observers(timestamp_ms);
        ran
    }

    /// Evaluates `ResizeObserver` and `IntersectionObserver` entries against current layout
    /// geometry and runs a strict WHATWG microtask checkpoint.
    pub fn flush_layout_observers(&mut self, timestamp_ms: f64) {
        deliver_layout_observers(&mut self.vm, &self.host, &self.observers, timestamp_ms);
        self.microtask_checkpoint();
    }

    /// Reactively drives pending async network/WebSocket events, timers, and microtasks
    /// using `tokio::select!` (ZERO polling timers!) until `done(self)` returns `true`
    /// or `timeout` expires.
    pub async fn drive_async_until(
        &mut self,
        timeout: Duration,
        mut done: impl FnMut(&mut Self) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = {
            let nb = self.net.borrow();
            let mut slot = nb.async_rx.borrow_mut();
            slot.take()
        };
        let Some(mut net_rx) = rx.take() else {
            return done(self);
        };
        let mut wrx = {
            let ws = self.workers.borrow();
            let mut slot = ws.async_rx.borrow_mut();
            slot.take()
        };
        let Some(mut worker_rx) = wrx.take() else {
            {
                let nb = self.net.borrow();
                *nb.async_rx.borrow_mut() = Some(net_rx);
            }
            return done(self);
        };
        let mut swrx = {
            let sw = self.service_workers.borrow();
            let mut slot = sw.async_rx.borrow_mut();
            slot.take()
        };
        let Some(mut sw_rx) = swrx.take() else {
            {
                let nb = self.net.borrow();
                *nb.async_rx.borrow_mut() = Some(net_rx);
            }
            {
                let ws = self.workers.borrow();
                *ws.async_rx.borrow_mut() = Some(worker_rx);
            }
            return done(self);
        };

        let result = loop {
            drain_ready_tasks(&self.event_loop, &mut self.vm);
            while let Ok(ev) = net_rx.try_recv() {
                deliver_async_net_event(
                    &mut self.vm,
                    &self.host,
                    &self.events,
                    &self.event_loop,
                    &self.web_apis,
                    &self.streams,
                    &self.net,
                    ev,
                );
                drain_ready_tasks(&self.event_loop, &mut self.vm);
            }
            while let Ok(ev) = worker_rx.try_recv() {
                deliver_async_worker_event(
                    &mut self.vm,
                    &self.host,
                    &self.events,
                    &self.event_loop,
                    &self.web_apis,
                    &self.frames,
                    &self.workers,
                    ev,
                );
                drain_ready_tasks(&self.event_loop, &mut self.vm);
            }
            while let Ok(ev) = sw_rx.try_recv() {
                deliver_async_sw_event(
                    &mut self.vm,
                    &self.event_loop,
                    &self.web_apis,
                    &self.service_workers,
                    ev,
                );
                drain_ready_tasks(&self.event_loop, &mut self.vm);
            }
            if done(self) {
                break true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break false;
            }
            let next_wake = match self.event_loop.borrow_mut().time_until_next_timer() {
                Some(dur) => (now + dur).min(deadline),
                None => deadline,
            };
            tokio::select! {
                biased;
                maybe_ev = net_rx.recv() => {
                    if let Some(ev) = maybe_ev {
                        deliver_async_net_event(
                            &mut self.vm,
                            &self.host,
                            &self.events,
                            &self.event_loop,
                            &self.web_apis,
                            &self.streams,
                            &self.net,
                            ev,
                        );
                    } else {
                        break done(self);
                    }
                }
                maybe_wev = worker_rx.recv() => {
                    if let Some(ev) = maybe_wev {
                        deliver_async_worker_event(
                            &mut self.vm,
                            &self.host,
                            &self.events,
                            &self.event_loop,
                            &self.web_apis,
                            &self.frames,
                            &self.workers,
                            ev,
                        );
                    } else {
                        break done(self);
                    }
                }
                maybe_swev = sw_rx.recv() => {
                    if let Some(ev) = maybe_swev {
                        deliver_async_sw_event(
                            &mut self.vm,
                            &self.event_loop,
                            &self.web_apis,
                            &self.service_workers,
                            ev,
                        );
                    } else {
                        break done(self);
                    }
                }
                () = tokio::time::sleep_until(next_wake) => {}
            }
        };

        {
            let nb = self.net.borrow();
            *nb.async_rx.borrow_mut() = Some(net_rx);
        }
        {
            let ws = self.workers.borrow();
            *ws.async_rx.borrow_mut() = Some(worker_rx);
        }
        {
            let sw = self.service_workers.borrow();
            *sw.async_rx.borrow_mut() = Some(sw_rx);
        }
        result
    }

    /// Attaches a compositor-owned [`rix_render::SharedGpuDevice`] so WebGPU/WebGL2 canvases
    /// and `<video>` YUV->RGBA shader layers share the same `wgpu::Device` as `VelloRenderer`.
    pub fn attach_shared_gpu(&mut self, shared: rix_render::SharedGpuDevice) {
        self.webgpu.borrow_mut().attach_shared_gpu(shared.clone());
        self.webgl2.borrow_mut().attach_shared_gpu(shared.clone());
        self.media_elements.borrow_mut().attach_shared_gpu(shared);
    }

    /// Synchronizes any dirty page `DisplayList` (only when DOM/CSS changed!) AND
    /// presents all dirty GPU-backed `Canvas2D` / `WebGPU` / `WebGL2` / `<video>` external surfaces
    /// into `compositor`. Returns `(display_list_rebuilt, surfaces_presented)`.
    pub fn sync_frame_to_compositor(
        &mut self,
        pipeline: PipelineId,
        epoch: u64,
        compositor: &mut Compositor,
        mut gpu: Option<&mut VelloRenderer>,
    ) -> (bool, usize) {
        let dl_rebuilt = if let Some(dl) = self
            .host
            .borrow_mut()
            .paint_if_dirty(pipeline, &mut compositor.resources)
        {
            compositor.submit_display_list(pipeline, LayerRole::WebSurface, epoch, dl);
            true
        } else {
            false
        };
        let canvases2d = self
            .canvas2d
            .borrow_mut()
            .present_dirty_to_compositor(compositor, gpu.as_deref_mut());
        let canvases_gpu = self
            .webgpu
            .borrow_mut()
            .present_dirty_to_compositor(compositor, gpu.as_deref_mut());
        let canvases_gl = self
            .webgl2
            .borrow_mut()
            .present_dirty_to_compositor(compositor, gpu.as_deref_mut());
        let videos = self
            .media_elements
            .borrow_mut()
            .present_dirty_to_compositor(compositor, gpu);
        (dl_rebuilt, canvases2d + canvases_gpu + canvases_gl + videos)
    }

    /// Runs a full Major GC across VM roots, event loop callbacks, listeners, observers,
    /// Web APIs, Streams, Canvas2D, WebGPU, WebGL2, WebAudio, MediaSource, active network handles,
    /// and host-pinned DOM wrappers.
    pub fn collect_garbage(&mut self) -> GcCycleStats {
        let roots = self.vm.collect_roots();
        let stats = self.vm.heap.collect_major(&roots);
        self.events.borrow_mut().sweep_dead(&self.vm.heap);
        self.web_apis.borrow_mut().sweep_dead(&self.vm.heap);
        self.web_animations.borrow_mut().sweep_dead(&self.vm.heap);
        self.streams.borrow_mut().sweep_dead(&self.vm.heap);
        self.net.borrow_mut().sweep_dead(&self.vm.heap);
        self.canvas2d.borrow_mut().sweep_dead(&self.vm.heap);
        self.webgpu.borrow_mut().sweep_dead(&self.vm.heap);
        self.webgl2.borrow_mut().sweep_dead(&self.vm.heap);
        self.frames.borrow_mut().sweep_dead(&self.vm.heap);
        self.workers.borrow_mut().sweep_dead(&self.vm.heap);
        self.service_workers.borrow_mut().sweep_dead(&self.vm.heap);
        self.indexeddb.borrow_mut().sweep_dead(&self.vm.heap);
        self.media_source.borrow_mut().sweep_dead(&self.vm.heap);
        self.webaudio.borrow_mut().sweep_dead(&self.vm.heap);
        self.host.borrow_mut().sweep_dead_wrappers(&self.vm.heap);
        stats
    }
}

fn lookup_proto_data_prop(
    vm: &JsVm,
    start: rix_js::HeapHandle,
    key: rix_js::PropertyKey,
) -> Option<JsValue> {
    let mut cur = Some(start);
    let mut depth = 0u8;
    while let Some(h) = cur {
        if depth > 16 {
            break;
        }
        depth += 1;
        if let Some(p) = vm.heap.object_get_own(h, key, &vm.shapes) {
            return Some(p.value);
        }
        cur = vm.heap.get_object(h).and_then(|o| o.prototype);
    }
    None
}

fn format_eval_error(vm: &JsVm, err: &EvalError) -> String {
    match err {
        EvalError::Compile(ce) => format!("SyntaxError: {ce}"),
        EvalError::Runtime(exc) => {
            if let Some(h) = exc.value.as_object() {
                let name = lookup_proto_data_prop(vm, h, vm.key_name)
                    .filter(|v| !v.is_undefined())
                    .map(|v| vm.value_to_rust_string(v));
                let msg = lookup_proto_data_prop(vm, h, vm.key_message)
                    .filter(|v| !v.is_undefined())
                    .map(|v| vm.value_to_rust_string(v))
                    .unwrap_or_default();
                if let Some(n) = name {
                    return if msg.is_empty() {
                        n
                    } else {
                        format!("{n}: {msg}")
                    };
                }
                if !msg.is_empty() {
                    return msg;
                }
            }
            if !exc.message.is_empty() {
                return exc.message.clone();
            }
            vm.value_to_rust_string(exc.value)
        }
    }
}
