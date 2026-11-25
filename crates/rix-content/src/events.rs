//! Shadow-DOM-aware WHATWG `EventTarget`, `Event`, `CustomEvent`, `UIEvent`,
//! `FocusEvent`, `MouseEvent`, `WheelEvent`, `PointerEvent`, `KeyboardEvent`,
//! `CompositionEvent`, `InputEvent`, `TextEvent`, `DragEvent`, `BeforeUnloadEvent`,
//! `HashChangeEvent`, `StorageEvent`, `DeviceMotionEvent`, `DeviceOrientationEvent`,
//! `Document.prototype.createEvent`, `GlobalEventHandlers`, and `WindowEventHandlers`.
//!
//! Reference: WHATWG DOM §2, §4.5, WHATWG HTML §8.1.7.2, Spec 06 §3 & Issue #16.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use rix_dom::{NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{wrap_node, SharedHost};
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::web_apis::create_dom_exception;

/// Unique target key supporting `window`, DOM `NodeId`s, and standalone `new EventTarget()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetKey {
    Window,
    Node(NodeId),
    Object(HeapHandle),
}

#[derive(Debug, Clone, Copy)]
struct ListenerEntry {
    /// Original JS callback function or `{ handleEvent }` listener object.
    listener: HeapHandle,
    capture: bool,
    once: bool,
    passive: bool,
}

#[derive(Debug, Clone)]
struct EventInternal {
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    is_trusted: bool,
    initialized: bool,
    dispatch_flag: bool,
    default_prevented: bool,
    propagation_stopped: bool,
    immediate_stopped: bool,
    in_passive_listener: bool,
    event_phase: i32,
    time_stamp: f64,
    target: JsValue,
    current_target: JsValue,
    composed_path: Vec<JsValue>,
    path_keys: Vec<EventTargetKey>,
    current_target_key: Option<EventTargetKey>,
}

/// Shared registry of active event listeners, IDL event handlers, and event internal state.
pub struct EventSystem {
    listeners: HashMap<(EventTargetKey, String), Vec<ListenerEntry>>,
    events: HashMap<HeapHandle, EventInternal>,
    close_events: HashMap<HeapHandle, (bool, u16, String)>,
    event_handlers: HashMap<(EventTargetKey, String), JsValue>,
    idl_handler_trampolines: HashMap<(EventTargetKey, String), HeapHandle>,
    pub event_proto: Option<HeapHandle>,
    pub event_protos: HashMap<&'static str, HeapHandle>,
    pub(crate) on_uncaught_error: Option<crate::event_loop::UncaughtErrorHook>,
    start_instant: Instant,
}

impl Default for EventSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle to [`EventSystem`].
pub type SharedEvents = Rc<RefCell<EventSystem>>;

impl EventSystem {
    #[must_use]
    pub fn new() -> Self {
        Self {
            listeners: HashMap::new(),
            events: HashMap::new(),
            close_events: HashMap::new(),
            event_handlers: HashMap::new(),
            idl_handler_trampolines: HashMap::new(),
            event_proto: None,
            event_protos: HashMap::new(),
            on_uncaught_error: None,
            start_instant: Instant::now(),
        }
    }

    #[must_use]
    fn now_ms(&self) -> f64 {
        self.start_instant.elapsed().as_secs_f64() * 1000.0
    }

    /// Collects GC roots for all registered event listener callbacks, handlers, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        if let Some(ep) = self.event_proto {
            out.push(JsValue::from_object(ep));
        }
        for &p in self.event_protos.values() {
            out.push(JsValue::from_object(p));
        }
        for list in self.listeners.values() {
            for entry in list {
                out.push(JsValue::from_object(entry.listener));
            }
        }
        for &v in self.event_handlers.values() {
            out.push(v);
        }
        for &tramp in self.idl_handler_trampolines.values() {
            out.push(JsValue::from_object(tramp));
        }
    }

    /// Drops event internal state entries for collected JS event objects.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.events.retain(|&h, _| heap.is_alive(h));
        self.close_events.retain(|&h, _| heap.is_alive(h));
        self.listeners.retain(|(k, _), list| {
            let key_alive = match *k {
                EventTargetKey::Object(h) => heap.is_alive(h),
                EventTargetKey::Window | EventTargetKey::Node(_) => true,
            };
            key_alive && !list.is_empty()
        });
        self.event_handlers.retain(|(k, _), _| match *k {
            EventTargetKey::Object(h) => heap.is_alive(h),
            EventTargetKey::Window | EventTargetKey::Node(_) => true,
        });
        self.idl_handler_trampolines.retain(|(k, _), _| match *k {
            EventTargetKey::Object(h) => heap.is_alive(h),
            EventTargetKey::Window | EventTargetKey::Node(_) => true,
        });
    }

    fn has_any_listeners_for_node(&self, nid: NodeId) -> bool {
        self.listeners
            .iter()
            .any(|((k, _), list)| *k == EventTargetKey::Node(nid) && !list.is_empty())
    }

    pub(crate) fn is_event_object(&self, h: HeapHandle) -> bool {
        self.events.contains_key(&h)
    }
}

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

