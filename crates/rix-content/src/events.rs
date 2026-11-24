//! Shadow-DOM-aware WHATWG `EventTarget`, `Event`, `CustomEvent`, `UIEvent`,
//! `MouseEvent`, `PointerEvent`, `KeyboardEvent`, and `InputEvent` dispatch
//! with capture/target/bubble phases, retargeting, `composed`, `composedPath()`,
//! `once`/`passive` options, and `WeakTable` listener pinning.
//!
//! Reference: Spec 06 §3 & Issue #16.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::{NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{wrap_node, SharedHost};
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};

/// Unique target key supporting `window`, DOM `NodeId`s, and standalone `new EventTarget()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventTargetKey {
    Window,
    Node(NodeId),
    Object(HeapHandle),
}

#[derive(Debug, Clone, Copy)]
struct ListenerEntry {
    callback: HeapHandle,
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
    default_prevented: bool,
    propagation_stopped: bool,
    immediate_stopped: bool,
    in_passive_listener: bool,
    event_phase: i32,
    target: JsValue,
    current_target: JsValue,
    composed_path: Vec<JsValue>,
}

/// Shared registry of active event listeners and event internal state.
#[derive(Default)]
pub struct EventSystem {
    listeners: HashMap<(EventTargetKey, String), Vec<ListenerEntry>>,
    events: HashMap<HeapHandle, EventInternal>,
    pub event_proto: Option<HeapHandle>,
}

/// Shared handle to [`EventSystem`].
pub type SharedEvents = Rc<RefCell<EventSystem>>;

