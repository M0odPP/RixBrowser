//! Web Platform Observers:
//! - `MutationObserver` (microtask-batched `MutationRecord` delivery)
//! - `ResizeObserver` (`ResizeObserverEntry` from live dirty-checked layout)
//! - `IntersectionObserver` (`IntersectionObserverEntry` with exact rect & ratio intersection)
//!
//! Reference: Spec 06 §3 & Issue #16.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_core::Rect;
use rix_dom::NodeId;
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{wrap_node, DomMutation, SharedHost};
use crate::event_loop::SharedEventLoop;

#[derive(Debug, Clone)]
struct MutationWatch {
    target: NodeId,
    child_list: bool,
    attributes: bool,
    character_data: bool,
    subtree: bool,
    attribute_old_value: bool,
    character_data_old_value: bool,
    attribute_filter: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct PendingMutationRecord {
    kind: &'static str,
    target: NodeId,
    added: Vec<NodeId>,
    removed: Vec<NodeId>,
    previous_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    attribute_name: Option<String>,
    attribute_namespace: Option<String>,
    old_value: Option<String>,
}

struct MutationObserverEntry {
    observer_obj: HeapHandle,
    callback: HeapHandle,
    watches: Vec<MutationWatch>,
    pending: Vec<PendingMutationRecord>,
}

struct ResizeObserverEntry {
    observer_obj: HeapHandle,
    callback: HeapHandle,
    targets: Vec<NodeId>,
    last_sizes: HashMap<NodeId, (f32, f32)>,
}

struct IntersectionObserverEntry {
    observer_obj: HeapHandle,
    callback: HeapHandle,
    root: Option<NodeId>,
    thresholds: Vec<f64>,
    targets: Vec<NodeId>,
    last_state: HashMap<NodeId, (bool, i32)>,
    pending_records: Vec<IntersectionSnapshot>,
}

#[derive(Debug, Clone, Copy)]
struct IntersectionSnapshot {
    target: NodeId,
    bounding_rect: Rect,
    root_bounds: Rect,
    intersection_rect: Rect,
    intersection_ratio: f64,
    is_intersecting: bool,
    time_ms: f64,
}

/// Shared registry for `MutationObserver`, `ResizeObserver`, and `IntersectionObserver`.
#[derive(Default)]
pub struct ObserverSystem {
    mutation_observers: HashMap<HeapHandle, MutationObserverEntry>,
    resize_observers: HashMap<HeapHandle, ResizeObserverEntry>,
    intersection_observers: HashMap<HeapHandle, IntersectionObserverEntry>,
}

/// Shared handle to [`ObserverSystem`].
pub type SharedObservers = Rc<RefCell<ObserverSystem>>;

impl ObserverSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Collects GC roots for active observer instances and callbacks.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for mo in self.mutation_observers.values() {
            if !mo.watches.is_empty() || !mo.pending.is_empty() {
                out.push(JsValue::from_object(mo.observer_obj));
                out.push(JsValue::from_object(mo.callback));
            }
        }
        for ro in self.resize_observers.values() {
            if !ro.targets.is_empty() {
                out.push(JsValue::from_object(ro.observer_obj));
                out.push(JsValue::from_object(ro.callback));
            }
        }
        for io in self.intersection_observers.values() {
            if !io.targets.is_empty() {
                out.push(JsValue::from_object(io.observer_obj));
                out.push(JsValue::from_object(io.callback));
            }
        }
    }

    fn record_mutation(&mut self, host: &SharedHost, mutation: &DomMutation) {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();

        for mo in self.mutation_observers.values_mut() {
            if mo.watches.is_empty() {
                continue;
            }
            match mutation {
                DomMutation::ChildList {
                    target,
                    added,
                    removed,
                    previous_sibling,
                    next_sibling,
                } => {
                    let matched = mo.watches.iter().any(|w| {
                        w.child_list
                            && (w.target == *target
                                || (w.subtree && doc.ancestors(*target).any(|a| a == w.target)))
                    });
                    if matched {
                        mo.pending.push(PendingMutationRecord {
                            kind: "childList",
                            target: *target,
                            added: added.clone(),
                            removed: removed.clone(),
                            previous_sibling: *previous_sibling,
                            next_sibling: *next_sibling,
                            attribute_name: None,
                            attribute_namespace: None,
                            old_value: None,
                        });
                    }
                }
                DomMutation::Attributes {
                    target,
                    attribute_name,
                    attribute_namespace,
                    old_value,
                } => {
                    let matched = mo.watches.iter().find(|w| {
                        w.attributes
                            && (w.target == *target
                                || (w.subtree && doc.ancestors(*target).any(|a| a == w.target)))
                            && w.attribute_filter.as_ref().is_none_or(|f| {
                                attribute_namespace.is_none()
                                    && f.iter().any(|name| name == attribute_name)
                            })
                    });
                    if let Some(w) = matched {
                        mo.pending.push(PendingMutationRecord {
                            kind: "attributes",
                            target: *target,
                            added: Vec::new(),
                            removed: Vec::new(),
                            previous_sibling: None,
                            next_sibling: None,
                            attribute_name: Some(attribute_name.clone()),
                            attribute_namespace: attribute_namespace.clone(),
                            old_value: if w.attribute_old_value {
                                old_value.clone()
                            } else {
                                None
                            },
                        });
                    }
                }
                DomMutation::CharacterData { target, old_value } => {
                    let matched = mo.watches.iter().find(|w| {
                        w.character_data
                            && (w.target == *target
                                || (w.subtree && doc.ancestors(*target).any(|a| a == w.target)))
                    });
                    if let Some(w) = matched {
                        mo.pending.push(PendingMutationRecord {
                            kind: "characterData",
                            target: *target,
                            added: Vec::new(),
                            removed: Vec::new(),
                            previous_sibling: None,
                            next_sibling: None,
                            attribute_name: None,
                            attribute_namespace: None,
                            old_value: if w.character_data_old_value {
                                Some(old_value.clone())
                            } else {
                                None
                            },
                        });
                    }
                }
            }
        }
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

/// Installs `MutationObserver`, `ResizeObserver`, and `IntersectionObserver` and wires
/// `MutationObserver` into `HostState::mutation_sinks` + `EventLoopState` microtask checkpoints.
pub fn install_observers(
    vm: &mut JsVm,
    host: &SharedHost,
    event_loop: &SharedEventLoop,
) -> SharedObservers {
    let observers = Rc::new(RefCell::new(ObserverSystem::new()));

    // Hook DOM mutations -> MutationObserver pending queues.
    let obs_sink = Rc::clone(&observers);
    let host_sink = Rc::clone(host);
    host.borrow_mut()
        .mutation_sinks
        .push(Rc::new(RefCell::new(Box::new(
            move |mutation: &DomMutation| {
                obs_sink.borrow_mut().record_mutation(&host_sink, mutation);
            },
        ))));

    // Hook WHATWG microtask checkpoint -> automatic MutationObserver delivery.
    let obs_cp = Rc::clone(&observers);
    let host_cp = Rc::clone(host);
    event_loop
        .borrow_mut()
        .add_checkpoint_hook(Rc::new(RefCell::new(Box::new(move |vm: &mut JsVm| {
            deliver_mutation_observers(vm, &host_cp, &obs_cp);
        }))));

    install_mutation_observer(vm, host, &observers);
    install_resize_observer(vm, host, &observers);
    install_intersection_observer(vm, host, &observers);

    observers
}

/// Delivers all pending `MutationRecord` batches to active `MutationObserver` callbacks.
pub fn deliver_mutation_observers(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let deliveries: Vec<(HeapHandle, HeapHandle, Vec<PendingMutationRecord>)> = {
        let mut os = observers.borrow_mut();
        let mut out = Vec::new();
        for mo in os.mutation_observers.values_mut() {
            if !mo.pending.is_empty() {
                out.push((
                    mo.observer_obj,
                    mo.callback,
                    std::mem::take(&mut mo.pending),
                ));
            }
        }
        out
    };
    for (obs_obj, cb, records) in deliveries {
        let arr = build_mutation_record_array(vm, host, records);
        let _ = vm.call_value(
            JsValue::from_object(cb),
            JsValue::from_object(obs_obj),
            &[JsValue::from_object(arr), JsValue::from_object(obs_obj)],
        );
    }
}

/// Evaluates `ResizeObserver` and `IntersectionObserver` targets against current layout
/// geometry and delivers any size-change or threshold-crossing entries.
pub fn deliver_layout_observers(
    vm: &mut JsVm,
    host: &SharedHost,
    observers: &SharedObservers,
    timestamp_ms: f64,
) {
    // 1. ResizeObserver delivery
    type ResizeBatch = (HeapHandle, HeapHandle, Vec<(NodeId, Rect, Rect)>);
    let resize_batches: Vec<ResizeBatch> = {
        let mut os = observers.borrow_mut();
        let mut out = Vec::new();
        for ro in os.resize_observers.values_mut() {
            if ro.targets.is_empty() {
                continue;
            }
            let mut changed = Vec::new();
            for &nid in &ro.targets {
                let (border, content) = host
                    .borrow_mut()
                    .bounding_rects_for_node(nid)
                    .unwrap_or((Rect::ZERO, Rect::ZERO));
                let cur_size = (content.size.width, content.size.height);
                let prev = ro.last_sizes.get(&nid).copied();
                if prev != Some(cur_size) {
                    ro.last_sizes.insert(nid, cur_size);
                    changed.push((nid, border, content));
                }
            }
            if !changed.is_empty() {
                out.push((ro.observer_obj, ro.callback, changed));
            }
        }
        out
    };

    for (obs_obj, cb, entries) in resize_batches {
        let arr = vm.alloc_array();
        for (nid, border, content) in entries {
            let entry = build_resize_entry(vm, host, nid, border, content);
            vm.heap
                .object_push_element(arr, JsValue::from_object(entry));
        }
        let _ = vm.call_value(
            JsValue::from_object(cb),
            JsValue::from_object(obs_obj),
            &[JsValue::from_object(arr), JsValue::from_object(obs_obj)],
        );
    }

    // 2. IntersectionObserver delivery
    let viewport = host.borrow().viewport;
    let vp_rect = Rect::new(0.0, 0.0, viewport.width, viewport.height);
    let inter_batches: Vec<(HeapHandle, HeapHandle, Vec<IntersectionSnapshot>)> = {
        let mut os = observers.borrow_mut();
        let mut out = Vec::new();
        for io in os.intersection_observers.values_mut() {
            if io.targets.is_empty() {
                continue;
            }
            let root_bounds = match io.root {
                Some(r_id) => host
                    .borrow_mut()
                    .bounding_rects_for_node(r_id)
                    .map_or(vp_rect, |(b, _)| b),
                None => vp_rect,
            };
            let mut entries = std::mem::take(&mut io.pending_records);
            for &nid in &io.targets {
                let bounding_rect = host
                    .borrow_mut()
                    .bounding_rects_for_node(nid)
                    .map_or(Rect::ZERO, |(b, _)| b);
                let (intersection_rect, is_intersecting) =
                    intersect_rects(bounding_rect, root_bounds);
                let target_area =
                    f64::from(bounding_rect.size.width) * f64::from(bounding_rect.size.height);
                let inter_area = f64::from(intersection_rect.size.width)
                    * f64::from(intersection_rect.size.height);
                let ratio = if !is_intersecting {
                    0.0
                } else if target_area <= 0.0 {
                    1.0
                } else {
                    (inter_area / target_area).clamp(0.0, 1.0)
                };
                let bucket = threshold_bucket(&io.thresholds, ratio, is_intersecting);
                let prev = io.last_state.get(&nid).copied();
                if prev != Some((is_intersecting, bucket)) {
                    io.last_state.insert(nid, (is_intersecting, bucket));
                    entries.push(IntersectionSnapshot {
                        target: nid,
                        bounding_rect,
                        root_bounds,
                        intersection_rect,
                        intersection_ratio: ratio,
                        is_intersecting,
                        time_ms: timestamp_ms,
                    });
                }
            }
            if !entries.is_empty() {
                out.push((io.observer_obj, io.callback, entries));
            }
        }
        out
    };

    for (obs_obj, cb, entries) in inter_batches {
        let arr = vm.alloc_array();
        for snap in entries {
            let entry = build_intersection_entry(vm, host, snap);
            vm.heap
                .object_push_element(arr, JsValue::from_object(entry));
        }
        let _ = vm.call_value(
            JsValue::from_object(cb),
            JsValue::from_object(obs_obj),
            &[JsValue::from_object(arr), JsValue::from_object(obs_obj)],
        );
    }
}

fn intersect_rects(a: Rect, b: Rect) -> (Rect, bool) {
    let x1 = a.origin.x.max(b.origin.x);
    let y1 = a.origin.y.max(b.origin.y);
    let x2 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y2 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if x2 >= x1 && y2 >= y1 && (x2 > x1 || y2 > y1) {
        (
            Rect::new(x1, y1, (x2 - x1).max(0.0), (y2 - y1).max(0.0)),
            true,
        )
    } else {
        (Rect::ZERO, false)
    }
}

fn threshold_bucket(thresholds: &[f64], ratio: f64, is_intersecting: bool) -> i32 {
    if !is_intersecting {
        return -1;
    }
    let mut bucket = 0i32;
    for &t in thresholds {
        if ratio >= t {
            bucket += 1;
        }
    }
    bucket
}

fn build_mutation_record_array(
    vm: &mut JsVm,
    host: &SharedHost,
    records: Vec<PendingMutationRecord>,
) -> HeapHandle {
    let arr = vm.alloc_array();
    for rec in records {
        let obj = vm.alloc_ordinary_object();
        let set_prop = |vm: &mut JsVm, name: &str, val: JsValue| {
            let k = vm.heap.intern_key(name);
            vm.heap
                .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
        };
        let ty = JsValue::from_string(vm.heap.alloc_string(rec.kind));
        set_prop(vm, "type", ty);
        let tgt = wrap_node(vm, host, rec.target);
        set_prop(vm, "target", tgt);

        let added_arr = vm.alloc_array();
        for n in rec.added {
            let v = wrap_node(vm, host, n);
            vm.heap.object_push_element(added_arr, v);
        }
        set_prop(vm, "addedNodes", JsValue::from_object(added_arr));

        let removed_arr = vm.alloc_array();
        for n in rec.removed {
            let v = wrap_node(vm, host, n);
            vm.heap.object_push_element(removed_arr, v);
        }
        set_prop(vm, "removedNodes", JsValue::from_object(removed_arr));

        let prev = rec
            .previous_sibling
            .map_or(JsValue::NULL, |n| wrap_node(vm, host, n));
        set_prop(vm, "previousSibling", prev);
        let next = rec
            .next_sibling
            .map_or(JsValue::NULL, |n| wrap_node(vm, host, n));
        set_prop(vm, "nextSibling", next);

        let attr_v = rec.attribute_name.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        });
        set_prop(vm, "attributeName", attr_v);
        let attr_ns_v = rec.attribute_namespace.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        });
        set_prop(vm, "attributeNamespace", attr_ns_v);
        let old_v = rec.old_value.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        });
        set_prop(vm, "oldValue", old_v);

        vm.heap.object_push_element(arr, JsValue::from_object(obj));
    }
    arr
}