fn define_global(vm: &mut JsVm, name: &str, val: JsValue) {
    let k = vm.heap.intern_key(name);
    let g = vm.global;
    vm.heap
        .object_define_data(g, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

fn define_host_method(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let func = vm.alloc_host_fn(name, f);
    vm.heap.object_define_data(
        target,
        key,
        JsValue::from_object(func),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn define_host_getter(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let g = vm.alloc_host_fn(name, f);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g),
        None,
        PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn define_host_accessor(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    get: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
    set: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let key = vm.heap.intern_key(name);
    let g = vm.alloc_host_fn(name, get);
    let s = vm.alloc_host_fn(name, set);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(g),
        Some(s),
        PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

/// Defines the `[LegacyUnforgeable]` own non-configurable `isTrusted` accessor on an Event instance.
fn define_own_is_trusted(vm: &mut JsVm, events: &SharedEvents, ev_h: HeapHandle) {
    let e = Rc::clone(events);
    let k = vm.heap.intern_key("isTrusted");
    let g = vm.alloc_host_fn("get isTrusted", move |_vm, t, _| {
        let trusted = t
            .as_object()
            .is_some_and(|h| e.borrow().events.get(&h).is_some_and(|ev| ev.is_trusted));
        Ok(JsValue::from_bool(trusted))
    });
    vm.heap.object_define_accessor(
        ev_h,
        k,
        Some(g),
        None,
        PropertyAttrs::ENUMERABLE,
        &mut vm.shapes,
    );
}

fn target_key_for_value(
    vm: &JsVm,
    host: &SharedHost,
    val: JsValue,
) -> Result<EventTargetKey, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(EventTargetKey::Window);
    }
    let Some(h) = val.as_object() else {
        return Err(JsException {
            value: JsValue::UNDEFINED,
            message: "TypeError: EventTarget receiver is not an object".into(),
        });
    };
    if h == vm.global {
        return Ok(EventTargetKey::Window);
    }
    if let Some(nid) = host.borrow().node_for_handle(vm, h) {
        return Ok(EventTargetKey::Node(nid));
    }
    Ok(EventTargetKey::Object(h))
}

fn value_for_target_key(vm: &mut JsVm, host: &SharedHost, key: EventTargetKey) -> JsValue {
    match key {
        EventTargetKey::Window => JsValue::from_object(vm.global),
        EventTargetKey::Node(nid) => wrap_node(vm, host, nid),
        EventTargetKey::Object(h) => JsValue::from_object(h),
    }
}

/// WHATWG DOM §2.7 "default passive value" for `touchstart`/`touchmove`/`wheel`/`mousewheel`
/// on `Window`, `Document`, `document.documentElement`, and `document.body`.
fn default_passive_value(host: &SharedHost, target_key: EventTargetKey, ev_type: &str) -> bool {
    if !matches!(ev_type, "touchstart" | "touchmove" | "wheel" | "mousewheel") {
        return false;
    }
    match target_key {
        EventTargetKey::Window => true,
        EventTargetKey::Object(_) => false,
        EventTargetKey::Node(nid) => {
            let doc_rc = Rc::clone(&host.borrow().document);
            let doc = doc_rc.borrow();
            match doc.get(nid) {
                Some(NodeData::Document) => true,
                Some(NodeData::Element(e)) => {
                    let root = host.borrow().owner_doc_of(nid);
                    if doc.document_element_of(root) == Some(nid) {
                        return true;
                    }
                    e.name.local.as_str().eq_ignore_ascii_case("body")
                        && doc
                            .parent(nid)
                            .is_some_and(|p| doc.document_element_of(root) == Some(p))
                }
                _ => false,
            }
        }
    }
}

struct ParsedListenerOptions {
    capture: bool,
    once: bool,
    passive: bool,
    signal: Option<HeapHandle>,
}

fn parse_listener_options(
    vm: &mut JsVm,
    host: &SharedHost,
    target_key: EventTargetKey,
    ev_type: &str,
    opt_val: JsValue,
) -> Result<ParsedListenerOptions, JsException> {
    let def_passive = default_passive_value(host, target_key, ev_type);
    if opt_val.is_undefined() || opt_val.is_null() {
        return Ok(ParsedListenerOptions {
            capture: false,
            once: false,
            passive: def_passive,
            signal: None,
        });
    }
    if let Some(h) = opt_val.as_object() {
        let k_cap = vm.heap.intern_key("capture");
        let k_once = vm.heap.intern_key("once");
        let k_pas = vm.heap.intern_key("passive");
        let k_sig = vm.heap.intern_key("signal");
        let cap = vm
            .get_property_by_key(h, k_cap)
            .is_ok_and(|v| vm.to_boolean(v));
        let once = vm
            .get_property_by_key(h, k_once)
            .is_ok_and(|v| vm.to_boolean(v));
        let pas_val = vm.get_property_by_key(h, k_pas)?;
        let passive = if pas_val.is_undefined() {
            def_passive
        } else {
            vm.to_boolean(pas_val)
        };
        let sig_val = vm.get_property_by_key(h, k_sig)?;
        let signal = if sig_val.is_undefined() {
            None
        } else {
            let Some(sh) = sig_val.as_object() else {
                return Err(vm.type_error("addEventListener signal option must be an AbortSignal"));
            };
            Some(sh)
        };
        return Ok(ParsedListenerOptions {
            capture: cap,
            once,
            passive,
            signal,
        });
    }
    Ok(ParsedListenerOptions {
        capture: vm.to_boolean(opt_val),
        once: false,
        passive: def_passive,
        signal: None,
    })
}

fn extract_listener_handle(vm: &mut JsVm, val: JsValue) -> Result<Option<HeapHandle>, JsException> {
    if val.is_null() || val.is_undefined() {
        return Ok(None);
    }
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Event listener must be an object or function"));
    };
    Ok(Some(h))
}

/// Resolves prototype for `new` / `super()` calls using `vm.native_new_target`.
fn resolve_new_target_proto(
    vm: &mut JsVm,
    default_proto: HeapHandle,
    ctor_name: &str,
) -> Result<HeapHandle, JsException> {
    if vm.native_new_target.is_undefined() {
        return Err(vm.type_error(format!(
            "Failed to construct '{ctor_name}': Please use the 'new' operator"
        )));
    }
    if let Some(nt_h) = vm.native_new_target.as_object() {
        let k_proto = vm.key_prototype;
        if let Ok(pv) = vm.get_property_by_key(nt_h, k_proto) {
            if let Some(ph) = pv.as_object() {
                return Ok(ph);
            }
        }
    }
    Ok(default_proto)
}

/// Installs `EventTarget.prototype` methods, all DOM Event constructors, `Document.createEvent`,
/// `GlobalEventHandlers`, and `WindowEventHandlers` on `vm`.
pub fn install_event_system(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
) -> SharedEvents {
    let events = Rc::new(RefCell::new(EventSystem::new()));
    let etp = host.borrow().prototypes.event_target_proto;

    // HTMLElement.prototype.click() dispatches a real bubbling, cancelable MouseEvent.
    let ep = host.borrow().prototypes.element_proto;
    let h_click = Rc::clone(host);
    let evs_click = Rc::clone(&events);
    let el_click = Rc::clone(event_loop);
    define_host_method(vm, ep, "click", move |vm, this_val, _| {
        let nid = h_click.borrow().node_for_value(vm, this_val);
        if let Some(nid) = nid {
            let _ = fire_dom_node_event(
                vm,
                &h_click,
                &evs_click,
                &el_click,
                nid,
                "click",
                true,
                true,
                &[],
            );
        }
        Ok(JsValue::UNDEFINED)
    });

    // EventTarget.prototype.addEventListener(type, listener, options?)
    let h = Rc::clone(host);
    let evs = Rc::clone(&events);
    define_host_method(vm, etp, "addEventListener", move |vm, this_val, args| {
        let key = target_key_for_value(vm, &h, this_val)
            .map_err(|_| vm.type_error("addEventListener called on incompatible receiver"))?;
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let Some(listener_h) = extract_listener_handle(vm, arg(args, 1))? else {
            return Ok(JsValue::UNDEFINED);
        };
        let opts = parse_listener_options(vm, &h, key, &ev_type, arg(args, 2))?;
        if let Some(sig_h) = opts.signal {
            let k_ab = vm.heap.intern_key("aborted");
            if vm
                .get_property_by_key(sig_h, k_ab)
                .is_ok_and(|v| vm.to_boolean(v))
            {
                return Ok(JsValue::UNDEFINED);
            }
        }
        let added = {
            let mut es = evs.borrow_mut();
            let list = es.listeners.entry((key, ev_type.clone())).or_default();
            if !list
                .iter()
                .any(|e| e.listener == listener_h && e.capture == opts.capture)
            {
                list.push(ListenerEntry {
                    listener: listener_h,
                    capture: opts.capture,
                    once: opts.once,
                    passive: opts.passive,
                });
                true
            } else {
                false
            }
        };
        if added {
            if let Some(sig_h) = opts.signal {
                let evs_rm = Rc::clone(&evs);
                let ev_type_rm = ev_type;
                let cap_rm = opts.capture;
                let cleanup_fn = vm.alloc_host_fn("abort_remove_listener", move |_vm, _, _| {
                    let mut es = evs_rm.borrow_mut();
                    if let Some(list) = es.listeners.get_mut(&(key, ev_type_rm.clone())) {
                        list.retain(|e| !(e.listener == listener_h && e.capture == cap_rm));
                    }
                    Ok(JsValue::UNDEFINED)
                });
                evs.borrow_mut()
                    .listeners
                    .entry((EventTargetKey::Object(sig_h), "abort".to_string()))
                    .or_default()
                    .push(ListenerEntry {
                        listener: cleanup_fn,
                        capture: false,
                        once: true,
                        passive: false,
                    });
            }
            if let EventTargetKey::Node(nid) = key {
                h.borrow_mut().wrappers.set_pinned(&nid, true);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // EventTarget.prototype.removeEventListener(type, listener, options?)
    let h = Rc::clone(host);
    let evs = Rc::clone(&events);
    define_host_method(vm, etp, "removeEventListener", move |vm, this_val, args| {
        let key = target_key_for_value(vm, &h, this_val)
            .map_err(|_| vm.type_error("removeEventListener called on incompatible receiver"))?;
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let Some(listener_h) = extract_listener_handle(vm, arg(args, 1))? else {
            return Ok(JsValue::UNDEFINED);
        };
        let capture = {
            let opt = arg(args, 2);
            if let Some(oh) = opt.as_object() {
                let k_cap = vm.heap.intern_key("capture");
                vm.get_property_by_key(oh, k_cap)
                    .is_ok_and(|v| vm.to_boolean(v))
            } else {
                vm.to_boolean(opt)
            }
        };
        let still_pinned = {
            let mut es = evs.borrow_mut();
            if let Some(list) = es.listeners.get_mut(&(key, ev_type)) {
                list.retain(|e| !(e.listener == listener_h && e.capture == capture));
            }
            match key {
                EventTargetKey::Node(nid) => es.has_any_listeners_for_node(nid),
                _ => true,
            }
        };
        if let EventTargetKey::Node(nid) = key {
            let is_doc_root = h.borrow().document.borrow().root() == nid;
            if !still_pinned && !is_doc_root {
                h.borrow_mut().wrappers.set_pinned(&nid, false);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // EventTarget.prototype.dispatchEvent(event)
    let h = Rc::clone(host);
    let evs = Rc::clone(&events);
    let el = Rc::clone(event_loop);
    define_host_method(vm, etp, "dispatchEvent", move |vm, this_val, args| {
        let target_key = target_key_for_value(vm, &h, this_val)
            .map_err(|_| vm.type_error("dispatchEvent called on incompatible receiver"))?;
        let Some(ev_h) = arg(args, 0).as_object() else {
            return Err(vm.type_error("dispatchEvent requires an Event object"));
        };
        let not_canceled = dispatch_event_internal(vm, &h, &evs, &el, target_key, ev_h)?;
        Ok(JsValue::from_bool(not_canceled))
    });

    // Constructible `new EventTarget()` supporting ES6 subclassing (`class Sub extends EventTarget`)
    define_to_string_tag(vm, etp, "EventTarget");
    let et_ctor = vm.alloc_host_fn("EventTarget", move |vm, _, _| {
        let proto = resolve_new_target_proto(vm, etp, "EventTarget")?;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(et_ctor);
    vm.heap.object_define_data(
        et_ctor,
        vm.key_prototype,
        JsValue::from_object(etp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        etp,
        vm.key_constructor,
        JsValue::from_object(et_ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, "EventTarget", JsValue::from_object(et_ctor));

    install_event_constructors(vm, host, &events);
    install_document_create_event(vm, host, &events);
    install_global_and_window_event_handlers(vm, host, &events);
    events
}

fn install_event_constructors(vm: &mut JsVm, host: &SharedHost, events: &SharedEvents) {
    let ep = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(ep, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(ep).unwrap().is_prototype = true;
    define_to_string_tag(vm, ep, "Event");
    {
        let mut es = events.borrow_mut();
        es.event_proto = Some(ep);
        es.event_protos.insert("Event", ep);
    }

    // Event phase constants on prototype & constructor.
    let define_phase_consts = |vm: &mut JsVm, target: HeapHandle| {
        for (name, val) in [
            ("NONE", 0),
            ("CAPTURING_PHASE", 1),
            ("AT_TARGET", 2),
            ("BUBBLING_PHASE", 3),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                target,
                k,
                JsValue::from_i32(val),
                PropertyAttrs::ENUMERABLE,
                &mut vm.shapes,
            );
        }
    };
    define_phase_consts(vm, ep);

    // Event getters & legacy aliases
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "type", move |vm, t, _| {
        let s = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.event_type.clone()))
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "bubbles", move |_vm, t, _| {
        let b = t
            .as_object()
            .is_some_and(|h| e.borrow().events.get(&h).is_some_and(|ev| ev.bubbles));
        Ok(JsValue::from_bool(b))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "cancelable", move |_vm, t, _| {
        let b = t
            .as_object()
            .is_some_and(|h| e.borrow().events.get(&h).is_some_and(|ev| ev.cancelable));
        Ok(JsValue::from_bool(b))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "composed", move |_vm, t, _| {
        let b = t
            .as_object()
            .is_some_and(|h| e.borrow().events.get(&h).is_some_and(|ev| ev.composed));
        Ok(JsValue::from_bool(b))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "defaultPrevented", move |_vm, t, _| {
        let b = t.as_object().is_some_and(|h| {
            e.borrow()
                .events
                .get(&h)
                .is_some_and(|ev| ev.default_prevented)
        });
        Ok(JsValue::from_bool(b))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "eventPhase", move |_vm, t, _| {
        let p = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.event_phase))
            .unwrap_or(0);
        Ok(JsValue::from_i32(p))
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "target", move |_vm, t, _| {
        let v = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.target))
            .unwrap_or(JsValue::NULL);
        Ok(v)
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "srcElement", move |_vm, t, _| {
        let v = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.target))
            .unwrap_or(JsValue::NULL);
        Ok(v)
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "currentTarget", move |_vm, t, _| {
        let v = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.current_target))
            .unwrap_or(JsValue::NULL);
        Ok(v)
    });
    let e = Rc::clone(events);
    define_host_getter(vm, ep, "timeStamp", move |_vm, t, _| {
        let ts = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.time_stamp))
            .unwrap_or(0.0);
        Ok(JsValue::from_f64(ts))
    });
    let e1 = Rc::clone(events);
    let e2 = Rc::clone(events);
    define_host_accessor(
        vm,
        ep,
        "cancelBubble",
        move |_vm, t, _| {
            let b = t.as_object().is_some_and(|h| {
                e1.borrow()
                    .events
                    .get(&h)
                    .is_some_and(|ev| ev.propagation_stopped)
            });
            Ok(JsValue::from_bool(b))
        },
        move |vm, t, args| {
            if vm.to_boolean(arg(args, 0)) {
                if let Some(h) = t.as_object() {
                    if let Some(ev) = e2.borrow_mut().events.get_mut(&h) {
                        ev.propagation_stopped = true;
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );
    let e1 = Rc::clone(events);
    let e2 = Rc::clone(events);
    define_host_accessor(
        vm,
        ep,
        "returnValue",
        move |_vm, t, _| {
            let b = t.as_object().is_some_and(|h| {
                e1.borrow()
                    .events
                    .get(&h)
                    .is_some_and(|ev| ev.default_prevented)
            });
            Ok(JsValue::from_bool(!b))
        },
        move |vm, t, args| {
            if !vm.to_boolean(arg(args, 0)) {
                if let Some(h) = t.as_object() {
                    if let Some(ev) = e2.borrow_mut().events.get_mut(&h) {
                        if ev.cancelable && !ev.in_passive_listener {
                            ev.default_prevented = true;
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    // Event methods
    let e = Rc::clone(events);
    define_host_method(vm, ep, "stopPropagation", move |_vm, t, _| {
        if let Some(h) = t.as_object() {
            if let Some(ev) = e.borrow_mut().events.get_mut(&h) {
                ev.propagation_stopped = true;
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let e = Rc::clone(events);
    define_host_method(vm, ep, "stopImmediatePropagation", move |_vm, t, _| {
        if let Some(h) = t.as_object() {
            if let Some(ev) = e.borrow_mut().events.get_mut(&h) {
                ev.propagation_stopped = true;
                ev.immediate_stopped = true;
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let e = Rc::clone(events);
    define_host_method(vm, ep, "preventDefault", move |_vm, t, _| {
        if let Some(h) = t.as_object() {
            if let Some(ev) = e.borrow_mut().events.get_mut(&h) {
                if ev.cancelable && !ev.in_passive_listener {
                    ev.default_prevented = true;
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });
    let e = Rc::clone(events);
    let h_cp = Rc::clone(host);
    define_host_method(vm, ep, "composedPath", move |vm, t, _| {
        let arr = vm.alloc_array();
        if let Some(h) = t.as_object() {
            let info = e
                .borrow()
                .events
                .get(&h)
                .filter(|ev| ev.event_phase != 0)
                .map(|ev| {
                    (
                        ev.path_keys.clone(),
                        ev.current_target_key,
                        ev.composed_path.clone(),
                    )
                });
            if let Some((keys, cur_key, fallback_path)) = info {
                if !keys.is_empty() {
                    let cur_node = match cur_key {
                        Some(EventTargetKey::Node(n)) => Some(n),
                        _ => None,
                    };
                    let visible: Vec<EventTargetKey> = {
                        let doc_rc = Rc::clone(&h_cp.borrow().document);
                        let doc = doc_rc.borrow();
                        keys.into_iter()
                            .filter(|k| match k {
                                EventTargetKey::Node(nid) => {
                                    let root = crate::shadow_dom::tree_root(&doc, *nid);
                                    if matches!(
                                        doc.get(root),
                                        Some(NodeData::ShadowRoot { mode, .. }) if mode == "closed"
                                    ) {
                                        cur_node.is_some_and(|cn| {
                                            crate::shadow_dom::is_shadow_including_inclusive_ancestor(
                                                &doc, root, cn,
                                            )
                                        })
                                    } else {
                                        true
                                    }
                                }
                                _ => true,
                            })
                            .collect()
                    };
                    for k in visible {
                        let v = value_for_target_key(vm, &h_cp, k);
                        vm.heap.object_push_element(arr, v);
                    }
                } else {
                    for v in fallback_path {
                        vm.heap.object_push_element(arr, v);
                    }
                }
            }
        }
        Ok(JsValue::from_object(arr))
    });
    let e = Rc::clone(events);
    define_host_method(vm, ep, "initEvent", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("initEvent requires at least 1 argument"));
        }
        let Some(h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let bubbles = vm.to_boolean(arg(args, 1));
        let cancelable = vm.to_boolean(arg(args, 2));
        if let Some(ev) = e.borrow_mut().events.get_mut(&h) {
            if ev.dispatch_flag {
                return Ok(JsValue::UNDEFINED);
            }
            ev.initialized = true;
            ev.propagation_stopped = false;
            ev.immediate_stopped = false;
            ev.default_prevented = false;
            ev.is_trusted = false;
            ev.target = JsValue::NULL;
            ev.event_type = ev_type;
            ev.bubbles = bubbles;
            ev.cancelable = cancelable;
        }
        Ok(JsValue::UNDEFINED)
    });

    // Base `Event` constructor
    let ev_ctor = register_event_subclass(vm, events, "Event", ep, None, |_vm, _obj, _init| Ok(()));
    define_phase_consts(vm, ev_ctor);

    // `CustomEvent`
    let (custom_ctor, custom_proto) = register_event_subclass_with_proto(
        vm,
        events,
        "CustomEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            let detail = read_init_prop(vm, init, "detail", JsValue::NULL)?;
            define_data(vm, obj, "detail", detail);
            Ok(())
        },
    );
    let _ = custom_ctor;
    let e_ce = Rc::clone(events);
    define_host_method(vm, custom_proto, "initCustomEvent", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("initCustomEvent requires at least 1 argument"));
        }
        let Some(h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let bubbles = vm.to_boolean(arg(args, 1));
        let cancelable = vm.to_boolean(arg(args, 2));
        let detail = if args.len() > 3 && !arg(args, 3).is_undefined() {
            arg(args, 3)
        } else {
            JsValue::NULL
        };
        let dispatched = {
            let mut es = e_ce.borrow_mut();
            if let Some(ev) = es.events.get_mut(&h) {
                if ev.dispatch_flag {
                    true
                } else {
                    ev.initialized = true;
                    ev.propagation_stopped = false;
                    ev.immediate_stopped = false;
                    ev.default_prevented = false;
                    ev.is_trusted = false;
                    ev.target = JsValue::NULL;
                    ev.event_type = ev_type;
                    ev.bubbles = bubbles;
                    ev.cancelable = cancelable;
                    false
                }
            } else {
                true
            }
        };
        if !dispatched {
            define_data(vm, h, "detail", detail);
        }
        Ok(JsValue::UNDEFINED)
    });

    // `CloseEvent` (WHATWG WebSocket §9.3.4 — prototype readonly getters)
    let evs_ce_ctor = Rc::clone(events);
    let (_, close_proto) = register_event_subclass_with_proto(
        vm,
        events,
        "CloseEvent",
        ep,
        Some(ev_ctor),
        move |vm, obj, init| {
            let was_clean_v = read_init_prop(vm, init, "wasClean", JsValue::FALSE)?;
            let was_clean = vm.to_boolean(was_clean_v);
            let code_v = read_init_prop(vm, init, "code", JsValue::from_i32(0))?;
            let code_n = vm.to_number(code_v)?;
            let code_u16 = if code_n.is_finite() {
                (code_n.trunc() as i64).rem_euclid(65536) as u16
            } else {
                0
            };
            let reason_v = read_init_prop(vm, init, "reason", JsValue::UNDEFINED)?;
            let reason_str = if reason_v.is_undefined() {
                String::new()
            } else {
                vm.to_js_string(reason_v)?
            };
            evs_ce_ctor
                .borrow_mut()
                .close_events
                .insert(obj, (was_clean, code_u16, reason_str));
            Ok(())
        },
    );
    {
        let evs_wc = Rc::clone(events);
        define_host_getter(vm, close_proto, "wasClean", move |vm, this_val, _| {
            let h = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            let wc = evs_wc
                .borrow()
                .close_events
                .get(&h)
                .map(|&(wc, _, _)| wc)
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            Ok(JsValue::from_bool(wc))
        });
        let evs_cd = Rc::clone(events);
        define_host_getter(vm, close_proto, "code", move |vm, this_val, _| {
            let h = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            let code = evs_cd
                .borrow()
                .close_events
                .get(&h)
                .map(|&(_, code, _)| code)
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            Ok(JsValue::from_i32(i32::from(code)))
        });
        let evs_rs = Rc::clone(events);
        define_host_getter(vm, close_proto, "reason", move |vm, this_val, _| {
            let h = this_val
                .as_object()
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            let reason = evs_rs
                .borrow()
                .close_events
                .get(&h)
                .map(|(_, _, r)| r.clone())
                .ok_or_else(|| vm.type_error("Incompatible CloseEvent receiver"))?;
            Ok(JsValue::from_string(vm.heap.alloc_string(&reason)))
        });
    }

    // `UIEvent`
    let (ui_ctor, ui_proto) = register_event_subclass_with_proto(
        vm,
        events,
        "UIEvent",
        ep,
        Some(ev_ctor),
        init_ui_event_props,
    );
    let e_ui = Rc::clone(events);
    define_host_method(vm, ui_proto, "initUIEvent", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("initUIEvent requires at least 1 argument"));
        }
        let Some(h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let bubbles = vm.to_boolean(arg(args, 1));
        let cancelable = vm.to_boolean(arg(args, 2));
        let view = if args.len() > 3 && !arg(args, 3).is_undefined() {
            arg(args, 3)
        } else {
            JsValue::NULL
        };
        let detail = if args.len() > 4 && !arg(args, 4).is_undefined() {
            JsValue::from_i32(vm.to_number(arg(args, 4))? as i32)
        } else {
            JsValue::from_i32(0)
        };
        let dispatched = init_event_flags(&e_ui, h, ev_type, bubbles, cancelable);
        if !dispatched {
            define_data(vm, h, "view", view);
            define_data(vm, h, "detail", detail);
        }
        Ok(JsValue::UNDEFINED)
    });

    // `FocusEvent` inherits `UIEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "FocusEvent",
        ui_proto,
        Some(ui_ctor),
        |vm, obj, init| {
            init_ui_event_props(vm, obj, init)?;
            let rt = read_init_prop(vm, init, "relatedTarget", JsValue::NULL)?;
            define_data(vm, obj, "relatedTarget", rt);
            Ok(())
        },
    );

    // `MouseEvent` inherits `UIEvent`
    let (mouse_ctor, mouse_proto) = register_event_subclass_with_proto(
        vm,
        events,
        "MouseEvent",
        ui_proto,
        Some(ui_ctor),
        init_mouse_event_props,
    );
    let e_me = Rc::clone(events);
    define_host_method(vm, mouse_proto, "initMouseEvent", move |vm, t, args| {
        if args.is_empty() {
            return Err(vm.type_error("initMouseEvent requires at least 1 argument"));
        }
        let Some(h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let bubbles = vm.to_boolean(arg(args, 1));
        let cancelable = vm.to_boolean(arg(args, 2));
        let dispatched = init_event_flags(&e_me, h, ev_type, bubbles, cancelable);
        if !dispatched {
            let view = if args.len() > 3 && !arg(args, 3).is_undefined() {
                arg(args, 3)
            } else {
                JsValue::NULL
            };
            let detail = JsValue::from_i32(vm.to_number(arg(args, 4)).unwrap_or(0.0) as i32);
            let screen_x = JsValue::from_i32(vm.to_number(arg(args, 5)).unwrap_or(0.0) as i32);
            let screen_y = JsValue::from_i32(vm.to_number(arg(args, 6)).unwrap_or(0.0) as i32);
            let client_x = JsValue::from_i32(vm.to_number(arg(args, 7)).unwrap_or(0.0) as i32);
            let client_y = JsValue::from_i32(vm.to_number(arg(args, 8)).unwrap_or(0.0) as i32);
            let ctrl = JsValue::from_bool(vm.to_boolean(arg(args, 9)));
            let alt = JsValue::from_bool(vm.to_boolean(arg(args, 10)));
            let shift = JsValue::from_bool(vm.to_boolean(arg(args, 11)));
            let meta = JsValue::from_bool(vm.to_boolean(arg(args, 12)));
            let button = JsValue::from_i32(vm.to_number(arg(args, 13)).unwrap_or(0.0) as i32);
            let rt = if args.len() > 14 && !arg(args, 14).is_undefined() {
                arg(args, 14)
            } else {
                JsValue::NULL
            };
            define_data(vm, h, "view", view);
            define_data(vm, h, "detail", detail);
            define_data(vm, h, "screenX", screen_x);
            define_data(vm, h, "screenY", screen_y);
            define_data(vm, h, "clientX", client_x);
            define_data(vm, h, "clientY", client_y);
            define_data(vm, h, "ctrlKey", ctrl);
            define_data(vm, h, "altKey", alt);
            define_data(vm, h, "shiftKey", shift);
            define_data(vm, h, "metaKey", meta);
            define_data(vm, h, "button", button);
            define_data(vm, h, "relatedTarget", rt);
        }
        Ok(JsValue::UNDEFINED)
    });

    // `WheelEvent` inherits `MouseEvent`
    let (wheel_ctor, wheel_proto) = register_event_subclass_with_proto(
        vm,
        events,
        "WheelEvent",
        mouse_proto,
        Some(mouse_ctor),
        |vm, obj, init| {
            init_mouse_event_props(vm, obj, init)?;
            for (prop, def) in [
                ("deltaX", JsValue::from_f64(0.0)),
                ("deltaY", JsValue::from_f64(0.0)),
                ("deltaZ", JsValue::from_f64(0.0)),
                ("deltaMode", JsValue::from_i32(0)),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );
    for target in [wheel_ctor, wheel_proto] {
        for (cname, cval) in [
            ("DOM_DELTA_PIXEL", 0),
            ("DOM_DELTA_LINE", 1),
            ("DOM_DELTA_PAGE", 2),
        ] {
            let k = vm.heap.intern_key(cname);
            vm.heap.object_define_data(
                target,
                k,
                JsValue::from_i32(cval),
                PropertyAttrs::ENUMERABLE,
                &mut vm.shapes,
            );
        }
    }

    // `PointerEvent` inherits `MouseEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "PointerEvent",
        mouse_proto,
        Some(mouse_ctor),
        |vm, obj, init| {
            init_mouse_event_props(vm, obj, init)?;
            let pt_def = JsValue::from_string(vm.heap.alloc_string("mouse"));
            for (prop, def) in [
                ("pointerId", JsValue::from_i32(1)),
                ("pointerType", pt_def),
                ("pressure", JsValue::from_f64(0.0)),
                ("width", JsValue::from_i32(1)),
                ("height", JsValue::from_i32(1)),
                ("isPrimary", JsValue::TRUE),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );

    // `DragEvent` inherits `MouseEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "DragEvent",
        mouse_proto,
        Some(mouse_ctor),
        |vm, obj, init| {
            init_mouse_event_props(vm, obj, init)?;
            let dt = read_init_prop(vm, init, "dataTransfer", JsValue::NULL)?;
            define_data(vm, obj, "dataTransfer", dt);
            Ok(())
        },
    );

    // `KeyboardEvent` inherits `UIEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "KeyboardEvent",
        ui_proto,
        Some(ui_ctor),
        |vm, obj, init| {
            init_ui_event_props(vm, obj, init)?;
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            for (prop, def) in [
                ("key", empty),
                ("code", empty),
                ("location", JsValue::from_i32(0)),
                ("repeat", JsValue::FALSE),
                ("isComposing", JsValue::FALSE),
                ("ctrlKey", JsValue::FALSE),
                ("shiftKey", JsValue::FALSE),
                ("altKey", JsValue::FALSE),
                ("metaKey", JsValue::FALSE),
                ("charCode", JsValue::from_i32(0)),
                ("keyCode", JsValue::from_i32(0)),
                ("which", JsValue::from_i32(0)),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );

    // `CompositionEvent` inherits `UIEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "CompositionEvent",
        ui_proto,
        Some(ui_ctor),
        |vm, obj, init| {
            init_ui_event_props(vm, obj, init)?;
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            let data = read_init_prop(vm, init, "data", empty)?;
            define_data(vm, obj, "data", data);
            Ok(())
        },
    );

    // `InputEvent` inherits `UIEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "InputEvent",
        ui_proto,
        Some(ui_ctor),
        |vm, obj, init| {
            init_ui_event_props(vm, obj, init)?;
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            for (prop, def) in [
                ("data", JsValue::NULL),
                ("inputType", empty),
                ("isComposing", JsValue::FALSE),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );

    // `TextEvent` inherits `UIEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "TextEvent",
        ui_proto,
        Some(ui_ctor),
        |vm, obj, init| {
            init_ui_event_props(vm, obj, init)?;
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            let data = read_init_prop(vm, init, "data", empty)?;
            define_data(vm, obj, "data", data);
            Ok(())
        },
    );

    // Additional HTML / Device Event subclasses required by `Document.createEvent`
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "BeforeUnloadEvent",
        ep,
        Some(ev_ctor),
        |_vm, _obj, _init| Ok(()),
    );
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "HashChangeEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            for prop in ["oldURL", "newURL"] {
                let v = read_init_prop(vm, init, prop, empty)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "StorageEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            for (prop, def) in [
                ("key", JsValue::NULL),
                ("oldValue", JsValue::NULL),
                ("newValue", JsValue::NULL),
                ("url", empty),
                ("storageArea", JsValue::NULL),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "DeviceMotionEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            for (prop, def) in [
                ("acceleration", JsValue::NULL),
                ("accelerationIncludingGravity", JsValue::NULL),
                ("rotationRate", JsValue::NULL),
                ("interval", JsValue::from_f64(0.0)),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "ErrorEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            let msg_v = read_init_prop(vm, init, "message", empty)?;
            let msg = if msg_v.is_string() {
                msg_v
            } else {
                let s = vm.to_js_string(msg_v)?;
                JsValue::from_string(vm.heap.alloc_string(&s))
            };
            let fn_v = read_init_prop(vm, init, "filename", empty)?;
            let filename = if fn_v.is_string() {
                fn_v
            } else {
                let s = vm.to_js_string(fn_v)?;
                JsValue::from_string(vm.heap.alloc_string(&s))
            };
            let lineno_v = read_init_prop(vm, init, "lineno", JsValue::from_i32(0))?;
            let lineno = vm.to_u32(lineno_v).unwrap_or(0);
            let colno_v = read_init_prop(vm, init, "colno", JsValue::from_i32(0))?;
            let colno = vm.to_u32(colno_v).unwrap_or(0);
            let error = read_init_prop(vm, init, "error", JsValue::NULL)?;
            define_data(vm, obj, "message", msg);
            define_data(vm, obj, "filename", filename);
            define_data(vm, obj, "lineno", JsValue::from_f64(f64::from(lineno)));
            define_data(vm, obj, "colno", JsValue::from_f64(f64::from(colno)));
            define_data(vm, obj, "error", error);
            Ok(())
        },
    );
    let _ = register_event_subclass_with_proto(
        vm,
        events,
        "DeviceOrientationEvent",
        ep,
        Some(ev_ctor),
        |vm, obj, init| {
            for (prop, def) in [
                ("alpha", JsValue::NULL),
                ("beta", JsValue::NULL),
                ("gamma", JsValue::NULL),
                ("absolute", JsValue::FALSE),
            ] {
                let v = read_init_prop(vm, init, prop, def)?;
                define_data(vm, obj, prop, v);
            }
            Ok(())
        },
    );
}

fn define_data(vm: &mut JsVm, obj: HeapHandle, prop: &str, val: JsValue) {
    let k = vm.heap.intern_key(prop);
    vm.heap
        .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
}

fn init_event_flags(
    events: &SharedEvents,
    h: HeapHandle,
    ev_type: String,
    bubbles: bool,
    cancelable: bool,
) -> bool {
    let mut es = events.borrow_mut();
    if let Some(ev) = es.events.get_mut(&h) {
        if ev.dispatch_flag {
            return true;
        }
        ev.initialized = true;
        ev.propagation_stopped = false;
        ev.immediate_stopped = false;
        ev.default_prevented = false;
        ev.is_trusted = false;
        ev.target = JsValue::NULL;
        ev.event_type = ev_type;
        ev.bubbles = bubbles;
        ev.cancelable = cancelable;
        false
    } else {
        true
    }
}

fn init_ui_event_props(vm: &mut JsVm, obj: HeapHandle, init: JsValue) -> Result<(), JsException> {
    let view = read_init_prop(vm, init, "view", JsValue::NULL)?;
    if !view.is_null() && !view.is_undefined() && view.as_object().is_none() {
        return Err(vm.type_error("UIEventInit.view must be a Window or null"));
    }
    define_data(vm, obj, "view", view);
    let detail = read_init_prop(vm, init, "detail", JsValue::from_i32(0))?;
    define_data(vm, obj, "detail", detail);
    Ok(())
}

fn init_mouse_event_props(
    vm: &mut JsVm,
    obj: HeapHandle,
    init: JsValue,
) -> Result<(), JsException> {
    init_ui_event_props(vm, obj, init)?;
    for (prop, def) in [
        ("clientX", JsValue::from_i32(0)),
        ("clientY", JsValue::from_i32(0)),
        ("screenX", JsValue::from_i32(0)),
        ("screenY", JsValue::from_i32(0)),
        ("button", JsValue::from_i32(0)),
        ("buttons", JsValue::from_i32(0)),
        ("ctrlKey", JsValue::FALSE),
        ("shiftKey", JsValue::FALSE),
        ("altKey", JsValue::FALSE),
        ("metaKey", JsValue::FALSE),
        ("relatedTarget", JsValue::NULL),
    ] {
        let v = read_init_prop(vm, init, prop, def)?;
        define_data(vm, obj, prop, v);
    }
    Ok(())
}

fn read_init_prop(
    vm: &mut JsVm,
    init: JsValue,
    prop: &str,
    default: JsValue,
) -> Result<JsValue, JsException> {
    if let Some(ih) = init.as_object() {
        let k = vm.heap.intern_key(prop);
        let v = vm.get_property_by_key(ih, k)?;
        if !v.is_undefined() {
            return Ok(v);
        }
    }
    Ok(default)
}

fn register_event_subclass(
    vm: &mut JsVm,
    events: &SharedEvents,
    name: &'static str,
    parent_proto: HeapHandle,
    parent_ctor: Option<HeapHandle>,
    extra_init: impl Fn(&mut JsVm, HeapHandle, JsValue) -> Result<(), JsException> + 'static,
) -> HeapHandle {
    register_event_subclass_with_proto(vm, events, name, parent_proto, parent_ctor, extra_init).0
}

fn register_event_subclass_with_proto(
    vm: &mut JsVm,
    events: &SharedEvents,
    name: &'static str,
    parent_proto: HeapHandle,
    parent_ctor: Option<HeapHandle>,
    extra_init: impl Fn(&mut JsVm, HeapHandle, JsValue) -> Result<(), JsException> + 'static,
) -> (HeapHandle, HeapHandle) {
    let proto = if name == "Event" {
        parent_proto
    } else {
        let p = vm.heap.alloc_empty_object();
        vm.heap
            .object_set_prototype(p, Some(parent_proto), &mut vm.shapes);
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
        define_to_string_tag(vm, p, name);
        events.borrow_mut().event_protos.insert(name, p);
        p
    };
    let evs = Rc::clone(events);
    let ctor = vm.alloc_host_fn(name, move |vm, _, args| {
        let target_proto = resolve_new_target_proto(vm, proto, name)?;
        if args.is_empty() {
            return Err(vm.type_error(format!(
                "Failed to construct '{name}': 1 argument required, but only 0 present."
            )));
        }
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let init = arg(args, 1);
        if !init.is_undefined() && !init.is_null() && init.as_object().is_none() {
            return Err(vm.type_error(format!(
                "Failed to construct '{name}': parameter 2 is not of type 'object'."
            )));
        }
        let b_val = read_init_prop(vm, init, "bubbles", JsValue::FALSE)?;
        let c_val = read_init_prop(vm, init, "cancelable", JsValue::FALSE)?;
        let comp_val = read_init_prop(vm, init, "composed", JsValue::FALSE)?;
        let bubbles = vm.to_boolean(b_val);
        let cancelable = vm.to_boolean(c_val);
        let composed = vm.to_boolean(comp_val);
        let obj = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(target_proto),
        ));
        let ts = evs.borrow().now_ms();
        evs.borrow_mut().events.insert(
            obj,
            EventInternal {
                event_type: ev_type,
                bubbles,
                cancelable,
                composed,
                is_trusted: false,
                initialized: true,
                dispatch_flag: false,
                default_prevented: false,
                propagation_stopped: false,
                immediate_stopped: false,
                in_passive_listener: false,
                event_phase: 0,
                time_stamp: ts,
                target: JsValue::NULL,
                current_target: JsValue::NULL,
                composed_path: Vec::new(),
                path_keys: Vec::new(),
                current_target_key: None,
            },
        );
        define_own_is_trusted(vm, &evs, obj);
        extra_init(vm, obj, init)?;
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(ctor);
    if let Some(pc) = parent_ctor {
        vm.heap.object_set_prototype(ctor, Some(pc), &mut vm.shapes);
    }
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    define_global(vm, name, JsValue::from_object(ctor));
    (ctor, proto)
}

/// Installs `Document.prototype.createEvent(interface)` per WHATWG DOM §4.5.
fn install_document_create_event(vm: &mut JsVm, host: &SharedHost, events: &SharedEvents) {
    let dp = host.borrow().prototypes.document_proto;
    let evs = Rc::clone(events);
    define_host_method(vm, dp, "createEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("Document.createEvent requires 1 argument"));
        }
        let raw = vm.value_to_rust_string(arg(args, 0));
        let canonical = match raw.to_ascii_lowercase().as_str() {
            "event" | "events" | "htmlevents" | "svgevents" => "Event",
            "beforeunloadevent" => "BeforeUnloadEvent",
            "compositionevent" => "CompositionEvent",
            "customevent" => "CustomEvent",
            "devicemotionevent" => "DeviceMotionEvent",
            "deviceorientationevent" => "DeviceOrientationEvent",
            "dragevent" => "DragEvent",
            "focusevent" => "FocusEvent",
            "hashchangeevent" => "HashChangeEvent",
            "keyboardevent" => "KeyboardEvent",
            "messageevent" => "MessageEvent",
            "mouseevent" | "mouseevents" => "MouseEvent",
            "storageevent" => "StorageEvent",
            "textevent" => "TextEvent",
            "uievent" | "uievents" => "UIEvent",
            _ => {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    &format!("The provided event type ('{raw}') is invalid."),
                ));
            }
        };
        let proto = if canonical == "MessageEvent" {
            let k_me = vm.heap.intern_key("MessageEvent");
            let k_proto = vm.key_prototype;
            vm.get_property_by_key(vm.global, k_me)
                .ok()
                .and_then(JsValue::as_object)
                .and_then(|c| vm.get_property_by_key(c, k_proto).ok())
                .and_then(JsValue::as_object)
                .or_else(|| evs.borrow().event_proto)
                .unwrap_or(vm.object_proto)
        } else {
            evs.borrow()
                .event_protos
                .get(canonical)
                .copied()
                .or_else(|| evs.borrow().event_proto)
                .unwrap_or(vm.object_proto)
        };
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let ts = evs.borrow().now_ms();
        evs.borrow_mut().events.insert(
            obj,
            EventInternal {
                event_type: String::new(),
                bubbles: false,
                cancelable: false,
                composed: false,
                is_trusted: false,
                initialized: false,
                dispatch_flag: false,
                default_prevented: false,
                propagation_stopped: false,
                immediate_stopped: false,
                in_passive_listener: false,
                event_phase: 0,
                time_stamp: ts,
                target: JsValue::NULL,
                current_target: JsValue::NULL,
                composed_path: Vec::new(),
                path_keys: Vec::new(),
                current_target_key: None,
            },
        );
        define_own_is_trusted(vm, &evs, obj);
        // Populate default subclass fields
        match canonical {
            "CustomEvent" => define_data(vm, obj, "detail", JsValue::NULL),
            "UIEvent" => {
                define_data(vm, obj, "view", JsValue::NULL);
                define_data(vm, obj, "detail", JsValue::from_i32(0));
            }
            "FocusEvent" => {
                define_data(vm, obj, "view", JsValue::NULL);
                define_data(vm, obj, "detail", JsValue::from_i32(0));
                define_data(vm, obj, "relatedTarget", JsValue::NULL);
            }
            "MouseEvent" | "DragEvent" => {
                let _ = init_mouse_event_props(vm, obj, JsValue::UNDEFINED);
                if canonical == "DragEvent" {
                    define_data(vm, obj, "dataTransfer", JsValue::NULL);
                }
            }
            "KeyboardEvent" => {
                let empty = JsValue::from_string(vm.heap.alloc_string(""));
                define_data(vm, obj, "view", JsValue::NULL);
                define_data(vm, obj, "detail", JsValue::from_i32(0));
                define_data(vm, obj, "key", empty);
                define_data(vm, obj, "code", empty);
                define_data(vm, obj, "location", JsValue::from_i32(0));
                define_data(vm, obj, "repeat", JsValue::FALSE);
                define_data(vm, obj, "isComposing", JsValue::FALSE);
                define_data(vm, obj, "ctrlKey", JsValue::FALSE);
                define_data(vm, obj, "shiftKey", JsValue::FALSE);
                define_data(vm, obj, "altKey", JsValue::FALSE);
                define_data(vm, obj, "metaKey", JsValue::FALSE);
                define_data(vm, obj, "charCode", JsValue::from_i32(0));
                define_data(vm, obj, "keyCode", JsValue::from_i32(0));
                define_data(vm, obj, "which", JsValue::from_i32(0));
            }
            "CompositionEvent" | "TextEvent" => {
                let empty = JsValue::from_string(vm.heap.alloc_string(""));
                define_data(vm, obj, "view", JsValue::NULL);
                define_data(vm, obj, "detail", JsValue::from_i32(0));
                define_data(vm, obj, "data", empty);
            }
            "HashChangeEvent" => {
                let empty = JsValue::from_string(vm.heap.alloc_string(""));
                define_data(vm, obj, "oldURL", empty);
                define_data(vm, obj, "newURL", empty);
            }
            "StorageEvent" => {
                let empty = JsValue::from_string(vm.heap.alloc_string(""));
                define_data(vm, obj, "key", JsValue::NULL);
                define_data(vm, obj, "oldValue", JsValue::NULL);
                define_data(vm, obj, "newValue", JsValue::NULL);
                define_data(vm, obj, "url", empty);
                define_data(vm, obj, "storageArea", JsValue::NULL);
            }
            "MessageEvent" => {
                let empty = JsValue::from_string(vm.heap.alloc_string(""));
                let ports = vm.alloc_array();
                define_data(vm, obj, "data", JsValue::NULL);
                define_data(vm, obj, "origin", empty);
                define_data(vm, obj, "lastEventId", empty);
                define_data(vm, obj, "source", JsValue::NULL);
                define_data(vm, obj, "ports", JsValue::from_object(ports));
            }
            _ => {}
        }
        Ok(JsValue::from_object(obj))
    });
}

/// Standard `GlobalEventHandlers` + `WindowEventHandlers` IDL event handler names (WHATWG HTML §8.1.7.2).
const GLOBAL_EVENT_HANDLERS: &[&str] = &[
    "onabort",
    "onanimationend",
    "onanimationiteration",
    "onanimationstart",
    "onblur",
    "oncancel",
    "oncanplay",
    "onchange",
    "onclick",
    "onclose",
    "oncontextmenu",
    "ondblclick",
    "onerror",
    "onfocus",
    "oninput",
    "oninvalid",
    "onkeydown",
    "onkeypress",
    "onkeyup",
    "onload",
    "onmousedown",
    "onmouseenter",
    "onmouseleave",
    "onmousemove",
    "onmouseout",
    "onmouseover",
    "onmouseup",
    "onreset",
    "onresize",
    "onscroll",
    "onselect",
    "onsubmit",
    "ontransitionend",
    "onwebkitanimationend",
    "onwebkitanimationiteration",
    "onwebkitanimationstart",
    "onwebkittransitionend",
];

const WINDOW_FORWARDED_HANDLERS: &[&str] = &[
    "onblur",
    "onerror",
    "onfocus",
    "onload",
    "onresize",
    "onscroll",
    "onbeforeunload",
    "onhashchange",
    "onmessage",
    "onmessageerror",
    "onoffline",
    "ononline",
    "onpagehide",
    "onpageshow",
    "onpopstate",
    "onrejectionhandled",
    "onstorage",
    "onunhandledrejection",
    "onunload",
];

fn compile_inline_handler(vm: &mut JsVm, code: &str) -> JsValue {
    let wrapped = format!("(function(event){{\n{code}\n}})");
    vm.eval_script(&wrapped).unwrap_or(JsValue::NULL)
}

fn install_event_handler_accessor(
    vm: &mut JsVm,
    target_proto: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    attr_name: &'static str,
    forward_to_window: bool,
) {
    let ev_type = &attr_name[2..];
    let h_g = Rc::clone(host);
    let e_g = Rc::clone(events);
    let h_s = Rc::clone(host);
    let e_s = Rc::clone(events);
    define_host_accessor(
        vm,
        target_proto,
        attr_name,
        move |vm, this_val, _| {
            let key = if forward_to_window {
                EventTargetKey::Window
            } else {
                target_key_for_value(vm, &h_g, this_val)?
            };
            if let Some(&v) = e_g.borrow().event_handlers.get(&(key, ev_type.to_string())) {
                return Ok(v);
            }
            // Compile inline HTML content attribute lazily on first IDL read.
            if let EventTargetKey::Node(nid) = target_key_for_value(vm, &h_g, this_val)? {
                let inline = {
                    let doc_rc = Rc::clone(&h_g.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(e)) => {
                            e.get_attribute(attr_name).map(str::to_string)
                        }
                        _ => None,
                    }
                };
                if let Some(code) = inline {
                    let fn_val = compile_inline_handler(vm, &code);
                    e_g.borrow_mut()
                        .event_handlers
                        .insert((key, ev_type.to_string()), fn_val);
                    return Ok(fn_val);
                }
            }
            Ok(JsValue::NULL)
        },
        move |vm, this_val, args| {
            let key = if forward_to_window {
                EventTargetKey::Window
            } else {
                target_key_for_value(vm, &h_s, this_val)?
            };
            let v = arg(args, 0);
            let stored = match v.as_object() {
                Some(fh) if vm.is_callable_handle(fh) => v,
                _ => JsValue::NULL,
            };
            let _ = &h_s;
            e_s.borrow_mut()
                .event_handlers
                .insert((key, ev_type.to_string()), stored);
            Ok(JsValue::UNDEFINED)
        },
    );
}

fn install_global_and_window_event_handlers(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
) {
    let p = host.borrow().prototypes;
    let global = vm.global;

    // Install GlobalEventHandlers on Window, Document, HTMLElement, SVGElement, MathMLElement.
    for &attr in GLOBAL_EVENT_HANDLERS {
        install_event_handler_accessor(vm, global, host, events, attr, true);
        install_event_handler_accessor(vm, p.document_proto, host, events, attr, false);
        install_event_handler_accessor(vm, p.html_element_proto, host, events, attr, false);
        install_event_handler_accessor(vm, p.svg_element_proto, host, events, attr, false);
        install_event_handler_accessor(vm, p.mathml_element_proto, host, events, attr, false);
    }
    // Additional Window-only handlers on Window.
    for &attr in WINDOW_FORWARDED_HANDLERS {
        if !GLOBAL_EVENT_HANDLERS.contains(&attr) {
            install_event_handler_accessor(vm, global, host, events, attr, true);
        }
    }
    // Window-forwarding handlers on HTMLBodyElement & HTMLFrameSetElement (HTML §8.1.7.2).
    let iface_protos = host.borrow().interface_protos.clone();
    for iface in ["HTMLBodyElement", "HTMLFrameSetElement"] {
        if let Some(&proto) = iface_protos.get(iface) {
            for &attr in WINDOW_FORWARDED_HANDLERS {
                install_event_handler_accessor(vm, proto, host, events, attr, true);
            }
        }
    }
    // Hook `setAttribute("on...", code)` on `<body>`/`<frameset>` so window-forwarded handlers update immediately.
    // Use `Weak` to avoid an `Rc` cycle with `HostState`.
    let e_hook = Rc::downgrade(events);
    host.borrow_mut().body_attr_handler_hook = Some(Rc::new(move |vm, _nid, attr_name, code| {
        if WINDOW_FORWARDED_HANDLERS.contains(&attr_name) {
            if let Some(ev) = e_hook.upgrade() {
                let fn_val = compile_inline_handler(vm, code);
                let ev_type = attr_name[2..].to_string();
                let key = (EventTargetKey::Window, ev_type);
                let needs_tramp = !ev.borrow().idl_handler_trampolines.contains_key(&key);
                ev.borrow_mut().event_handlers.insert(key.clone(), fn_val);
                if needs_tramp {
                    let e_cb = Rc::clone(&ev);
                    let key_cb = key.clone();
                    let tramp = vm.alloc_host_fn(attr_name, move |vm, this_v, cb_args| {
                        let cb = e_cb
                            .borrow()
                            .event_handlers
                            .get(&key_cb)
                            .copied()
                            .unwrap_or(JsValue::NULL);
                        if cb.as_object().is_some_and(|fh| vm.is_callable_handle(fh)) {
                            let ev_arg = arg(cb_args, 0);
                            if let Err(ex) = vm.call_value(cb, this_v, &[ev_arg]) {
                                let hook = e_cb.borrow().on_uncaught_error.clone();
                                if let Some(hook) = hook {
                                    hook(vm, ex);
                                }
                            }
                        }
                        Ok(JsValue::UNDEFINED)
                    });
                    let mut es = ev.borrow_mut();
                    es.idl_handler_trampolines.insert(key.clone(), tramp);
                    es.listeners.entry(key).or_default().push(ListenerEntry {
                        listener: tramp,
                        capture: false,
                        once: false,
                        passive: false,
                    });
                }
            }
        }
    }));
}

/// Propagation entry storing `(current_target_key, retargeted_event_target_key)`.
#[derive(Debug, Clone, Copy)]
struct PathEntry {
    current: EventTargetKey,
    retargeted_target: EventTargetKey,
    retargeted_related_target: Option<EventTargetKey>,
    in_closed_shadow: bool,
}

fn build_propagation_path(
    host: &SharedHost,
    target_key: EventTargetKey,
    composed: bool,
    related_target: Option<NodeId>,
) -> Vec<PathEntry> {
    let EventTargetKey::Node(start_node) = target_key else {
        return vec![PathEntry {
            current: target_key,
            retargeted_target: target_key,
            retargeted_related_target: related_target.map(EventTargetKey::Node),
            in_closed_shadow: false,
        }];
    };

    let hb = host.borrow();
    let doc = hb.document.borrow();
    let doc_root = doc.root();

    let mut path = Vec::new();
    let mut cur = start_node;
    let mut seen_steps = 0usize;

    while seen_steps < 512 {
        seen_steps += 1;
        let cur_root = crate::shadow_dom::tree_root(&doc, cur);
        let in_closed_shadow = matches!(
            doc.get(cur_root),
            Some(NodeData::ShadowRoot { mode, .. }) if mode == "closed"
        );
        let adj_target = crate::shadow_dom::retarget_node(&doc, start_node, cur);
        let adj_rt = related_target.map(|rt| crate::shadow_dom::retarget_node(&doc, rt, cur));

        path.push(PathEntry {
            current: EventTargetKey::Node(cur),
            retargeted_target: EventTargetKey::Node(adj_target),
            retargeted_related_target: adj_rt.map(EventTargetKey::Node),
            in_closed_shadow,
        });

        // Compute next activation/propagation target (WHATWG DOM §2.9):
        // 1. Assigned `<slot>` if `cur` is a slottable child of a shadow host.
        // 2. Shadow host if `cur` is a `ShadowRoot` and `composed == true`.
        // 3. `Window` if `cur == doc_root`.
        // 4. Otherwise `doc.parent(cur)`.
        let next = if let Some(slot_id) =
            crate::shadow_dom::find_slot_for_slottable(&doc, &hb.shadow_dom.borrow(), cur, false)
        {
            Some(slot_id)
        } else {
            match doc.get(cur) {
                Some(NodeData::ShadowRoot {
                    host: shadow_host, ..
                }) => {
                    if !composed {
                        None
                    } else {
                        Some(*shadow_host)
                    }
                }
                _ => {
                    if cur == doc_root {
                        let win_rt = related_target
                            .map(|rt| crate::shadow_dom::retarget_node(&doc, rt, doc_root));
                        let skip_win = related_target
                            .is_some_and(|rt| start_node != rt && Some(adj_target) == win_rt);
                        if !skip_win {
                            path.push(PathEntry {
                                current: EventTargetKey::Window,
                                retargeted_target: EventTargetKey::Node(adj_target),
                                retargeted_related_target: win_rt.map(EventTargetKey::Node),
                                in_closed_shadow: false,
                            });
                        }
                        break;
                    }
                    doc.parent(cur)
                }
            }
        };

        let Some(next_id) = next else {
            break;
        };
        // WHATWG DOM §2.9 step 5.8: stop propagating if retargeted target and
        // retargeted relatedTarget become identical at `next_id`!
        if let Some(rt_id) = related_target {
            if start_node != rt_id {
                let next_t = crate::shadow_dom::retarget_node(&doc, start_node, next_id);
                let next_rt = crate::shadow_dom::retarget_node(&doc, rt_id, next_id);
                if next_t == next_rt {
                    break;
                }
            }
        }
        cur = next_id;
    }
    path
}

fn dispatch_event_internal(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target_key: EventTargetKey,
    ev_h: HeapHandle,
) -> Result<bool, JsException> {
    let (ev_type, bubbles, composed) = {
        let es = events.borrow();
        let Some(ev) = es.events.get(&ev_h) else {
            return Err(vm.type_error("Argument to dispatchEvent is not an Event instance"));
        };
        if !ev.initialized || ev.dispatch_flag {
            return Err(create_dom_exception(
                vm,
                "InvalidStateError",
                "Event is either uninitialized or already being dispatched",
            ));
        }
        (ev.event_type.clone(), ev.bubbles, ev.composed)
    };

    // Inspect optional `relatedTarget` (MouseEvent, FocusEvent, PointerEvent, etc.) for DOM §2.9 retargeting.
    let k_rt = vm.heap.intern_key("relatedTarget");
    let orig_rt_val = vm.get_property_by_key(ev_h, k_rt).ok();
    let related_target_node = orig_rt_val.and_then(|v| host.borrow().node_for_value(vm, v));

    let path = build_propagation_path(host, target_key, composed, related_target_node);
    let path_keys: Vec<EventTargetKey> = path.iter().map(|e| e.current).collect();
    let js_path: Vec<JsValue> = path
        .iter()
        .filter(|e| !e.in_closed_shadow)
        .map(|e| value_for_target_key(vm, host, e.current))
        .collect();
    if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
        ev.dispatch_flag = true;
        ev.composed_path = js_path;
        ev.path_keys = path_keys;
        ev.current_target_key = None;
        ev.propagation_stopped = false;
        ev.immediate_stopped = false;
    }

    // WHATWG DOM §2.9 Pass 1 (Capturing, outermost -> innermost INCLUDING `path[0]`):
    // When `entry.current == entry.retargeted_target` (including shadow hosts), `eventPhase` is
    // `AT_TARGET` (2); otherwise `CAPTURING_PHASE` (1).
    for entry in path.iter().rev() {
        if is_propagation_stopped(events, ev_h) {
            break;
        }
        let phase = if entry.current == entry.retargeted_target {
            2
        } else {
            1
        };
        invoke_listeners_at(vm, host, events, ev_h, &ev_type, *entry, phase, Some(true));
    }

    // WHATWG DOM §2.9 Pass 2 (Bubbling, innermost -> outermost INCLUDING `path[0]`):
    // Runs on every entry where `bubbles == true` OR `entry.current == entry.retargeted_target`
    // (`AT_TARGET`, including retargeted shadow hosts even when `bubbles == false`!).
    for entry in &path {
        if is_propagation_stopped(events, ev_h) {
            break;
        }
        let at_target = entry.current == entry.retargeted_target;
        if !bubbles && !at_target {
            continue;
        }
        let phase = if at_target { 2 } else { 3 };
        invoke_listeners_at(vm, host, events, ev_h, &ev_type, *entry, phase, Some(false));
    }

    // Clean up phase/currentTarget/dispatch_flag and apply WHATWG DOM §2.9 step 14
    // `clearTargets` post-dispatch rule (clears `target`/`relatedTarget` if either ends
    // up in a ShadowRoot, otherwise sets `target` to the final retargeted target even
    // when zero listeners fired).
    let (clear_targets, final_target_key, final_rt_key) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let ft = path.last().map(|e| e.retargeted_target);
        let frt = path.last().and_then(|e| e.retargeted_related_target);
        let in_shadow = |k: EventTargetKey| match k {
            EventTargetKey::Node(n) => matches!(
                doc.get(crate::shadow_dom::tree_root(&doc, n)),
                Some(NodeData::ShadowRoot { .. })
            ),
            _ => false,
        };
        let clear = ft.is_some_and(in_shadow) || frt.is_some_and(in_shadow);
        (clear, ft, frt)
    };
    let final_target_val = if clear_targets {
        JsValue::NULL
    } else {
        final_target_key.map_or(JsValue::NULL, |k| value_for_target_key(vm, host, k))
    };
    if related_target_node.is_some() {
        let final_rt_val = if clear_targets {
            JsValue::NULL
        } else {
            final_rt_key.map_or(JsValue::NULL, |k| value_for_target_key(vm, host, k))
        };
        define_data(vm, ev_h, "relatedTarget", final_rt_val);
    }
    let default_prevented = {
        let mut es = events.borrow_mut();
        if let Some(ev) = es.events.get_mut(&ev_h) {
            ev.dispatch_flag = false;
            ev.event_phase = 0;
            ev.target = final_target_val;
            ev.current_target = JsValue::NULL;
            ev.current_target_key = None;
            ev.in_passive_listener = false;
            ev.composed_path.clear();
            ev.path_keys.clear();
            ev.default_prevented
        } else {
            false
        }
    };
    run_microtask_checkpoint(event_loop, vm);
    Ok(!default_prevented)
}

fn is_propagation_stopped(events: &SharedEvents, ev_h: HeapHandle) -> bool {
    events
        .borrow()
        .events
        .get(&ev_h)
        .is_some_and(|ev| ev.propagation_stopped)
}

#[allow(clippy::too_many_arguments)]
fn invoke_listeners_at(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    ev_h: HeapHandle,
    ev_type: &str,
    entry: PathEntry,
    phase: i32,
    capture_filter: Option<bool>,
) {
    let snapshot: Vec<ListenerEntry> = events
        .borrow()
        .listeners
        .get(&(entry.current, ev_type.to_string()))
        .cloned()
        .unwrap_or_default();
    if snapshot.is_empty() {
        return;
    }

    let cur_val = value_for_target_key(vm, host, entry.current);
    let tgt_val = value_for_target_key(vm, host, entry.retargeted_target);
    let rt_val = entry
        .retargeted_related_target
        .map(|rt| value_for_target_key(vm, host, rt));

    for listener in snapshot {
        if let Some(want_cap) = capture_filter {
            if listener.capture != want_cap {
                continue;
            }
        }
        // Check if listener was removed during an earlier listener or AbortSignal.abort().
        let still_registered = events
            .borrow()
            .listeners
            .get(&(entry.current, ev_type.to_string()))
            .is_some_and(|list| {
                list.iter()
                    .any(|e| e.listener == listener.listener && e.capture == listener.capture)
            });
        if !still_registered {
            continue;
        }
        if events
            .borrow()
            .events
            .get(&ev_h)
            .is_some_and(|ev| ev.immediate_stopped)
        {
            break;
        }
        if listener.once {
            let mut es = events.borrow_mut();
            if let Some(list) = es.listeners.get_mut(&(entry.current, ev_type.to_string())) {
                list.retain(|e| {
                    !(e.listener == listener.listener && e.capture == listener.capture)
                });
            }
        }
        if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
            ev.event_phase = phase;
            ev.current_target = cur_val;
            ev.current_target_key = Some(entry.current);
            ev.target = tgt_val;
            ev.in_passive_listener = listener.passive;
        }
        if let Some(rv) = rt_val {
            define_data(vm, ev_h, "relatedTarget", rv);
        }
        // WHATWG DOM §2.9: invoke callable directly with `this = currentTarget`, or look up
        // `.handleEvent` dynamically on an EventListener object with `this = listener`.
        // Exceptions thrown by listeners MUST NOT abort remaining listeners.
        if vm.is_callable_handle(listener.listener) {
            if let Err(ex) = vm.call_value(
                JsValue::from_object(listener.listener),
                cur_val,
                &[JsValue::from_object(ev_h)],
            ) {
                let hook = events.borrow().on_uncaught_error.clone();
                if let Some(cb) = hook {
                    cb(vm, ex);
                }
            }
        } else {
            let k_he = vm.heap.intern_key("handleEvent");
            if let Ok(he_val) = vm.get_property_by_key(listener.listener, k_he) {
                if let Some(fh) = he_val.as_object().filter(|&h| vm.is_callable_handle(h)) {
                    if let Err(ex) = vm.call_value(
                        JsValue::from_object(fh),
                        JsValue::from_object(listener.listener),
                        &[JsValue::from_object(ev_h)],
                    ) {
                        let hook = events.borrow().on_uncaught_error.clone();
                        if let Some(cb) = hook {
                            cb(vm, ex);
                        }
                    }
                }
            }
        }
    }
}

/// Dispatches a trusted W3C DOM event on `node` (executing capture listeners,
/// target/bubble listeners, JS `el["on" + ev_type]` properties, and inline HTML `on<type>="..."`
/// attributes). Returns `Ok(true)` if default action is NOT prevented, `Ok(false)` if `preventDefault()` ran.
#[allow(clippy::too_many_arguments)]
pub fn fire_dom_node_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    node: NodeId,
    ev_type: &str,
    bubbles: bool,
    cancelable: bool,
    props: &[(&str, JsValue)],
) -> Result<bool, JsException> {
    let proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let target_val = value_for_target_key(vm, host, EventTargetKey::Node(node));
    let ts = events.borrow().now_ms();
    let composed = ev_type != "slotchange";
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles,
            cancelable,
            composed,
            is_trusted: true,
            initialized: true,
            dispatch_flag: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            time_stamp: ts,
            target: target_val,
            current_target: target_val,
            composed_path: Vec::new(),
            path_keys: Vec::new(),
            current_target_key: None,
        },
    );
    define_own_is_trusted(vm, events, ev_h);
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }

    // Invoke inline HTML `on<type>="..."` attributes and JS `node.on<type>` handlers along the
    // target -> ancestor path (respecting `bubbles`, `stopPropagation`, and `return false`).
    let rt_node = props
        .iter()
        .find(|&&(k, _)| k == "relatedTarget")
        .and_then(|&(_, v)| host.borrow().node_for_value(vm, v));
    let path = build_propagation_path(host, EventTargetKey::Node(node), composed, rt_node);
    let attr_name = format!("on{ev_type}");
    let prop_key = vm.heap.intern_key(&attr_name);
    for (idx, entry) in path.iter().enumerate() {
        if idx > 0 && !bubbles {
            break;
        }
        if is_propagation_stopped(events, ev_h) {
            break;
        }
        let cur_val = value_for_target_key(vm, host, entry.current);
        let tgt_val = value_for_target_key(vm, host, entry.retargeted_target);
        if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
            ev.event_phase = if idx == 0 { 2 } else { 3 };
            ev.current_target = cur_val;
            ev.target = tgt_val;
        }
        let mut invoked_idl = false;
        // 1. JS `element.on<type>` property handler (also compiles inline attribute if GlobalEventHandler!)
        if let Some(cur_h) = cur_val.as_object() {
            if let Ok(handler_val) = vm.get_property_by_key(cur_h, prop_key) {
                if let Some(fh) = handler_val
                    .as_object()
                    .filter(|&h| vm.is_callable_handle(h))
                {
                    invoked_idl = true;
                    if let Ok(ret) = vm.call_value(
                        JsValue::from_object(fh),
                        cur_val,
                        &[JsValue::from_object(ev_h)],
                    ) {
                        if ret == JsValue::FALSE && cancelable {
                            if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
                                ev.default_prevented = true;
                            }
                        }
                    }
                }
            }
        }
        // 2. Inline HTML `on<type>="..."` attribute (for events not covered by GlobalEventHandlers)
        if !invoked_idl {
            if let EventTargetKey::Node(nid) = entry.current {
                let inline_code = {
                    let doc_rc = Rc::clone(&host.borrow().document);
                    let doc = doc_rc.borrow();
                    match doc.get(nid) {
                        Some(NodeData::Element(e)) => {
                            e.get_attribute(&attr_name).map(str::to_string)
                        }
                        _ => None,
                    }
                };
                if let Some(code) = inline_code {
                    if !code.trim().is_empty() {
                        let wrapped = format!("(function(event){{\n{code}\n}})");
                        if let Ok(fn_val) = vm.eval_script(&wrapped) {
                            if let Ok(ret) =
                                vm.call_value(fn_val, cur_val, &[JsValue::from_object(ev_h)])
                            {
                                if ret == JsValue::FALSE && cancelable {
                                    if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
                                        ev.default_prevented = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if is_propagation_stopped(events, ev_h) {
        let prevented = events
            .borrow()
            .events
            .get(&ev_h)
            .is_some_and(|ev| ev.default_prevented);
        run_microtask_checkpoint(event_loop, vm);
        return Ok(!prevented);
    }

    dispatch_event_internal(
        vm,
        host,
        events,
        event_loop,
        EventTargetKey::Node(node),
        ev_h,
    )
}

/// Allocates a trusted `Event` instance with extra `props`, invokes `target["on" + ev_type]`
/// if set, and dispatches through `EventTarget` listeners on `target`.
pub fn fire_object_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    ev_type: &str,
    props: &[(&str, JsValue)],
) {
    let ep = events.borrow().event_proto.unwrap_or(vm.object_proto);
    fire_object_event_with_proto(vm, host, events, event_loop, target, ev_type, ep, props);
}

/// Allocates a trusted `Event` inheriting from `proto` with extra `props`, invokes
/// `target["on" + ev_type]` if set, and dispatches through `EventTarget` listeners.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_object_event_with_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    ev_type: &str,
    proto: HeapHandle,
    props: &[(&str, JsValue)],
) {
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ts = events.borrow().now_ms();
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles: false,
            cancelable: false,
            composed: false,
            is_trusted: true,
            initialized: true,
            dispatch_flag: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            time_stamp: ts,
            target: JsValue::from_object(target),
            current_target: JsValue::from_object(target),
            composed_path: vec![JsValue::from_object(target)],
            path_keys: Vec::new(),
            current_target_key: None,
        },
    );
    define_own_is_trusted(vm, events, ev_h);
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }

    let target_key = if target == vm.global {
        EventTargetKey::Window
    } else if let Some(nid) = host.borrow().node_for_handle(vm, target) {
        EventTargetKey::Node(nid)
    } else {
        EventTargetKey::Object(target)
    };

    let has_trampoline = events
        .borrow()
        .idl_handler_trampolines
        .contains_key(&(target_key, ev_type.to_string()));
    if !has_trampoline {
        let handler_key = vm.heap.intern_key(&format!("on{ev_type}"));
        if let Ok(handler_val) = vm.get_property_by_key(target, handler_key) {
            if let Some(h) = handler_val
                .as_object()
                .filter(|&fh| vm.is_callable_handle(fh))
            {
                if let Err(ex) = vm.call_value(
                    JsValue::from_object(h),
                    JsValue::from_object(target),
                    &[JsValue::from_object(ev_h)],
                ) {
                    let hook = events.borrow().on_uncaught_error.clone();
                    if let Some(cb) = hook {
                        cb(vm, ex);
                    }
                }
            }
        }
    }

    let _ = dispatch_event_internal(vm, host, events, event_loop, target_key, ev_h);
}

/// Dispatches a trusted cancelable `ErrorEvent` (`bubbles: false`, `cancelable: true`)
/// on `target` (invoking `target.onerror` and `addEventListener("error")` listeners).
/// Returns `true` if the error was handled (`preventDefault()` called or `onerror` returned `true`
/// when `use_window_onerror_signature` is `true`).
#[allow(clippy::too_many_arguments, dead_code)]
pub(crate) fn fire_error_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    message: &str,
    filename: &str,
    lineno: u32,
    colno: u32,
    error: JsValue,
    use_window_onerror_signature: bool,
) -> (bool, Option<JsException>) {
    let proto = events
        .borrow()
        .event_protos
        .get("ErrorEvent")
        .copied()
        .or_else(|| events.borrow().event_proto)
        .unwrap_or(vm.object_proto);
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ts = events.borrow().now_ms();
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: "error".to_string(),
            bubbles: false,
            cancelable: true,
            composed: false,
            is_trusted: true,
            initialized: true,
            dispatch_flag: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            time_stamp: ts,
            target: JsValue::from_object(target),
            current_target: JsValue::from_object(target),
            composed_path: vec![JsValue::from_object(target)],
            path_keys: Vec::new(),
            current_target_key: None,
        },
    );
    define_own_is_trusted(vm, events, ev_h);
    let msg_v = JsValue::from_string(vm.heap.alloc_string(message));
    let fn_v = JsValue::from_string(vm.heap.alloc_string(filename));
    let line_v = JsValue::from_f64(f64::from(lineno));
    let col_v = JsValue::from_f64(f64::from(colno));
    for &(name, val) in &[
        ("message", msg_v),
        ("filename", fn_v),
        ("lineno", line_v),
        ("colno", col_v),
        ("error", error),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }

    let target_key = if target == vm.global {
        EventTargetKey::Window
    } else if let Some(nid) = host.borrow().node_for_handle(vm, target) {
        EventTargetKey::Node(nid)
    } else {
        EventTargetKey::Object(target)
    };

    let mut handler_exception: Option<JsException> = None;
    let has_trampoline = events
        .borrow()
        .idl_handler_trampolines
        .contains_key(&(target_key, "error".to_string()));
    if !has_trampoline {
        let handler_key = vm.heap.intern_key("onerror");
        if let Ok(handler_val) = vm.get_property_by_key(target, handler_key) {
            if let Some(h) = handler_val
                .as_object()
                .filter(|&fh| vm.is_callable_handle(fh))
            {
                let call_res = if use_window_onerror_signature {
                    vm.call_value(
                        JsValue::from_object(h),
                        JsValue::from_object(target),
                        &[msg_v, fn_v, line_v, col_v, error],
                    )
                } else {
                    vm.call_value(
                        JsValue::from_object(h),
                        JsValue::from_object(target),
                        &[JsValue::from_object(ev_h)],
                    )
                };
                match call_res {
                    Ok(ret) => {
                        if use_window_onerror_signature && ret == JsValue::TRUE {
                            if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
                                ev.default_prevented = true;
                            }
                        }
                    }
                    Err(ex) => {
                        handler_exception = Some(ex);
                    }
                }
            }
        }
    }

    let _ = dispatch_event_internal(vm, host, events, event_loop, target_key, ev_h);
    let prevented = events
        .borrow()
        .events
        .get(&ev_h)
        .is_some_and(|ev| ev.default_prevented);
    (prevented, handler_exception)
}

/// Registers an `EventHandler` IDL attribute (`on<type>`) on `target_proto` following
/// WHATWG HTML §8.1.7.1: any object value is stored verbatim, activating inserts a single
/// trampoline listener in `EventTarget` listener order, deactivating removes it, and
/// dispatch invokes the stored value only if callable.
pub(crate) fn install_object_event_handler_accessor<F>(
    vm: &mut JsVm,
    target_proto: HeapHandle,
    events: &SharedEvents,
    attr_name: &'static str,
    validate_receiver: F,
) where
    F: Fn(&mut JsVm, JsValue) -> Result<HeapHandle, JsException> + 'static,
{
    let ev_type = &attr_name[2..];
    let validate = Rc::new(validate_receiver);
    let v_g = Rc::clone(&validate);
    let e_g = Rc::clone(events);
    let v_s = Rc::clone(&validate);
    let e_s = Rc::clone(events);
    define_host_accessor(
        vm,
        target_proto,
        attr_name,
        move |vm, this_val, _| {
            let h = v_g(vm, this_val)?;
            let key = (EventTargetKey::Object(h), ev_type.to_string());
            Ok(e_g
                .borrow()
                .event_handlers
                .get(&key)
                .copied()
                .unwrap_or(JsValue::NULL))
        },
        move |vm, this_val, args| {
            let h = v_s(vm, this_val)?;
            let v = arg(args, 0);
            let stored = if v.is_object() { v } else { JsValue::NULL };
            let key = (EventTargetKey::Object(h), ev_type.to_string());
            if stored.is_null() {
                let mut es = e_s.borrow_mut();
                es.event_handlers.insert(key.clone(), JsValue::NULL);
                if let Some(tramp) = es.idl_handler_trampolines.remove(&key) {
                    if let Some(list) = es.listeners.get_mut(&key) {
                        list.retain(|entry| entry.listener != tramp);
                    }
                }
            } else {
                let needs_trampoline = !e_s.borrow().idl_handler_trampolines.contains_key(&key);
                e_s.borrow_mut().event_handlers.insert(key.clone(), stored);
                if needs_trampoline {
                    let e_cb = Rc::clone(&e_s);
                    let key_cb = key.clone();
                    let tramp = vm.alloc_host_fn(attr_name, move |vm, this_v, cb_args| {
                        let cb = e_cb
                            .borrow()
                            .event_handlers
                            .get(&key_cb)
                            .copied()
                            .unwrap_or(JsValue::NULL);
                        if cb.as_object().is_some_and(|fh| vm.is_callable_handle(fh)) {
                            let ev_arg = arg(cb_args, 0);
                            if let Err(ex) = vm.call_value(cb, this_v, &[ev_arg]) {
                                let hook = e_cb.borrow().on_uncaught_error.clone();
                                if let Some(hook) = hook {
                                    hook(vm, ex);
                                }
                            }
                        }
                        Ok(JsValue::UNDEFINED)
                    });
                    let mut es = e_s.borrow_mut();
                    es.idl_handler_trampolines.insert(key.clone(), tramp);
                    es.listeners.entry(key).or_default().push(ListenerEntry {
                        listener: tramp,
                        capture: false,
                        once: false,
                        passive: false,
                    });
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );
}

/// Registers an internal listener on an `EventTarget` JS object (e.g. `AbortSignal`).
pub(crate) fn add_internal_object_listener(
    events: &SharedEvents,
    target: HeapHandle,
    ev_type: &str,
    listener: HeapHandle,
    once: bool,
) {
    let key = (EventTargetKey::Object(target), ev_type.to_string());
    events
        .borrow_mut()
        .listeners
        .entry(key)
        .or_default()
        .push(ListenerEntry {
            listener,
            capture: false,
            once,
            passive: false,
        });
}

/// Dispatches a trusted `Event` (or `CloseEvent`) on `target` purely through `EventTarget`'s
/// listener list (respecting WHATWG HTML §8.1.7.1 EventHandler ordering).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_object_event_dispatch_only(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    ev_type: &str,
    proto: HeapHandle,
    props: &[(&str, JsValue)],
    close_fields: Option<(bool, u16, String)>,
) {
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ts = events.borrow().now_ms();
    {
        let mut es = events.borrow_mut();
        es.events.insert(
            ev_h,
            EventInternal {
                event_type: ev_type.to_string(),
                bubbles: false,
                cancelable: false,
                composed: false,
                is_trusted: true,
                initialized: true,
                dispatch_flag: false,
                default_prevented: false,
                propagation_stopped: false,
                immediate_stopped: false,
                in_passive_listener: false,
                event_phase: 2,
                time_stamp: ts,
                target: JsValue::from_object(target),
                current_target: JsValue::from_object(target),
                composed_path: vec![JsValue::from_object(target)],
                path_keys: Vec::new(),
                current_target_key: None,
            },
        );
        if let Some(cf) = close_fields {
            es.close_events.insert(ev_h, cf);
        }
    }
    define_own_is_trusted(vm, events, ev_h);
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    let target_key = if target == vm.global {
        EventTargetKey::Window
    } else if let Some(nid) = host.borrow().node_for_handle(vm, target) {
        EventTargetKey::Node(nid)
    } else {
        EventTargetKey::Object(target)
    };
    let _ = dispatch_event_internal(vm, host, events, event_loop, target_key, ev_h);
}

/// Fires a trusted cancelable event on `target` and returns `true` if a listener called `preventDefault()`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_cancelable_object_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    ev_type: &str,
    proto: HeapHandle,
) -> bool {
    let ev_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ts = events.borrow().now_ms();
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles: true,
            cancelable: true,
            composed: false,
            is_trusted: true,
            initialized: true,
            dispatch_flag: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            time_stamp: ts,
            target: JsValue::from_object(target),
            current_target: JsValue::from_object(target),
            composed_path: vec![JsValue::from_object(target)],
            path_keys: Vec::new(),
            current_target_key: None,
        },
    );
    define_own_is_trusted(vm, events, ev_h);

    let handler_key = vm.heap.intern_key(&format!("on{ev_type}"));
    if let Ok(handler_val) = vm.get_property_by_key(target, handler_key) {
        if let Some(h) = handler_val
            .as_object()
            .filter(|&fh| vm.is_callable_handle(fh))
        {
            let _ = vm.call_value(
                JsValue::from_object(h),
                JsValue::from_object(target),
                &[JsValue::from_object(ev_h)],
            );
        }
    }

    let target_key = if target == vm.global {
        EventTargetKey::Window
    } else if let Some(nid) = host.borrow().node_for_handle(vm, target) {
        EventTargetKey::Node(nid)
    } else {
        EventTargetKey::Object(target)
    };
    let _ = dispatch_event_internal(vm, host, events, event_loop, target_key, ev_h);
    events
        .borrow()
        .events
        .get(&ev_h)
        .is_some_and(|e| e.default_prevented)
}

/// Registers an externally constructed `Event` subclass instance (such as `MessageEvent`)
/// in `EventSystem` so `event.type`, `preventDefault()`, and `dispatchEvent()` work seamlessly.
pub(crate) fn register_constructed_event(
    events: &SharedEvents,
    ev_h: HeapHandle,
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    composed: bool,
) {
    let ts = events.borrow().now_ms();
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type,
            bubbles,
            cancelable,
            composed,
            is_trusted: false,
            initialized: true,
            dispatch_flag: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 0,
            time_stamp: ts,
            target: JsValue::NULL,
            current_target: JsValue::NULL,
            composed_path: Vec::new(),
            path_keys: Vec::new(),
            current_target_key: None,
        },
    );
}
