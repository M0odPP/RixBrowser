#![deny(unsafe_code)]
//! Content-process runtime integrating `rix-js`, `rix-dom`, `rix-html`,
//! `rix-css`, `rix-layout`, `rix-net`, and `rix-storage` with a reactive
//! WHATWG Event Loop, zero-leak Weak DOM Bridge (`WeakTable<NodeId>`),
//! and core WHATWG Web Platform APIs.
//!
//! Reference: Spec 05 §2.2, Spec 06 §1–§3, Issues #16 & #17.

pub mod dom_bridge;
pub mod event_loop;
pub mod events;
pub mod observers;
pub mod web_apis;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_core::Size;
use rix_dom::{Document, NodeData};
use rix_html::{attach_declarative_shadow_roots, HtmlParserStream, ParserStreamStep};
use rix_js::{EvalError, GcCycleStats, JsValue, JsVm};
use rix_storage::StorageEngine;
use rix_text::FontDatabase;

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
pub use observers::{
    deliver_layout_observers, deliver_mutation_observers, install_observers, ObserverSystem,
    SharedObservers,
};
pub use web_apis::{install_web_apis, SharedWebApis, WebApiState, DEFAULT_DOCUMENT_URL};

/// Complete single-origin Content Process execution context owning a `JsVm`,
/// Weak DOM Bridge (`HostState`), Event System, Observers, Core Web APIs,
/// and WHATWG `EventLoopState`.
pub struct ContentRuntime {
    pub vm: JsVm,
    pub host: SharedHost,
    pub event_loop: SharedEventLoop,
    pub events: SharedEvents,
    pub observers: SharedObservers,
    pub web_apis: SharedWebApis,
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
        let mut vm = JsVm::new();
        let event_loop = Rc::new(RefCell::new(EventLoopState::new()));
        install_event_loop_globals(&mut vm, &event_loop);
        let host = install_dom_bridge(&mut vm, document, viewport, font_db);
        let events = install_event_system(&mut vm, &host, &event_loop);
        let observers = install_observers(&mut vm, &host, &event_loop);
        let web_apis = install_web_apis(&mut vm, &host, &events, &event_loop, origin_url, storage);
        Self {
            vm,
            host,
            event_loop,
            events,
            observers,
            web_apis,
        }
    }

    /// Creates a runtime backed by an empty document and default `800x600` viewport.
    #[must_use]
    pub fn with_empty_document() -> Self {
        let doc = Rc::new(RefCell::new(Document::new()));
        let font_db = Arc::new(FontDatabase::new());
        Self::new(doc, Size::new(800.0, 600.0), font_db)
    }

    /// Updates the current document URL used by `location`, `history`, and `localStorage`.
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
                    if is_classic && !code.trim().is_empty() {
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
        self.microtask_checkpoint();
        Ok(())
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

    /// Runs a full Major GC across VM roots, event loop callbacks, listeners, observers,
    /// Web APIs, and host-pinned DOM wrappers, then sweeps all dead unpinned handles cleanly.
    pub fn collect_garbage(&mut self) -> GcCycleStats {
        let mut roots = self.vm.collect_roots();
        self.event_loop.borrow().collect_roots(&mut roots);
        self.events.borrow().collect_roots(&mut roots);
        self.observers.borrow().collect_roots(&mut roots);
        self.web_apis.borrow().collect_roots(&mut roots);
        self.host.borrow().collect_roots(&mut roots);
        let stats = self.vm.heap.collect_major(&roots);
        self.events.borrow_mut().sweep_dead(&self.vm.heap);
        self.web_apis.borrow_mut().sweep_dead(&self.vm.heap);
        self.host.borrow_mut().sweep_dead_wrappers(&self.vm.heap);
        stats
    }
}