fn alloc_rect_obj(vm: &mut JsVm, r: Rect) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    for (name, val) in [
        ("x", f64::from(r.origin.x)),
        ("y", f64::from(r.origin.y)),
        ("left", f64::from(r.origin.x)),
        ("top", f64::from(r.origin.y)),
        ("width", f64::from(r.size.width)),
        ("height", f64::from(r.size.height)),
        ("right", f64::from(r.origin.x + r.size.width)),
        ("bottom", f64::from(r.origin.y + r.size.height)),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_f64(val),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    obj
}

fn alloc_box_size_list(vm: &mut JsVm, r: Rect) -> HeapHandle {
    let arr = vm.alloc_array();
    let item = vm.alloc_ordinary_object();
    let ki = vm.heap.intern_key("inlineSize");
    let kb = vm.heap.intern_key("blockSize");
    vm.heap.object_define_data(
        item,
        ki,
        JsValue::from_f64(f64::from(r.size.width)),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        item,
        kb,
        JsValue::from_f64(f64::from(r.size.height)),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.heap.object_push_element(arr, JsValue::from_object(item));
    arr
}

fn build_resize_entry(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    border: Rect,
    content: Rect,
) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let tgt = wrap_node(vm, host, nid);
    let cr = alloc_rect_obj(vm, content);
    let bbs = alloc_box_size_list(vm, border);
    let cbs = alloc_box_size_list(vm, content);
    for (name, val) in [
        ("target", tgt),
        ("contentRect", JsValue::from_object(cr)),
        ("borderBoxSize", JsValue::from_object(bbs)),
        ("contentBoxSize", JsValue::from_object(cbs)),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    obj
}

fn build_intersection_entry(
    vm: &mut JsVm,
    host: &SharedHost,
    snap: IntersectionSnapshot,
) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let tgt = wrap_node(vm, host, snap.target);
    let br = alloc_rect_obj(vm, snap.bounding_rect);
    let rb = alloc_rect_obj(vm, snap.root_bounds);
    let ir = alloc_rect_obj(vm, snap.intersection_rect);
    for (name, val) in [
        ("target", tgt),
        ("boundingClientRect", JsValue::from_object(br)),
        ("rootBounds", JsValue::from_object(rb)),
        ("intersectionRect", JsValue::from_object(ir)),
        (
            "intersectionRatio",
            JsValue::from_f64(snap.intersection_ratio),
        ),
        ("isIntersecting", JsValue::from_bool(snap.is_intersecting)),
        ("time", JsValue::from_f64(snap.time_ms)),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    obj
}

// ---------------------------------------------------------------------------
// JS Constructors: MutationObserver, ResizeObserver, IntersectionObserver
// ---------------------------------------------------------------------------

fn install_mutation_observer(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(proto, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "observe", move |vm, this_val, args| {
        let Some(mo_h) = this_val.as_object() else {
            return Err(vm.type_error("MutationObserver.observe requires observer receiver"));
        };
        let Some(target) = h.borrow().node_for_value(vm, arg(args, 0)) else {
            return Err(vm.type_error("MutationObserver.observe target must be a DOM Node"));
        };
        let opts = arg(args, 1);
        let read_bool = |vm: &mut JsVm, name: &str| -> bool {
            opts.as_object().is_some_and(|oh| {
                let k = vm.heap.intern_key(name);
                vm.get_property_by_key(oh, k)
                    .is_ok_and(|v| vm.to_boolean(v))
            })
        };
        let child_list = read_bool(vm, "childList");
        let attribute_old_value = read_bool(vm, "attributeOldValue");
        let character_data_old_value = read_bool(vm, "characterDataOldValue");
        let subtree = read_bool(vm, "subtree");
        let mut attribute_filter = None;
        if let Some(oh) = opts.as_object() {
            let kf = vm.heap.intern_key("attributeFilter");
            if let Ok(fv) = vm.get_property_by_key(oh, kf) {
                if fv.is_object() {
                    if let Ok(items) = vm.collect_iterable(fv) {
                        attribute_filter = Some(
                            items
                                .into_iter()
                                .map(|v| vm.value_to_rust_string(v))
                                .collect(),
                        );
                    }
                }
            }
        }
        let attributes =
            read_bool(vm, "attributes") || attribute_old_value || attribute_filter.is_some();
        let character_data = read_bool(vm, "characterData") || character_data_old_value;
        if !child_list && !attributes && !character_data {
            return Err(vm.type_error(
                "MutationObserver.observe requires at least one of childList, attributes, or characterData",
            ));
        }
        if let Some(mo) = obs.borrow_mut().mutation_observers.get_mut(&mo_h) {
            mo.watches.retain(|w| w.target != target);
            mo.watches.push(MutationWatch {
                target,
                child_list,
                attributes,
                character_data,
                subtree,
                attribute_old_value,
                character_data_old_value,
                attribute_filter,
            });
        }
        Ok(JsValue::UNDEFINED)
    });

    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "disconnect", move |_vm, this_val, _| {
        if let Some(mo_h) = this_val.as_object() {
            if let Some(mo) = obs.borrow_mut().mutation_observers.get_mut(&mo_h) {
                mo.watches.clear();
                mo.pending.clear();
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "takeRecords", move |vm, this_val, _| {
        let records = this_val
            .as_object()
            .and_then(|mo_h| {
                obs.borrow_mut()
                    .mutation_observers
                    .get_mut(&mo_h)
                    .map(|mo| std::mem::take(&mut mo.pending))
            })
            .unwrap_or_default();
        Ok(JsValue::from_object(build_mutation_record_array(
            vm, &h, records,
        )))
    });

    let obs = Rc::clone(observers);
    let ctor = vm.alloc_host_fn("MutationObserver", move |vm, _, args| {
        let Some(cb) = arg(args, 0)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Err(vm.type_error("MutationObserver requires a callback function"));
        };
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        obs.borrow_mut().mutation_observers.insert(
            obj,
            MutationObserverEntry {
                observer_obj: obj,
                callback: cb,
                watches: Vec::new(),
                pending: Vec::new(),
            },
        );
        Ok(JsValue::from_object(obj))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "MutationObserver", JsValue::from_object(ctor));
}

fn install_resize_observer(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(proto, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "observe", move |vm, this_val, args| {
        let Some(ro_h) = this_val.as_object() else {
            return Err(vm.type_error("ResizeObserver.observe requires observer receiver"));
        };
        let Some(target) = h.borrow().node_for_value(vm, arg(args, 0)) else {
            return Err(vm.type_error("ResizeObserver.observe target must be an Element"));
        };
        if let Some(ro) = obs.borrow_mut().resize_observers.get_mut(&ro_h) {
            if !ro.targets.contains(&target) {
                ro.targets.push(target);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "unobserve", move |vm, this_val, args| {
        if let (Some(ro_h), Some(target)) = (
            this_val.as_object(),
            h.borrow().node_for_value(vm, arg(args, 0)),
        ) {
            if let Some(ro) = obs.borrow_mut().resize_observers.get_mut(&ro_h) {
                ro.targets.retain(|&t| t != target);
                ro.last_sizes.remove(&target);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "disconnect", move |_vm, this_val, _| {
        if let Some(ro_h) = this_val.as_object() {
            if let Some(ro) = obs.borrow_mut().resize_observers.get_mut(&ro_h) {
                ro.targets.clear();
                ro.last_sizes.clear();
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let obs = Rc::clone(observers);
    let ctor = vm.alloc_host_fn("ResizeObserver", move |vm, _, args| {
        let Some(cb) = arg(args, 0)
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Err(vm.type_error("ResizeObserver requires a callback function"));
        };
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        obs.borrow_mut().resize_observers.insert(
            obj,
            ResizeObserverEntry {
                observer_obj: obj,
                callback: cb,
                targets: Vec::new(),
                last_sizes: HashMap::new(),
            },
        );
        Ok(JsValue::from_object(obj))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "ResizeObserver", JsValue::from_object(ctor));
}

fn install_intersection_observer(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(proto, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "observe", move |vm, this_val, args| {
        let Some(io_h) = this_val.as_object() else {
            return Err(vm.type_error("IntersectionObserver.observe requires observer receiver"));
        };
        let Some(target) = h.borrow().node_for_value(vm, arg(args, 0)) else {
            return Err(vm.type_error("IntersectionObserver.observe target must be an Element"));
        };
        if let Some(io) = obs.borrow_mut().intersection_observers.get_mut(&io_h) {
            if !io.targets.contains(&target) {
                io.targets.push(target);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "unobserve", move |vm, this_val, args| {
        if let (Some(io_h), Some(target)) = (
            this_val.as_object(),
            h.borrow().node_for_value(vm, arg(args, 0)),
        ) {
            if let Some(io) = obs.borrow_mut().intersection_observers.get_mut(&io_h) {
                io.targets.retain(|&t| t != target);
                io.last_state.remove(&target);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "disconnect", move |_vm, this_val, _| {
        if let Some(io_h) = this_val.as_object() {
            if let Some(io) = obs.borrow_mut().intersection_observers.get_mut(&io_h) {
                io.targets.clear();
                io.last_state.clear();
                io.pending_records.clear();
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "takeRecords", move |vm, this_val, _| {
        let snaps = this_val
            .as_object()
            .and_then(|io_h| {
                obs.borrow_mut()
                    .intersection_observers
                    .get_mut(&io_h)
                    .map(|io| std::mem::take(&mut io.pending_records))
            })
            .unwrap_or_default();
        let arr = vm.alloc_array();
        for s in snaps {
            let entry = build_intersection_entry(vm, &h, s);
            vm.heap
                .object_push_element(arr, JsValue::from_object(entry));
        }
        Ok(JsValue::from_object(arr))
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    let ctor = vm.alloc_host_fn("IntersectionObserver", move |vm, _, args| {
        let Some(cb) = arg(args, 0)
            .as_object()
            .filter(|&fh| vm.is_callable_handle(fh))
        else {
            return Err(vm.type_error("IntersectionObserver requires a callback function"));
        };
        let opts = arg(args, 1);
        let mut root = None;
        let mut thresholds = vec![0.0];
        if let Some(oh) = opts.as_object() {
            let kr = vm.heap.intern_key("root");
            if let Ok(rv) = vm.get_property_by_key(oh, kr) {
                root = h.borrow().node_for_value(vm, rv);
            }
            let kt = vm.heap.intern_key("threshold");
            if let Ok(tv) = vm.get_property_by_key(oh, kt) {
                if let Some(n) = tv.as_number() {
                    thresholds = vec![n.clamp(0.0, 1.0)];
                } else if tv.is_object() {
                    if let Ok(items) = vm.collect_iterable(tv) {
                        let mut parsed = Vec::new();
                        for it in items {
                            if let Ok(n) = vm.to_number(it) {
                                parsed.push(n.clamp(0.0, 1.0));
                            }
                        }
                        if !parsed.is_empty() {
                            parsed.sort_by(f64::total_cmp);
                            thresholds = parsed;
                        }
                    }
                }
            }
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        obs.borrow_mut().intersection_observers.insert(
            obj,
            IntersectionObserverEntry {
                observer_obj: obj,
                callback: cb,
                root,
                thresholds,
                targets: Vec::new(),
                last_state: HashMap::new(),
                pending_records: Vec::new(),
            },
        );
        Ok(JsValue::from_object(obj))
    });
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "IntersectionObserver", JsValue::from_object(ctor));
}
