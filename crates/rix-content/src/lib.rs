#![deny(unsafe_code)]
//! Content-process runtime integrating `rix-js`, `rix-dom`, `rix-html`,
//! `rix-css`, `rix-layout`, `rix-net`, and `rix-storage` with a reactive
//! WHATWG Event Loop, zero-leak Weak DOM Bridge (`WeakTable<NodeId>`),
//! Fetch CORS/preflight, RFC 6455 WebSocket, Streams, and Web Storage.
//!
//! Reference: Spec 05 §2.2, Spec 06 §1–§3, Issues #16 & #17.

pub mod canvas2d;
pub mod dom_bridge;
pub mod event_loop;
pub mod events;
pub mod fetch_ws;
pub mod frames;
pub mod observers;
pub mod service_worker;
pub mod streams;
pub mod web_apis;
pub mod workers;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_core::Size;
use rix_dom::{Document, NodeData, NodeId};
use rix_html::{attach_declarative_shadow_roots, HtmlParserStream, ParserStreamStep};
use rix_ipc::{ContentToBrowser, MessagePortId, PipelineId};
use rix_js::{EvalError, GcCycleStats, JsValue, JsVm};
use rix_net::FetchEngine;
use rix_render::{Compositor, DisplayList, LayerRole, ResourceRegistry, VelloRenderer};
use rix_storage::StorageEngine;
use rix_text::FontDatabase;

pub use canvas2d::{install_canvas2d, Canvas2DSystem, SharedCanvas2D};
pub use frames::{
    bind_child_frame, deliver_ipc_port_message, deliver_ipc_post_message,
    install_frame_and_messaging_apis, mark_child_frame_crashed, sync_all_iframes_in_document,
    FrameSystem, SandboxFlags, SharedFrames,
};

pub use dom_bridge::{
    install_dom_bridge, wrap_node, CustomElementDef, DomMutation, DomPrototypes, HostState,
    MutationSink, SharedHost,
};
pub use event_loop::{
    drain_ready_tasks, fire_vsync, install_event_loop_globals, run_microtask_checkpoint,
    step_event_loop_once, wait_for_next_event_or_timer, EventLoopState, ExternalEvent,
    MacrotaskAction, MicrotaskCheckpointHook, SharedEventLoop, TaskPriority,
};
pub use events::{
    fire_object_event, install_event_system, EventSystem, EventTargetKey, SharedEvents,
};
pub use fetch_ws::{
    deliver_async_net_event, install_fetch_and_websocket, AsyncNetEvent, FetchFailure,
    NetworkBridge, SharedNetBridge,
};
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
pub use web_apis::{install_web_apis, SharedWebApis, WebApiState, DEFAULT_DOCUMENT_URL};
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
    pub streams: SharedStreams,
    pub net: SharedNetBridge,
    pub canvas2d: SharedCanvas2D,
    pub frames: SharedFrames,
    pub workers: SharedWorkers,
    pub service_workers: SharedServiceWorkers,
    pub jit: Rc<rix_jit::TieredJit>,
    pub wasm: rix_jit::SharedWasmHost,
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
        let web_apis = install_web_apis(&mut vm, &host, &events, &event_loop, origin_url, storage);
        let streams = install_streams(&mut vm, &web_apis);
        let net = install_fetch_and_websocket(&mut vm, &host, &web_apis, &streams, fetch_engine);
        let canvas2d = install_canvas2d(&mut vm, &host, &web_apis);
        let frames =
            install_frame_and_messaging_apis(&mut vm, &host, &events, &event_loop, &web_apis, &net);
        let workers = install_worker_and_sse_apis(
            &mut vm,
            &host,
            &events,
            &event_loop,
            &web_apis,
            &net,
            &frames,
        );
        let service_workers =
            install_service_worker_and_cache_apis(&mut vm, &host, &web_apis, &streams, &net);
        Self {
            vm,
            host,
            event_loop,
            events,
            observers,
            web_apis,
            streams,
            net,
            canvas2d,
            frames,
            workers,
            service_workers,
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
                        self.eval_script(&code)?;
                    }
                }
                ParserStreamStep::Done => break,
            }
        }

        parser.borrow_mut().finish();
        {
            let doc_cell = Rc::clone(&self.host.borrow().document);
            let mut doc = doc_cell.borrow_mut();
            attach_declarative_shadow_roots(&mut doc);
        }
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

    /// Fires a compositor VSync frame signal at `timestamp_ms`, executing rAF callbacks
    /// followed by `ResizeObserver` and `IntersectionObserver` layout observations.
    pub fn fire_vsync(&mut self, timestamp_ms: f64) -> usize {
        let ran = fire_vsync(&self.event_loop, &mut self.vm, timestamp_ms);
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

    /// Synchronizes any dirty page `DisplayList` (only when DOM/CSS changed!) AND
    /// presents all dirty GPU-backed `Canvas2D` external surfaces into `compositor`.
    /// Returns `(display_list_rebuilt, canvases_presented)`.
    pub fn sync_frame_to_compositor(
        &mut self,
        pipeline: PipelineId,
        epoch: u64,
        compositor: &mut Compositor,
        gpu: Option<&mut VelloRenderer>,
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
        let canvases = self
            .canvas2d
            .borrow_mut()
            .present_dirty_to_compositor(compositor, gpu);
        (dl_rebuilt, canvases)
    }

    /// Runs a full Major GC across VM roots, event loop callbacks, listeners, observers,
    /// Web APIs, Streams, Canvas2D, active network handles, and host-pinned DOM wrappers.
    pub fn collect_garbage(&mut self) -> GcCycleStats {
        let mut roots = self.vm.collect_roots();
        self.event_loop.borrow().collect_roots(&mut roots);
        self.events.borrow().collect_roots(&mut roots);
        self.observers.borrow().collect_roots(&mut roots);
        self.web_apis.borrow().collect_roots(&mut roots);
        self.streams.borrow().collect_roots(&mut roots);
        self.net.borrow().collect_roots(&mut roots);
        self.canvas2d.borrow().collect_roots(&mut roots);
        self.frames.borrow().collect_roots(&mut roots);
        self.workers.borrow().collect_roots(&mut roots);
        self.service_workers.borrow().collect_roots(&mut roots);
        self.host.borrow().collect_roots(&mut roots);
        let stats = self.vm.heap.collect_major(&roots);
        self.events.borrow_mut().sweep_dead(&self.vm.heap);
        self.web_apis.borrow_mut().sweep_dead(&self.vm.heap);
        self.streams.borrow_mut().sweep_dead(&self.vm.heap);
        self.net.borrow_mut().sweep_dead(&self.vm.heap);
        self.canvas2d.borrow_mut().sweep_dead(&self.vm.heap);
        self.frames.borrow_mut().sweep_dead(&self.vm.heap);
        self.workers.borrow_mut().sweep_dead(&self.vm.heap);
        self.service_workers.borrow_mut().sweep_dead(&self.vm.heap);
        self.host.borrow_mut().sweep_dead_wrappers(&self.vm.heap);
        stats
    }
}