impl EventSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Collects GC roots for all registered event listener callbacks.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        if let Some(ep) = self.event_proto {
            out.push(JsValue::from_object(ep));
        }
        for list in self.listeners.values() {
            for entry in list {
                out.push(JsValue::from_object(entry.callback));
            }
        }
    }

    /// Drops event internal state entries for collected JS event objects.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.events.retain(|&h, _| heap.is_alive(h));
        self.listeners.retain(|(k, _), list| {
            let key_alive = match *k {
                EventTargetKey::Object(h) => heap.is_alive(h),
                EventTargetKey::Window | EventTargetKey::Node(_) => true,
            };
            key_alive && !list.is_empty()
        });
    }

    fn has_any_listeners_for_node(&self, nid: NodeId) -> bool {
        self.listeners
            .iter()
            .any(|((k, _), list)| *k == EventTargetKey::Node(nid) && !list.is_empty())
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
        PropertyAttrs::CONFIGURABLE,
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

fn parse_listener_options(
    vm: &mut JsVm,
    opt_val: JsValue,
) -> Result<(bool, bool, bool), JsException> {
    if opt_val.is_undefined() || opt_val.is_null() {
        return Ok((false, false, false));
    }
    if let Some(h) = opt_val.as_object() {
        let k_cap = vm.heap.intern_key("capture");
        let k_once = vm.heap.intern_key("once");
        let k_pas = vm.heap.intern_key("passive");
        let cap = vm
            .get_property_by_key(h, k_cap)
            .is_ok_and(|v| vm.to_boolean(v));
        let once = vm
            .get_property_by_key(h, k_once)
            .is_ok_and(|v| vm.to_boolean(v));
        let pas = vm
            .get_property_by_key(h, k_pas)
            .is_ok_and(|v| vm.to_boolean(v));
        return Ok((cap, once, pas));
    }
    Ok((vm.to_boolean(opt_val), false, false))
}

fn extract_callback_handle(vm: &mut JsVm, val: JsValue) -> Result<Option<HeapHandle>, JsException> {
    let Some(h) = val.as_object() else {
        return Ok(None);
    };
    if vm.is_callable_handle(h) {
        return Ok(Some(h));
    }
    let k_handle = vm.heap.intern_key("handleEvent");
    if let Ok(he) = vm.get_property_by_key(h, k_handle) {
        if let Some(hh) = he.as_object().filter(|&fh| vm.is_callable_handle(fh)) {
            return Ok(Some(hh));
        }
    }
    Ok(None)
}

/// Installs `EventTarget.prototype` methods and all DOM Event constructors on `vm`.
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
        let Some(cb) = extract_callback_handle(vm, arg(args, 1))? else {
            return Ok(JsValue::UNDEFINED);
        };
        let (capture, once, passive) = parse_listener_options(vm, arg(args, 2))?;
        {
            let mut es = evs.borrow_mut();
            let list = es.listeners.entry((key, ev_type)).or_default();
            if !list
                .iter()
                .any(|e| e.callback == cb && e.capture == capture)
            {
                list.push(ListenerEntry {
                    callback: cb,
                    capture,
                    once,
                    passive,
                });
            }
        }
        if let EventTargetKey::Node(nid) = key {
            h.borrow_mut().wrappers.set_pinned(&nid, true);
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
        let Some(cb) = extract_callback_handle(vm, arg(args, 1))? else {
            return Ok(JsValue::UNDEFINED);
        };
        let (capture, _, _) = parse_listener_options(vm, arg(args, 2))?;
        let still_pinned = {
            let mut es = evs.borrow_mut();
            if let Some(list) = es.listeners.get_mut(&(key, ev_type)) {
                list.retain(|e| !(e.callback == cb && e.capture == capture));
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

    // Replace EventTarget constructor so `new EventTarget()` creates an EventTarget instance.
    let et_ctor = vm.alloc_host_fn("EventTarget", move |vm, _, _| {
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(etp)));
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
    define_global(vm, "EventTarget", JsValue::from_object(et_ctor));

    install_event_constructors(vm, &events);
    events
}

fn install_event_constructors(vm: &mut JsVm, events: &SharedEvents) {
    let ep = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(ep, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(ep).unwrap().is_prototype = true;
    events.borrow_mut().event_proto = Some(ep);

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

    // Event getters
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
    define_host_getter(vm, ep, "currentTarget", move |_vm, t, _| {
        let v = t
            .as_object()
            .and_then(|h| e.borrow().events.get(&h).map(|ev| ev.current_target))
            .unwrap_or(JsValue::NULL);
        Ok(v)
    });

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
    define_host_method(vm, ep, "composedPath", move |vm, t, _| {
        let arr = vm.alloc_array();
        if let Some(h) = t.as_object() {
            let path = e
                .borrow()
                .events
                .get(&h)
                .map(|ev| ev.composed_path.clone())
                .unwrap_or_default();
            for v in path {
                vm.heap.object_push_element(arr, v);
            }
        }
        Ok(JsValue::from_object(arr))
    });

    // Base `Event` constructor
    let ev_ctor = register_event_subclass(vm, events, "Event", ep, |_vm, _obj, _init| Ok(()));
    define_phase_consts(vm, ev_ctor);

    // `CustomEvent`
    let _ = register_event_subclass(vm, events, "CustomEvent", ep, |vm, obj, init| {
        let detail = read_init_prop(vm, init, "detail", JsValue::NULL)?;
        let k = vm.heap.intern_key("detail");
        vm.heap
            .object_define_data(obj, k, detail, PropertyAttrs::DEFAULT, &mut vm.shapes);
        Ok(())
    });

    // `UIEvent`
    let ui_proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(ui_proto, Some(ep), &mut vm.shapes);
    vm.heap.get_object_mut(ui_proto).unwrap().is_prototype = true;
    let _ = register_event_subclass(vm, events, "UIEvent", ep, |vm, obj, init| {
        let detail = read_init_prop(vm, init, "detail", JsValue::from_i32(0))?;
        let k = vm.heap.intern_key("detail");
        vm.heap
            .object_define_data(obj, k, detail, PropertyAttrs::DEFAULT, &mut vm.shapes);
        Ok(())
    });

    // `MouseEvent`
    let mouse_ctor = register_event_subclass(vm, events, "MouseEvent", ep, |vm, obj, init| {
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
        ] {
            let v = read_init_prop(vm, init, prop, def)?;
            let k = vm.heap.intern_key(prop);
            vm.heap
                .object_define_data(obj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(())
    });
    let mouse_proto = vm
        .get_property_by_key(mouse_ctor, vm.key_prototype)
        .ok()
        .and_then(JsValue::as_object)
        .unwrap_or(ep);

    // `PointerEvent` inherits from `MouseEvent.prototype`
    let _ = register_event_subclass(vm, events, "PointerEvent", mouse_proto, |vm, obj, init| {
        let pt_def = JsValue::from_string(vm.heap.alloc_string("mouse"));
        for (prop, def) in [
            ("clientX", JsValue::from_i32(0)),
            ("clientY", JsValue::from_i32(0)),
            ("button", JsValue::from_i32(0)),
            ("buttons", JsValue::from_i32(0)),
            ("pointerId", JsValue::from_i32(1)),
            ("pointerType", pt_def),
            ("pressure", JsValue::from_f64(0.0)),
            ("width", JsValue::from_i32(1)),
            ("height", JsValue::from_i32(1)),
            ("isPrimary", JsValue::TRUE),
        ] {
            let v = read_init_prop(vm, init, prop, def)?;
            let k = vm.heap.intern_key(prop);
            vm.heap
                .object_define_data(obj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(())
    });

    // `KeyboardEvent`
    let _ = register_event_subclass(vm, events, "KeyboardEvent", ep, |vm, obj, init| {
        let empty = JsValue::from_string(vm.heap.alloc_string(""));
        for (prop, def) in [
            ("key", empty),
            ("code", empty),
            ("repeat", JsValue::FALSE),
            ("ctrlKey", JsValue::FALSE),
            ("shiftKey", JsValue::FALSE),
            ("altKey", JsValue::FALSE),
            ("metaKey", JsValue::FALSE),
        ] {
            let v = read_init_prop(vm, init, prop, def)?;
            let k = vm.heap.intern_key(prop);
            vm.heap
                .object_define_data(obj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(())
    });

    // `InputEvent`
    let _ = register_event_subclass(vm, events, "InputEvent", ep, |vm, obj, init| {
        let empty = JsValue::from_string(vm.heap.alloc_string(""));
        for (prop, def) in [
            ("data", JsValue::NULL),
            ("inputType", empty),
            ("isComposing", JsValue::FALSE),
        ] {
            let v = read_init_prop(vm, init, prop, def)?;
            let k = vm.heap.intern_key(prop);
            vm.heap
                .object_define_data(obj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(())
    });
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
    name: &str,
    parent_proto: HeapHandle,
    extra_init: impl Fn(&mut JsVm, HeapHandle, JsValue) -> Result<(), JsException> + 'static,
) -> HeapHandle {
    let proto = if name == "Event" {
        parent_proto
    } else {
        let p = vm.heap.alloc_empty_object();
        vm.heap
            .object_set_prototype(p, Some(parent_proto), &mut vm.shapes);
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
        p
    };
    let evs = Rc::clone(events);
    let ctor = vm.alloc_host_fn(name, move |vm, _, args| {
        let ev_type = vm.value_to_rust_string(arg(args, 0));
        let init = arg(args, 1);
        let b_val = read_init_prop(vm, init, "bubbles", JsValue::FALSE)?;
        let c_val = read_init_prop(vm, init, "cancelable", JsValue::FALSE)?;
        let comp_val = read_init_prop(vm, init, "composed", JsValue::FALSE)?;
        let bubbles = vm.to_boolean(b_val);
        let cancelable = vm.to_boolean(c_val);
        let composed = vm.to_boolean(comp_val);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        evs.borrow_mut().events.insert(
            obj,
            EventInternal {
                event_type: ev_type,
                bubbles,
                cancelable,
                composed,
                default_prevented: false,
                propagation_stopped: false,
                immediate_stopped: false,
                in_passive_listener: false,
                event_phase: 0,
                target: JsValue::NULL,
                current_target: JsValue::NULL,
                composed_path: Vec::new(),
            },
        );
        extra_init(vm, obj, init)?;
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(ctor);
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
    ctor
}

/// Propagation entry storing `(current_target_key, retargeted_event_target_key)`.
#[derive(Debug, Clone, Copy)]
struct PathEntry {
    current: EventTargetKey,
    retargeted_target: EventTargetKey,
    in_closed_shadow: bool,
}

fn build_propagation_path(
    host: &SharedHost,
    target_key: EventTargetKey,
    composed: bool,
) -> Vec<PathEntry> {
    let EventTargetKey::Node(start_node) = target_key else {
        return vec![PathEntry {
            current: target_key,
            retargeted_target: target_key,
            in_closed_shadow: false,
        }];
    };

    let doc_rc = Rc::clone(&host.borrow().document);
    let doc = doc_rc.borrow();
    let doc_root = doc.root();

    let mut path = Vec::new();
    let mut cur = Some(start_node);
    let mut current_target_for_tree = EventTargetKey::Node(start_node);
    let mut in_closed_shadow = false;

    while let Some(nid) = cur {
        path.push(PathEntry {
            current: EventTargetKey::Node(nid),
            retargeted_target: current_target_for_tree,
            in_closed_shadow,
        });

        match doc.get(nid) {
            Some(NodeData::ShadowRoot {
                host: shadow_host,
                mode,
                ..
            }) => {
                if !composed {
                    break;
                }
                if mode == "closed" {
                    in_closed_shadow = true;
                }
                // Crossing out of a ShadowRoot retargets `event.target` to the shadow host!
                current_target_for_tree = EventTargetKey::Node(*shadow_host);
                cur = Some(*shadow_host);
            }
            _ => {
                if nid == doc_root {
                    path.push(PathEntry {
                        current: EventTargetKey::Window,
                        retargeted_target: current_target_for_tree,
                        in_closed_shadow: false,
                    });
                    break;
                }
                cur = doc.parent(nid);
            }
        }
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
        (ev.event_type.clone(), ev.bubbles, ev.composed)
    };

    let path = build_propagation_path(host, target_key, composed);
    let js_path: Vec<JsValue> = path
        .iter()
        .filter(|e| !e.in_closed_shadow)
        .map(|e| value_for_target_key(vm, host, e.current))
        .collect();
    if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
        ev.composed_path = js_path;
        ev.propagation_stopped = false;
        ev.immediate_stopped = false;
    }

    // Phase 1: Capturing (outermost ancestor -> target's immediate parent).
    if path.len() > 1 {
        for entry in path[1..].iter().rev() {
            if is_propagation_stopped(events, ev_h) {
                break;
            }
            invoke_listeners_at(vm, host, events, ev_h, &ev_type, *entry, 1, Some(true))?;
        }
    }

    // Phase 2: At Target (runs all listeners on target in registration order).
    if !is_propagation_stopped(events, ev_h) {
        if let Some(&target_entry) = path.first() {
            invoke_listeners_at(vm, host, events, ev_h, &ev_type, target_entry, 2, None)?;
        }
    }

    // Phase 3: Bubbling (target's immediate parent -> outermost ancestor) when `bubbles == true`.
    if bubbles && path.len() > 1 {
        for entry in &path[1..] {
            if is_propagation_stopped(events, ev_h) {
                break;
            }
            invoke_listeners_at(vm, host, events, ev_h, &ev_type, *entry, 3, Some(false))?;
        }
    }

    // Clean up phase/currentTarget after dispatch & run strict WHATWG microtask checkpoint.
    let default_prevented = {
        let mut es = events.borrow_mut();
        if let Some(ev) = es.events.get_mut(&ev_h) {
            ev.event_phase = 0;
            ev.current_target = JsValue::NULL;
            ev.in_passive_listener = false;
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
) -> Result<(), JsException> {
    let snapshot: Vec<ListenerEntry> = events
        .borrow()
        .listeners
        .get(&(entry.current, ev_type.to_string()))
        .cloned()
        .unwrap_or_default();
    if snapshot.is_empty() {
        return Ok(());
    }

    let cur_val = value_for_target_key(vm, host, entry.current);
    let tgt_val = value_for_target_key(vm, host, entry.retargeted_target);

    for listener in snapshot {
        if let Some(want_cap) = capture_filter {
            if listener.capture != want_cap {
                continue;
            }
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
                    !(e.callback == listener.callback && e.capture == listener.capture)
                });
            }
        }
        if let Some(ev) = events.borrow_mut().events.get_mut(&ev_h) {
            ev.event_phase = phase;
            ev.current_target = cur_val;
            ev.target = tgt_val;
            ev.in_passive_listener = listener.passive;
        }
        let _ = vm.call_value(
            JsValue::from_object(listener.callback),
            cur_val,
            &[JsValue::from_object(ev_h)],
        )?;
    }
    Ok(())
}

/// Dispatches a full W3C bubbling/cancelable DOM event on `node` (executing capture listeners,
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
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles,
            cancelable,
            composed: true,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            target: target_val,
            current_target: target_val,
            composed_path: Vec::new(),
        },
    );
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }

    // Invoke inline HTML `on<type>="..."` attributes and JS `node.on<type>` handlers along the
    // target -> ancestor path (respecting `bubbles`, `stopPropagation`, and `return false`).
    let path = build_propagation_path(host, EventTargetKey::Node(node), true);
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
        // 1. JS `element.on<type>` property handler
        if let Some(cur_h) = cur_val.as_object() {
            if let Ok(handler_val) = vm.get_property_by_key(cur_h, prop_key) {
                if let Some(fh) = handler_val
                    .as_object()
                    .filter(|&h| vm.is_callable_handle(h))
                {
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
        // 2. Inline HTML `on<type>="..."` attribute
        if let EventTargetKey::Node(nid) = entry.current {
            let inline_code = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let doc = doc_rc.borrow();
                match doc.get(nid) {
                    Some(NodeData::Element(e)) => e.get_attribute(&attr_name).map(str::to_string),
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

/// Allocates a real `Event` instance with extra `props`, invokes `target["on" + ev_type]`
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

/// Allocates an `Event` inheriting from `proto` with extra `props`, invokes
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
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles: false,
            cancelable: false,
            composed: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            target: JsValue::from_object(target),
            current_target: JsValue::from_object(target),
            composed_path: vec![JsValue::from_object(target)],
        },
    );
    for &(name, val) in props {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(ev_h, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }

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
}

/// Fires a cancelable event on `target` and returns `true` if a listener called `preventDefault()`.
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
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type: ev_type.to_string(),
            bubbles: true,
            cancelable: true,
            composed: false,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 2,
            target: JsValue::from_object(target),
            current_target: JsValue::from_object(target),
            composed_path: vec![JsValue::from_object(target)],
        },
    );

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
    events.borrow_mut().events.insert(
        ev_h,
        EventInternal {
            event_type,
            bubbles,
            cancelable,
            composed,
            default_prevented: false,
            propagation_stopped: false,
            immediate_stopped: false,
            in_passive_listener: false,
            event_phase: 0,
            target: JsValue::NULL,
            current_target: JsValue::NULL,
            composed_path: Vec::new(),
        },
    );
}
