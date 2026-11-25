//! Web Platform Observers:
//! - `MutationObserver` (microtask-batched `MutationRecord` delivery)
//! - `ResizeObserver` (W3C Resize Observer Level 1: `content-box`, `border-box`,
//!   `device-pixel-content-box`, logical writing-mode sizes, multi-fragment boxes,
//!   SVG boxes, zoom scaling, and §4.2 depth-loop delivery with loop-error reporting)
//! - `IntersectionObserver` (W3C Intersection Observer v1 + v2: `root`, `rootMargin`,
//!   `scrollMargin`, `thresholds`, `delay`, `trackVisibility`, `isVisible` occlusion/effects,
//!   transformed/SVG/Text/iframe targets, intermediate scroll clipping, and edge-inclusive math)

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_core::{Point, Rect};
use rix_css::{
    ClipPathValue, DisplayMode, FilterOp, MixBlendMode, OverflowMode, PositionMode, VisibilityMode,
};
use rix_dom::{NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::Fragment;

use crate::cssom_geometry::{
    collect_boxes_for_node, mat4_project_2d, svg_outer_root_clip_rect, svg_stroke_expansion,
    transformed_bounding_rect_for_node,
};
use crate::cssom_view::{
    current_css_zoom_impl, establishes_fixed_cb, flat_tree_parent, is_scroll_container_style,
};
use crate::dom_bridge::{define_host_accessor, wrap_node, DomMutation, SharedHost};
use crate::event_loop::SharedEventLoop;

// ---------------------------------------------------------------------------
// Data Structures
// ---------------------------------------------------------------------------

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeBoxOption {
    Content,
    Border,
    DevicePixelContent,
}

#[derive(Debug, Clone)]
struct ResizeTargetObservation {
    target: NodeId,
    box_opt: ResizeBoxOption,
    last_reported: Option<Vec<(f64, f64)>>,
}

struct ResizeObserverEntry {
    observer_obj: HeapHandle,
    callback: HeapHandle,
    observations: Vec<ResizeTargetObservation>,
}

#[derive(Debug, Clone, Copy)]
enum MarginValue {
    Px(f64),
    Percent(f64),
}

impl MarginValue {
    fn resolve(self, basis: f64) -> f64 {
        match self {
            Self::Px(v) => v,
            Self::Percent(p) => basis * p / 100.0,
        }
    }

    fn format_css(self) -> String {
        match self {
            Self::Px(v) => {
                if (v - v.trunc()).abs() < 1e-6 {
                    format!("{}px", v as i64)
                } else {
                    format!("{v}px")
                }
            }
            Self::Percent(p) => {
                if (p - p.trunc()).abs() < 1e-6 {
                    format!("{}%", p as i64)
                } else {
                    format!("{p}%")
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MarginBox {
    top: MarginValue,
    right: MarginValue,
    bottom: MarginValue,
    left: MarginValue,
}

impl Default for MarginBox {
    fn default() -> Self {
        Self {
            top: MarginValue::Px(0.0),
            right: MarginValue::Px(0.0),
            bottom: MarginValue::Px(0.0),
            left: MarginValue::Px(0.0),
        }
    }
}

impl MarginBox {
    fn to_css_string(self) -> String {
        format!(
            "{} {} {} {}",
            self.top.format_css(),
            self.right.format_css(),
            self.bottom.format_css(),
            self.left.format_css()
        )
    }

    fn expand_rect(self, r: Rect) -> Rect {
        let w = f64::from(r.size.width);
        let h = f64::from(r.size.height);
        let top = self.top.resolve(h);
        let right = self.right.resolve(w);
        let bottom = self.bottom.resolve(h);
        let left = self.left.resolve(w);
        let nx = f64::from(r.origin.x) - left;
        let ny = f64::from(r.origin.y) - top;
        let nw = (w + left + right).max(0.0);
        let nh = (h + top + bottom).max(0.0);
        Rect::new(nx as f32, ny as f32, nw as f32, nh as f32)
    }
}

struct IntersectionObserverEntry {
    observer_obj: HeapHandle,
    callback: HeapHandle,
    root: Option<NodeId>,
    root_margin: MarginBox,
    scroll_margin: MarginBox,
    thresholds: Vec<f64>,
    thresholds_arr: Option<HeapHandle>,
    delay: f64,
    track_visibility: bool,
    targets: Vec<NodeId>,
    last_state: HashMap<NodeId, (bool, i32, bool)>,
    last_notified_ms: HashMap<NodeId, f64>,
    pending_records: Vec<IntersectionSnapshot>,
}

#[derive(Debug, Clone, Copy)]
struct IntersectionSnapshot {
    target: NodeId,
    bounding_rect: Rect,
    root_bounds: Option<Rect>,
    intersection_rect: Rect,
    intersection_ratio: f64,
    is_intersecting: bool,
    is_visible: bool,
    time_ms: f64,
}

/// Shared registry for `MutationObserver`, `ResizeObserver`, and `IntersectionObserver`.
#[derive(Default)]
pub struct ObserverSystem {
    mutation_observers: HashMap<HeapHandle, MutationObserverEntry>,
    resize_observers: HashMap<HeapHandle, ResizeObserverEntry>,
    intersection_observers: HashMap<HeapHandle, IntersectionObserverEntry>,
    resize_order: Vec<HeapHandle>,
    intersection_order: Vec<HeapHandle>,
    resize_entry_proto: Option<HeapHandle>,
    resize_size_proto: Option<HeapHandle>,
    intersection_entry_proto: Option<HeapHandle>,
}

/// Shared handle to [`ObserverSystem`].
pub type SharedObservers = Rc<RefCell<ObserverSystem>>;

impl ObserverSystem {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Collects GC roots for active observer instances, callbacks, and entry prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.resize_entry_proto,
            self.resize_size_proto,
            self.intersection_entry_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for mo in self.mutation_observers.values() {
            if !mo.watches.is_empty() || !mo.pending.is_empty() {
                out.push(JsValue::from_object(mo.observer_obj));
                out.push(JsValue::from_object(mo.callback));
            }
        }
        for ro in self.resize_observers.values() {
            if !ro.observations.is_empty() {
                out.push(JsValue::from_object(ro.observer_obj));
                out.push(JsValue::from_object(ro.callback));
            }
        }
        for io in self.intersection_observers.values() {
            if let Some(arr) = io.thresholds_arr {
                out.push(JsValue::from_object(arr));
            }
            if !io.targets.is_empty() || !io.pending_records.is_empty() {
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

fn define_host_getter(
    vm: &mut JsVm,
    target: HeapHandle,
    name: &str,
    f: impl Fn(&mut JsVm, JsValue, &[JsValue]) -> Result<JsValue, JsException> + 'static,
) {
    let g = vm.alloc_host_fn(name, f);
    define_host_accessor(vm, target, name, Some(g), None);
}

fn throw_syntax_dom_exception(vm: &mut JsVm, msg: &str) -> JsException {
    crate::web_apis::create_dom_exception(vm, "SyntaxError", msg)
}

/// Parses a W3C IntersectionObserver `rootMargin` / `scrollMargin` string (1–4 tokens of
/// absolute CSS lengths `px|cm|mm|q|in|pt|pc` or `%`).
fn parse_observer_margin(raw: &str) -> Result<MarginBox, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(MarginBox::default());
    }
    let tokens: Vec<&str> = trimmed.split_ascii_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 4 {
        return Err(format!("Invalid margin syntax '{raw}'"));
    }
    let mut vals = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let lower = tok.to_ascii_lowercase();
        let mv = if let Some(num_s) = lower.strip_suffix('%') {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Percent(n)
        } else if let Some(num_s) = lower.strip_suffix("px") {
            MarginValue::Px(parse_strict_css_num(num_s)?)
        } else if let Some(num_s) = lower.strip_suffix("cm") {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 96.0 / 2.54).trunc())
        } else if let Some(num_s) = lower.strip_suffix("mm") {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 96.0 / 25.4).trunc())
        } else if let Some(num_s) = lower.strip_suffix("in") {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 96.0).trunc())
        } else if let Some(num_s) = lower.strip_suffix("pt") {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 96.0 / 72.0).trunc())
        } else if let Some(num_s) = lower.strip_suffix("pc") {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 16.0).trunc())
        } else if let Some(num_s) = lower.strip_suffix('q') {
            let n = parse_strict_css_num(num_s)?;
            MarginValue::Px((n * 96.0 / 101.6).trunc())
        } else {
            return Err(format!("Invalid margin unit in '{tok}'"));
        };
        vals.push(mv);
    }
    let (top, right, bottom, left) = match vals.as_slice() {
        [a] => (*a, *a, *a, *a),
        [a, b] => (*a, *b, *a, *b),
        [a, b, c] => (*a, *b, *c, *b),
        [a, b, c, d] => (*a, *b, *c, *d),
        _ => return Err("Invalid margin count".to_string()),
    };
    Ok(MarginBox {
        top,
        right,
        bottom,
        left,
    })
}

fn parse_strict_css_num(s: &str) -> Result<f64, String> {
    if s.is_empty() || s.starts_with("inf") || s.starts_with("nan") {
        return Err(format!("Invalid number '{s}'"));
    }
    let v = s
        .parse::<f64>()
        .map_err(|_| format!("Invalid number '{s}'"))?;
    if !v.is_finite() {
        return Err(format!("Non-finite number '{s}'"));
    }
    Ok(v)
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

// ---------------------------------------------------------------------------
// ResizeObserver Box Measurement & §4.2 Depth-Loop Delivery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MeasuredResizeBox {
    content_rect: Rect,
    border_sizes: Vec<(f64, f64)>,
    content_sizes: Vec<(f64, f64)>,
    device_pixel_sizes: Vec<(f64, f64)>,
}

impl MeasuredResizeBox {
    fn observed_sizes(&self, opt: ResizeBoxOption) -> Vec<(f64, f64)> {
        match opt {
            ResizeBoxOption::Content => self.content_sizes.clone(),
            ResizeBoxOption::Border => self.border_sizes.clone(),
            ResizeBoxOption::DevicePixelContent => self.device_pixel_sizes.clone(),
        }
    }
}

fn node_depth_for_resize(host: &SharedHost, nid: NodeId) -> usize {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if !doc.is_connected(nid) {
        return 0;
    }
    let mut depth = 1usize;
    let mut cur = nid;
    while let Some(p) = flat_tree_parent(&doc, cur) {
        depth += 1;
        cur = p;
    }
    depth
}

fn is_replaced_html_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "img"
            | "video"
            | "audio"
            | "canvas"
            | "iframe"
            | "embed"
            | "object"
            | "input"
            | "textarea"
            | "select"
            | "button"
            | "meter"
            | "progress"
            | "svg"
    )
}

fn measure_resize_target(host: &SharedHost, nid: NodeId) -> MeasuredResizeBox {
    let dpr = f64::from(host.borrow().device_pixel_ratio);
    let zoom = current_css_zoom_impl(host, nid);
    let (is_connected, is_svg_any, is_svg_child, is_vertical, display_none, is_non_replaced_inline) = {
        let style_opt = host.borrow_mut().computed_style_for_node(nid);
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let conn = doc.is_connected(nid);
        let svg_ns = rix_dom::Namespace::from("http://www.w3.org/2000/svg");
        let (svg_any, svg_child, replaced_html) = match doc.get(nid) {
            Some(NodeData::Element(el)) => {
                let in_svg = el.name.ns == svg_ns;
                let parent_in_svg = doc.parent(nid).is_some_and(
                    |p| matches!(doc.get(p), Some(NodeData::Element(pe)) if pe.name.ns == svg_ns),
                );
                let child_svg = in_svg
                    && (!el.name.local.as_str().eq_ignore_ascii_case("svg") || parent_in_svg);
                (
                    in_svg,
                    child_svg,
                    is_replaced_html_tag(el.name.local.as_str()),
                )
            }
            _ => (false, false, false),
        };
        let vert = style_opt
            .as_ref()
            .is_some_and(|st| st.text.writing_mode.is_vertical());
        let dnone = style_opt.as_ref().is_some_and(|st| {
            st.display == DisplayMode::None || st.display == DisplayMode::Contents
        });
        // W3C ResizeObserver §3.1: non-replaced inline HTML elements report 0x0.
        let non_rep_inline = !svg_any
            && !replaced_html
            && style_opt
                .as_ref()
                .is_some_and(|st| st.display == DisplayMode::Inline);
        (conn, svg_any, svg_child, vert, dnone, non_rep_inline)
    };
    if !is_connected || display_none || is_non_replaced_inline {
        return MeasuredResizeBox {
            content_rect: Rect::ZERO,
            border_sizes: vec![(0.0, 0.0)],
            content_sizes: vec![(0.0, 0.0)],
            device_pixel_sizes: vec![(0.0, 0.0)],
        };
    }

    let boxes = collect_boxes_for_node(host, nid);
    if boxes.is_empty() {
        return MeasuredResizeBox {
            content_rect: Rect::ZERO,
            border_sizes: vec![(0.0, 0.0)],
            content_sizes: vec![(0.0, 0.0)],
            device_pixel_sizes: vec![(0.0, 0.0)],
        };
    }

    let to_logical = |w: f64, h: f64| -> (f64, f64) {
        if is_vertical {
            (h, w)
        } else {
            (w, h)
        }
    };

    let mut border_sizes = Vec::with_capacity(boxes.len());
    let mut content_sizes = Vec::with_capacity(boxes.len());
    let mut device_pixel_sizes = Vec::with_capacity(boxes.len());

    let (w_is_pct, h_is_pct) =
        host.borrow_mut()
            .computed_style_for_node(nid)
            .map_or((false, false), |st| {
                (
                    matches!(
                        st.box_model.width,
                        rix_css::SizeValue::Length(rix_css::LengthPercentage::Percent(_))
                    ),
                    matches!(
                        st.box_model.height,
                        rix_css::SizeValue::Length(rix_css::LengthPercentage::Percent(_))
                    ),
                )
            });
    let w_div = if w_is_pct && (zoom - 1.0).abs() > 1e-6 {
        zoom
    } else {
        1.0
    };
    let h_div = if h_is_pct && (zoom - 1.0).abs() > 1e-6 {
        zoom
    } else {
        1.0
    };

    let first = &boxes[0];
    let first_bw = f64::from(first.border_size.width) / w_div;
    let first_bh = f64::from(first.border_size.height) / h_div;
    let first_cw =
        (first_bw - f64::from(first.border.horizontal()) - f64::from(first.padding.horizontal()))
            .max(0.0);
    let first_ch =
        (first_bh - f64::from(first.border.vertical()) - f64::from(first.padding.vertical()))
            .max(0.0);
    let content_rect = if is_svg_child {
        Rect::new(0.0, 0.0, first_bw as f32, first_bh as f32)
    } else if is_svg_any {
        Rect::new(0.0, 0.0, first_cw as f32, first_ch as f32)
    } else {
        // W3C Resize Observer §3.1: contentRect.x/y = padding-left / padding-top.
        Rect::new(
            first.padding.left,
            first.padding.top,
            first_cw as f32,
            first_ch as f32,
        )
    };

    for bx in &boxes {
        let bw = f64::from(bx.border_size.width) / w_div;
        let bh = f64::from(bx.border_size.height) / h_div;
        let cw = if is_svg_child {
            bw
        } else {
            (bw - f64::from(bx.border.horizontal()) - f64::from(bx.padding.horizontal())).max(0.0)
        };
        let ch = if is_svg_child {
            bh
        } else {
            (bh - f64::from(bx.border.vertical()) - f64::from(bx.padding.vertical())).max(0.0)
        };
        let (b_in, b_bl) = to_logical(bw, bh);
        let (c_in, c_bl) = to_logical(cw, ch);
        border_sizes.push((b_in, b_bl));
        content_sizes.push((c_in, c_bl));
        // CSS Viewport zoom-om: devicePixelContentBoxSize is NOT divided by zoom!
        let dp_in = (c_in * zoom * dpr).round();
        let dp_bl = (c_bl * zoom * dpr).round();
        device_pixel_sizes.push((dp_in, dp_bl));
    }

    MeasuredResizeBox {
        content_rect,
        border_sizes,
        content_sizes,
        device_pixel_sizes,
    }
}

fn sizes_equal(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|((ai, ab), (bi, bb))| (ai - bi).abs() < 1e-4 && (ab - bb).abs() < 1e-4)
}

fn fire_resize_loop_error(vm: &mut JsVm) {
    let global = vm.global;
    let msg = "ResizeObserver loop completed with undelivered notifications.";
    let k_oe = vm.heap.intern_key("onerror");
    if let Ok(oe) = vm.get_property_by_key(global, k_oe) {
        if oe.as_object().is_some_and(|h| vm.is_callable_handle(h)) {
            let ms = JsValue::from_string(vm.heap.alloc_string(msg));
            let empty = JsValue::from_string(vm.heap.alloc_string(""));
            let zero = JsValue::from_i32(0);
            let _ = vm.call_value(
                oe,
                JsValue::from_object(global),
                &[ms, empty, zero, zero, JsValue::NULL],
            );
            return;
        }
    }
    let k_ec = vm.heap.intern_key("ErrorEvent");
    let k_de = vm.heap.intern_key("dispatchEvent");
    if let (Ok(ctor), Ok(de)) = (
        vm.get_property_by_key(global, k_ec),
        vm.get_property_by_key(global, k_de),
    ) {
        if ctor.as_object().is_some() && de.as_object().is_some() {
            let init = vm.alloc_ordinary_object();
            let km = vm.heap.intern_key("message");
            let ms = JsValue::from_string(vm.heap.alloc_string(msg));
            vm.heap
                .object_define_data(init, km, ms, PropertyAttrs::DEFAULT, &mut vm.shapes);
            let ty = JsValue::from_string(vm.heap.alloc_string("error"));
            if let Ok(ev) = vm.construct_value(ctor, &[ty, JsValue::from_object(init)], ctor) {
                let _ = vm.call_value(de, JsValue::from_object(global), &[ev]);
            }
        }
    }
}

fn deliver_resize_observers(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let mut depth_limit: isize = -1;
    let mut iterations = 0usize;

    loop {
        iterations += 1;
        if iterations > 32 {
            break;
        }
        // Gather active observations with depth > depth_limit.
        type Batch = (HeapHandle, HeapHandle, Vec<(NodeId, MeasuredResizeBox)>);
        let (batches, shallowest, has_skipped) = {
            let mut os = observers.borrow_mut();
            let order = os.resize_order.clone();
            let mut batches: Vec<Batch> = Vec::new();
            let mut shallowest = isize::MAX;
            let mut has_skipped = false;

            for ro_h in order {
                let Some(ro) = os.resize_observers.get_mut(&ro_h) else {
                    continue;
                };
                if ro.observations.is_empty() {
                    continue;
                }
                let mut active = Vec::new();
                for obs in &mut ro.observations {
                    let measured = measure_resize_target(host, obs.target);
                    let cur = measured.observed_sizes(obs.box_opt);
                    let changed = obs
                        .last_reported
                        .as_deref()
                        .is_none_or(|prev| !sizes_equal(prev, &cur));
                    if changed {
                        let d = node_depth_for_resize(host, obs.target) as isize;
                        if d > depth_limit {
                            shallowest = shallowest.min(d);
                            obs.last_reported = Some(cur);
                            active.push((obs.target, measured));
                        } else {
                            has_skipped = true;
                        }
                    }
                }
                if !active.is_empty() {
                    batches.push((ro.observer_obj, ro.callback, active));
                }
            }
            (batches, shallowest, has_skipped)
        };

        if batches.is_empty() {
            if has_skipped {
                fire_resize_loop_error(vm);
            }
            break;
        }

        let (entry_proto, size_proto) = {
            let os = observers.borrow();
            (os.resize_entry_proto, os.resize_size_proto)
        };
        for (obs_obj, cb, entries) in batches {
            let arr = vm.alloc_array();
            for (nid, measured) in entries {
                let entry = build_resize_entry(vm, host, nid, &measured, entry_proto, size_proto);
                vm.heap
                    .object_push_element(arr, JsValue::from_object(entry));
            }
            let _ = vm.call_value(
                JsValue::from_object(cb),
                JsValue::from_object(obs_obj),
                &[JsValue::from_object(arr), JsValue::from_object(obs_obj)],
            );
        }

        depth_limit = shallowest;
    }
}

// ---------------------------------------------------------------------------
// IntersectionObserver v1 + v2 Computation
// ---------------------------------------------------------------------------

fn owner_iframe_of_doc(host: &SharedHost, doc_id: NodeId) -> Option<NodeId> {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    let main_root = doc.root();
    if doc_id == main_root {
        return None;
    }
    let found = doc
        .descendants_with_shadow_roots(main_root)
        .into_iter()
        .find(|&n| {
            matches!(
                doc.get(n),
                Some(NodeData::Element(el)) if el.content_document == Some(doc_id)
            )
        });
    found
}

fn clip_rect_by_clip_path_inset(host: &SharedHost, node: NodeId, base_rect: Rect) -> Rect {
    let Some(st) = host.borrow_mut().computed_style_for_node(node) else {
        return base_rect;
    };
    let Some(fx) = st.effects.as_deref() else {
        return base_rect;
    };
    let ClipPathValue::Inset {
        ref top,
        ref right,
        ref bottom,
        ref left,
        ..
    } = fx.clip_path
    else {
        return base_rect;
    };
    let eval = rix_css::EvalContext {
        font_size_px: st.text.font_size_px,
        ..rix_css::EvalContext::default()
    };
    let w = base_rect.size.width;
    let h = base_rect.size.height;
    let it = top.resolve_px(h, &eval);
    let ir = right.resolve_px(w, &eval);
    let ib = bottom.resolve_px(h, &eval);
    let il = left.resolve_px(w, &eval);
    let x1 = base_rect.origin.x + il;
    let y1 = base_rect.origin.y + it;
    let x2 = base_rect.origin.x + w - ir;
    let y2 = base_rect.origin.y + h - ib;
    if x2 < x1 || y2 < y1 {
        Rect::new(x1, y1, -1.0, -1.0)
    } else {
        Rect::new(x1, y1, x2 - x1, y2 - y1)
    }
}

struct AncestorClippers {
    scroll_clippers: Vec<NodeId>,
    clip_path_ancestors: Vec<NodeId>,
}

/// Checks if `target` is in the containing-block chain of `root_elem` (and collects intermediate
/// scroll containers and `clip-path` ancestors along the containing-block chain).
fn containing_block_chain_to_root(
    host: &SharedHost,
    target: NodeId,
    root_opt: Option<NodeId>,
) -> Option<AncestorClippers> {
    let target_owner = host.borrow().owner_doc_of(target);
    let is_connected = host.borrow().document.borrow().is_connected(target);
    if !is_connected {
        return None;
    }
    // Explicit root in a different document cannot be in target's same-document CB chain.
    if let Some(r_id) = root_opt {
        let root_owner = host.borrow().owner_doc_of(r_id);
        let root_is_doc = matches!(
            host.borrow().document.borrow().get(r_id),
            Some(NodeData::Document)
        );
        let root_doc = if root_is_doc { r_id } else { root_owner };
        if root_doc != target_owner {
            return None;
        }
    }
    // Verify target itself (if Element) is not display:none.
    let target_is_elem = matches!(
        host.borrow().document.borrow().get(target),
        Some(NodeData::Element(_))
    );
    if target_is_elem {
        if let Some(st) = host.borrow_mut().computed_style_for_node(target) {
            if st.display == DisplayMode::None {
                return None;
            }
        }
    }

    let mut scroll_clippers = Vec::new();
    let mut clip_path_ancestors = Vec::new();
    let mut cur = target;
    let mut reached_root = root_opt.is_none();

    loop {
        let cur_pos = if matches!(
            host.borrow().document.borrow().get(cur),
            Some(NodeData::Element(_))
        ) {
            host.borrow_mut()
                .computed_style_for_node(cur)
                .map_or(PositionMode::Static, |s| s.position)
        } else {
            PositionMode::Static
        };

        // Find the next containing block ancestor of `cur`.
        let next_cb: Option<NodeId> = {
            let ancestors: Vec<NodeId> = {
                let hb = host.borrow();
                let doc = hb.document.borrow();
                let mut list = Vec::new();
                let mut p = flat_tree_parent(&doc, cur);
                while let Some(a) = p {
                    list.push(a);
                    p = flat_tree_parent(&doc, a);
                }
                list
            };
            let mut chosen = None;
            for a in ancestors {
                if matches!(
                    host.borrow().document.borrow().get(a),
                    Some(NodeData::Document)
                ) {
                    chosen = Some(a);
                    break;
                }
                if let Some(ast) = host.borrow_mut().computed_style_for_node(a) {
                    if ast.display == DisplayMode::None {
                        return None;
                    }
                    if ast.display == DisplayMode::Contents {
                        continue;
                    }
                    let matches_cb = match cur_pos {
                        PositionMode::Fixed => establishes_fixed_cb(&ast),
                        PositionMode::Absolute => {
                            ast.position != PositionMode::Static || establishes_fixed_cb(&ast)
                        }
                        _ => true,
                    };
                    if matches_cb {
                        chosen = Some(a);
                        break;
                    }
                }
            }
            chosen
        };

        let Some(cb) = next_cb else {
            break;
        };
        if root_opt == Some(cb) {
            reached_root = true;
            break;
        }
        if matches!(
            host.borrow().document.borrow().get(cb),
            Some(NodeData::Document)
        ) {
            if root_opt == Some(cb) {
                reached_root = true;
            }
            break;
        }
        let cb_style_opt = host.borrow_mut().computed_style_for_node(cb);
        if let Some(st) = cb_style_opt {
            let doc_el = host
                .borrow()
                .document
                .borrow()
                .document_element_of(target_owner);
            let doc_el_style = doc_el.and_then(|de| host.borrow_mut().computed_style_for_node(de));
            let is_body_root_prop = doc_el_style.is_some_and(|ds| {
                ds.overflow.x == OverflowMode::Visible && ds.overflow.y == OverflowMode::Visible
            }) && matches!(
                host.borrow().document.borrow().get(cb),
                Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("body")
            );
            let is_table_root = matches!(st.display, DisplayMode::Table | DisplayMode::InlineTable);
            if doc_el != Some(cb)
                && !is_body_root_prop
                && !is_table_root
                && is_scroll_container_style(&st)
            {
                scroll_clippers.push(cb);
            } else if st
                .effects
                .as_deref()
                .is_some_and(|fx| matches!(fx.clip_path, ClipPathValue::Inset { .. }))
            {
                clip_path_ancestors.push(cb);
            }
        }
        cur = cb;
    }

    if !reached_root {
        return None;
    }
    Some(AncestorClippers {
        scroll_clippers,
        clip_path_ancestors,
    })
}

fn root_intersection_rect(
    host: &SharedHost,
    root_opt: Option<NodeId>,
    root_margin: MarginBox,
) -> Option<(Rect, bool)> {
    let (base, is_scroll_root) = match root_opt {
        None => {
            let vp = host.borrow().viewport;
            (Rect::new(0.0, 0.0, vp.width, vp.height), true)
        }
        Some(r_id) => {
            let is_doc = matches!(
                host.borrow().document.borrow().get(r_id),
                Some(NodeData::Document)
            );
            if is_doc {
                let vp = host.borrow_mut().viewport_for_doc(r_id);
                (Rect::new(0.0, 0.0, vp.width, vp.height), true)
            } else {
                let info = host.borrow_mut().element_geometry_for_node(r_id)?;
                // CSS Tables L3: `display: table` / `inline-table` do not establish scroll clipping.
                let is_table = host
                    .borrow_mut()
                    .computed_style_for_node(r_id)
                    .is_some_and(|st| {
                        matches!(st.display, DisplayMode::Table | DisplayMode::InlineTable)
                    });
                let has_clip = !is_table
                    && (info.overflow_x != OverflowMode::Visible
                        || info.overflow_y != OverflowMode::Visible);
                let r = if has_clip {
                    info.abs_padding
                } else {
                    info.abs_border
                };
                (
                    Rect::new(
                        r.origin.x - info.ancestor_scroll.x,
                        r.origin.y - info.ancestor_scroll.y,
                        r.size.width,
                        r.size.height,
                    ),
                    has_clip,
                )
            }
        }
    };
    Some((root_margin.expand_rect(base), is_scroll_root))
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let min_x = a.origin.x.min(b.origin.x);
    let min_y = a.origin.y.min(b.origin.y);
    let max_x = a.max_x().max(b.max_x());
    let max_y = a.max_y().max(b.max_y());
    Rect::new(
        min_x,
        min_y,
        (max_x - min_x).max(0.0),
        (max_y - min_y).max(0.0),
    )
}

fn check_target_visibility_v2(host: &SharedHost, target: NodeId, intersection_rect: Rect) -> bool {
    // 1. Check self & all ancestors for visual effects that invalidate IOv2 visibility.
    let chain: Vec<NodeId> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut out = vec![target];
        let mut cur = target;
        while let Some(p) = flat_tree_parent(&doc, cur) {
            if matches!(doc.get(p), Some(NodeData::Element(_))) {
                out.push(p);
            }
            cur = p;
        }
        out
    };
    for &n in &chain {
        if let Some(st) = host.borrow_mut().computed_style_for_node(n) {
            if st.opacity < 1.0 || st.visibility != VisibilityMode::Visible {
                return false;
            }
            if let Some(fx) = st.effects.as_deref() {
                if !fx.filter.is_empty() || fx.mix_blend_mode != MixBlendMode::Normal {
                    return false;
                }
            }
        }
    }

    // 2. Check paint-order occlusion by any later non-ancestor/non-descendant fragment
    //    (including box-shadow and blur/drop-shadow filter expansion).
    //    Use live computed styles from `style_engine` so paint-only mutations
    //    (e.g. `opacity = 0`, `boxShadow = "none"`) are immediately reflected even when
    //    `layout_eq` skips rebuilding the fragment tree.
    let mut hb = host.borrow_mut();
    let root_scroll = hb.scroll_offset;
    let scroll_map = hb.element_scroll_offsets.clone();
    let doc_rc = Rc::clone(&hb.document);
    let _ = hb.ensure_styles();
    let _ = hb.ensure_layout();
    let Some(tree) = hb.cached_fragment_tree() else {
        return true;
    };
    let Some(root_frag) = tree.root.as_ref() else {
        return true;
    };
    let styles = &hb.style_ctx;
    let doc = doc_rc.borrow();

    struct OcclusionWalk<'a> {
        doc: &'a rix_dom::Document,
        styles: &'a rix_css::StyleContext,
        target: NodeId,
        inter: Rect,
        seen_target: bool,
        occluded: bool,
    }
    fn walk_occ(
        st: &mut OcclusionWalk<'_>,
        frag: &Fragment,
        parent_origin: Point,
        scroll_acc: Point,
        scroll_map: &HashMap<NodeId, Point>,
        parent_opacity: f32,
        is_root: bool,
    ) {
        if st.occluded {
            return;
        }
        let live_style = frag
            .dom_node
            .and_then(|n| st.styles.style_for(n))
            .unwrap_or(&frag.style);
        let eff_opacity = parent_opacity * live_style.opacity;
        let abs = frag.absolute_border_rect(parent_origin);
        let eff_scroll = if !is_root && live_style.position == PositionMode::Fixed {
            Point::ZERO
        } else {
            scroll_acc
        };
        let vp_rect = Rect::new(
            abs.origin.x - eff_scroll.x,
            abs.origin.y - eff_scroll.y,
            abs.size.width,
            abs.size.height,
        );
        if frag.dom_node == Some(st.target) {
            st.seen_target = true;
        } else if st.seen_target
            && eff_opacity > 0.0
            && live_style.visibility == VisibilityMode::Visible
        {
            let is_desc = frag
                .dom_node
                .is_some_and(|n| st.doc.ancestors(n).any(|a| a == st.target));
            if !is_desc {
                let mut occ_rect = vp_rect;
                let mut has_shadow = false;
                let eval = rix_css::EvalContext {
                    font_size_px: live_style.text.font_size_px,
                    ..rix_css::EvalContext::default()
                };
                if let Some(fx) = live_style.effects.as_deref() {
                    for sh in &fx.box_shadow {
                        if !sh.inset {
                            has_shadow = true;
                            let ox = sh.offset_x.resolve_px(0.0, &eval);
                            let oy = sh.offset_y.resolve_px(0.0, &eval);
                            let blur = sh.blur.resolve_px(0.0, &eval);
                            let spread = sh.spread.resolve_px(0.0, &eval);
                            let pad = blur + spread;
                            let sr = Rect::new(
                                vp_rect.origin.x + ox - pad,
                                vp_rect.origin.y + oy - pad,
                                (vp_rect.size.width + pad * 2.0).max(0.0),
                                (vp_rect.size.height + pad * 2.0).max(0.0),
                            );
                            occ_rect = union_rect(occ_rect, sr);
                        }
                    }
                    for op in &fx.filter {
                        match op {
                            FilterOp::Blur(r) => {
                                let rad = r.resolve_px(0.0, &eval).max(0.0) * 3.0;
                                if rad > 0.0 {
                                    has_shadow = true;
                                    let fr = Rect::new(
                                        vp_rect.origin.x - rad,
                                        vp_rect.origin.y - rad,
                                        vp_rect.size.width + rad * 2.0,
                                        vp_rect.size.height + rad * 2.0,
                                    );
                                    occ_rect = union_rect(occ_rect, fr);
                                }
                            }
                            FilterOp::DropShadow {
                                offset_x,
                                offset_y,
                                blur,
                                ..
                            } => {
                                has_shadow = true;
                                let ox = offset_x.resolve_px(0.0, &eval);
                                let oy = offset_y.resolve_px(0.0, &eval);
                                let rad = blur.resolve_px(0.0, &eval).max(0.0) * 3.0;
                                let dr = Rect::new(
                                    vp_rect.origin.x + ox - rad,
                                    vp_rect.origin.y + oy - rad,
                                    vp_rect.size.width + rad * 2.0,
                                    vp_rect.size.height + rad * 2.0,
                                );
                                occ_rect = union_rect(occ_rect, dr);
                            }
                            _ => {}
                        }
                    }
                }
                let paints_something = live_style.visual.background_color.a > 0
                    || !live_style.visual.background_layers.is_empty()
                    || frag.border.horizontal() > 0.0
                    || frag.border.vertical() > 0.0
                    || has_shadow
                    || matches!(
                        frag.kind,
                        rix_layout::FragmentKind::TextRun(_)
                            | rix_layout::FragmentKind::Image { .. }
                            | rix_layout::FragmentKind::Svg { .. }
                            | rix_layout::FragmentKind::FormControl { .. }
                    );
                if paints_something && occ_rect.size.width > 0.0 && occ_rect.size.height > 0.0 {
                    let (_, hit) = intersect_rects_strict(occ_rect, st.inter);
                    if hit {
                        st.occluded = true;
                        return;
                    }
                }
            }
        }
        // Skip descending into opacity:0 subtrees once we've already passed target.
        if st.seen_target && eff_opacity <= 0.0 {
            return;
        }
        let child_scroll = if let Some(nid) = frag.dom_node {
            if let Some(&s) = scroll_map.get(&nid) {
                Point::new(eff_scroll.x + s.x, eff_scroll.y + s.y)
            } else {
                eff_scroll
            }
        } else {
            eff_scroll
        };
        for ch in frag.children.iter() {
            walk_occ(
                st,
                ch,
                abs.origin,
                child_scroll,
                scroll_map,
                eff_opacity,
                false,
            );
        }
    }

    let mut walk = OcclusionWalk {
        doc: &doc,
        styles,
        target,
        inter: intersection_rect,
        seen_target: false,
        occluded: false,
    };
    walk_occ(
        &mut walk,
        root_frag,
        Point::ZERO,
        root_scroll,
        &scroll_map,
        1.0,
        true,
    );
    !walk.occluded
}

fn compute_intersection_snapshot(
    host: &SharedHost,
    io: &IntersectionObserverEntry,
    target: NodeId,
    timestamp_ms: f64,
) -> IntersectionSnapshot {
    let main_root = host.borrow().document.borrow().root();
    let target_owner = host.borrow().owner_doc_of(target);
    let root_info = root_intersection_rect(host, io.root, io.root_margin);

    // Check same-origin iframe case when observer root is implicit (top-level viewport)
    // and target lives inside a same-origin iframe.
    let in_same_origin_iframe = io.root.is_none() && target_owner != main_root;

    let Some((root_bounds, is_scroll_root)) = root_info else {
        return IntersectionSnapshot {
            target,
            bounding_rect: Rect::ZERO,
            root_bounds: None,
            intersection_rect: Rect::ZERO,
            intersection_ratio: 0.0,
            is_intersecting: false,
            is_visible: false,
            time_ms: timestamp_ms,
        };
    };

    let cb_root = if in_same_origin_iframe { None } else { io.root };
    let Some(clippers) = containing_block_chain_to_root(host, target, cb_root) else {
        // W3C IO §2.3: inactive / display:none / detached / cross-document explicit root ->
        // all three rects are empty (0,0,0,0).
        return IntersectionSnapshot {
            target,
            bounding_rect: Rect::ZERO,
            root_bounds: Some(Rect::ZERO),
            intersection_rect: Rect::ZERO,
            intersection_ratio: 0.0,
            is_intersecting: false,
            is_visible: false,
            time_ms: timestamp_ms,
        };
    };

    let mut bounding_rect = transformed_bounding_rect_for_node(host, target);
    let stroke_pad = svg_stroke_expansion(host, target);
    if stroke_pad > 0.0 {
        bounding_rect = Rect::new(
            bounding_rect.origin.x - stroke_pad,
            bounding_rect.origin.y - stroke_pad,
            bounding_rect.size.width + stroke_pad * 2.0,
            bounding_rect.size.height + stroke_pad * 2.0,
        );
    }

    // Start intersection rect at target's boundingClientRect.
    let mut cur_rect = bounding_rect;
    let mut intersecting = true;

    // Clip SVG child targets against their enclosing `<svg>` viewport when it clips overflow.
    if let Some(svg_clip) = svg_outer_root_clip_rect(host, target) {
        let (clipped, hit) = intersect_rects(cur_rect, svg_clip);
        if !hit {
            intersecting = false;
            cur_rect = Rect::ZERO;
        } else {
            cur_rect = clipped;
        }
    }

    // Clip against non-scroll ancestor `clip-path: inset(...)` boxes.
    if intersecting {
        for anc in clippers.clip_path_ancestors {
            let info_opt = host.borrow_mut().element_geometry_for_node(anc);
            if let Some(info) = info_opt {
                let border_box = Rect::new(
                    info.abs_border.origin.x - info.ancestor_scroll.x,
                    info.abs_border.origin.y - info.ancestor_scroll.y,
                    info.abs_border.size.width,
                    info.abs_border.size.height,
                );
                let cp = clip_rect_by_clip_path_inset(host, anc, border_box);
                if cp.size.width < 0.0 || cp.size.height < 0.0 {
                    intersecting = false;
                    cur_rect = Rect::ZERO;
                    break;
                }
                let (clipped, hit) = intersect_rects(cur_rect, cp);
                if !hit {
                    intersecting = false;
                    cur_rect = Rect::ZERO;
                    break;
                }
                cur_rect = clipped;
            }
        }
    }

    // Clip against intermediate scroll containers (clip-path applied before scrollMargin).
    if intersecting {
        for sc in clippers.scroll_clippers {
            let info_opt = host.borrow_mut().element_geometry_for_node(sc);
            if let Some(info) = info_opt {
                let raw_clip = Rect::new(
                    info.abs_padding.origin.x - info.ancestor_scroll.x,
                    info.abs_padding.origin.y - info.ancestor_scroll.y,
                    info.abs_padding.size.width,
                    info.abs_padding.size.height,
                );
                let clip_box = clip_rect_by_clip_path_inset(host, sc, raw_clip);
                if clip_box.size.width < 0.0 || clip_box.size.height < 0.0 {
                    intersecting = false;
                    cur_rect = Rect::ZERO;
                    break;
                }
                let expanded = io.scroll_margin.expand_rect(clip_box);
                let (clipped, hit) = intersect_rects(cur_rect, expanded);
                if !hit {
                    intersecting = false;
                    cur_rect = Rect::ZERO;
                    break;
                }
                cur_rect = clipped;
            }
        }
    }

    if intersecting {
        if in_same_origin_iframe {
            // 1. Clip against iframe's own viewport (expanded by scrollMargin).
            let iframe_vp = host.borrow_mut().viewport_for_doc(target_owner);
            let iframe_vp_rect = io.scroll_margin.expand_rect(Rect::new(
                0.0,
                0.0,
                iframe_vp.width,
                iframe_vp.height,
            ));
            let (clipped_local, hit_local) = intersect_rects(cur_rect, iframe_vp_rect);
            if !hit_local {
                intersecting = false;
                cur_rect = Rect::ZERO;
            } else if let Some(iframe_el) = owner_iframe_of_doc(host, target_owner) {
                // 2. Project into parent viewport using iframe's full 4x4 CSS transform matrix.
                let iframe_boxes = collect_boxes_for_node(host, iframe_el);
                if let Some(iframe_box) = iframe_boxes.first() {
                    let pad_x = f64::from(iframe_box.border.left + iframe_box.padding.left);
                    let pad_y = f64::from(iframe_box.border.top + iframe_box.padding.top);
                    let lx0 = pad_x + f64::from(clipped_local.origin.x);
                    let ly0 = pad_y + f64::from(clipped_local.origin.y);
                    let lx1 = lx0 + f64::from(clipped_local.size.width);
                    let ly1 = ly0 + f64::from(clipped_local.size.height);
                    let p0 = mat4_project_2d(&iframe_box.to_viewport, lx0, ly0);
                    let p1 = mat4_project_2d(&iframe_box.to_viewport, lx1, ly0);
                    let p2 = mat4_project_2d(&iframe_box.to_viewport, lx1, ly1);
                    let p3 = mat4_project_2d(&iframe_box.to_viewport, lx0, ly1);
                    // Detect degenerate / edge-on 3D rotation (e.g., rotateX(90deg)).
                    let quad_area2 =
                        ((p1.0 - p0.0) * (p2.1 - p0.1) - (p1.1 - p0.1) * (p2.0 - p0.0)).abs()
                            + ((p2.0 - p0.0) * (p3.1 - p0.1) - (p2.1 - p0.1) * (p3.0 - p0.0)).abs();
                    let local_area =
                        f64::from(clipped_local.size.width) * f64::from(clipped_local.size.height);
                    if local_area > 0.0 && quad_area2 < 1e-3 {
                        intersecting = false;
                        cur_rect = Rect::ZERO;
                    } else {
                        let min_x = p0.0.min(p1.0).min(p2.0).min(p3.0) as f32;
                        let min_y = p0.1.min(p1.1).min(p2.1).min(p3.1) as f32;
                        let max_x = p0.0.max(p1.0).max(p2.0).max(p3.0) as f32;
                        let max_y = p0.1.max(p1.1).max(p2.1).max(p3.1) as f32;
                        let mut parent_rect = Rect::new(
                            min_x,
                            min_y,
                            (max_x - min_x).max(0.0),
                            (max_y - min_y).max(0.0),
                        );
                        // Clip against parent's scroll containers above the iframe.
                        if let Some(parent_clippers) =
                            containing_block_chain_to_root(host, iframe_el, None)
                        {
                            for sc in parent_clippers.scroll_clippers {
                                let info_opt = host.borrow_mut().element_geometry_for_node(sc);
                                if let Some(info) = info_opt {
                                    let clip_box = Rect::new(
                                        info.abs_padding.origin.x - info.ancestor_scroll.x,
                                        info.abs_padding.origin.y - info.ancestor_scroll.y,
                                        info.abs_padding.size.width,
                                        info.abs_padding.size.height,
                                    );
                                    let expanded = io.scroll_margin.expand_rect(clip_box);
                                    let (c, hit) = intersect_rects(parent_rect, expanded);
                                    if !hit {
                                        intersecting = false;
                                        parent_rect = Rect::ZERO;
                                        break;
                                    }
                                    parent_rect = c;
                                }
                            }
                        } else {
                            intersecting = false;
                        }
                        if intersecting {
                            let clip_root = io.scroll_margin.expand_rect(root_bounds);
                            let (final_parent, hit_root) = intersect_rects(parent_rect, clip_root);
                            if hit_root {
                                // Express intersectionRect back in target's iframe viewport space.
                                let ox = p0.0 as f32 - clipped_local.origin.x;
                                let oy = p0.1 as f32 - clipped_local.origin.y;
                                cur_rect = Rect::new(
                                    final_parent.origin.x - ox,
                                    final_parent.origin.y - oy,
                                    final_parent.size.width,
                                    final_parent.size.height,
                                );
                            } else {
                                intersecting = false;
                                cur_rect = Rect::ZERO;
                            }
                        } else {
                            cur_rect = Rect::ZERO;
                        }
                    }
                } else {
                    intersecting = false;
                    cur_rect = Rect::ZERO;
                }
            } else {
                intersecting = false;
                cur_rect = Rect::ZERO;
            }
        } else {
            let clip_root = if is_scroll_root {
                io.scroll_margin.expand_rect(root_bounds)
            } else {
                root_bounds
            };
            let (clipped, hit) = intersect_rects(cur_rect, clip_root);
            intersecting = hit;
            cur_rect = clipped;
        }
    }

    let target_area = f64::from(bounding_rect.size.width) * f64::from(bounding_rect.size.height);
    let inter_area = f64::from(cur_rect.size.width) * f64::from(cur_rect.size.height);
    let ratio = if !intersecting {
        0.0
    } else if target_area <= 0.0 {
        1.0
    } else {
        (inter_area / target_area).clamp(0.0, 1.0)
    };

    // W3C IntersectionObserver §2.3 step 2.2.15 (issue #432):
    // `isIntersecting` is true iff the target meets at least the minimum threshold.
    let bucket = threshold_bucket(&io.thresholds, ratio, intersecting);
    let final_is_intersecting = bucket > 0;

    let is_visible = if io.track_visibility && final_is_intersecting {
        check_target_visibility_v2(host, target, cur_rect)
    } else {
        false
    };

    IntersectionSnapshot {
        target,
        bounding_rect,
        root_bounds: Some(root_bounds),
        intersection_rect: cur_rect,
        intersection_ratio: ratio,
        is_intersecting: final_is_intersecting,
        is_visible,
        time_ms: timestamp_ms,
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
    // 1. W3C ResizeObserver §4.2 depth-loop delivery
    deliver_resize_observers(vm, host, observers);

    // 2. W3C IntersectionObserver v1 + v2 delivery
    let main_root = host.borrow().document.borrow().root();
    let (inter_batches, entry_proto) = {
        let mut os = observers.borrow_mut();
        let entry_proto = os.intersection_entry_proto;
        let order = os.intersection_order.clone();
        let mut out = Vec::new();
        for io_h in order {
            let Some(io) = os.intersection_observers.get_mut(&io_h) else {
                continue;
            };
            if io.targets.is_empty() && io.pending_records.is_empty() {
                continue;
            }
            let mut entries = std::mem::take(&mut io.pending_records);
            let targets = io.targets.clone();
            for nid in targets {
                // W3C IO §2.3 step 2.1: skip targets in detached (non-active) documents
                // that have not yet produced an initial observation.
                let target_owner = host.borrow().owner_doc_of(nid);
                if target_owner != main_root
                    && owner_iframe_of_doc(host, target_owner).is_none()
                    && !io.last_state.contains_key(&nid)
                {
                    continue;
                }
                // W3C IO v2 `delay`: first notification fires immediately; subsequent updates
                // are throttled until `timestamp_ms - last_notified >= delay`.
                if io.delay > 0.0 {
                    if let Some(&prev_ms) = io.last_notified_ms.get(&nid) {
                        if timestamp_ms - prev_ms <= io.delay {
                            continue;
                        }
                    }
                }
                let snap = compute_intersection_snapshot(host, io, nid, timestamp_ms);
                let bucket = threshold_bucket(
                    &io.thresholds,
                    snap.intersection_ratio,
                    snap.is_intersecting,
                );
                let prev = io.last_state.get(&nid).copied();
                let next_state = (snap.is_intersecting, bucket, snap.is_visible);
                if prev != Some(next_state) {
                    io.last_state.insert(nid, next_state);
                    io.last_notified_ms.insert(nid, timestamp_ms);
                    entries.push(snap);
                }
            }
            if !entries.is_empty() {
                out.push((io.observer_obj, io.callback, entries));
            }
        }
        (out, entry_proto)
    };

    for (obs_obj, cb, entries) in inter_batches {
        let arr = vm.alloc_array();
        for snap in entries {
            let entry = build_intersection_entry(vm, host, snap, entry_proto);
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

/// Edge-inclusive rectangle intersection per W3C IntersectionObserver §2.3.
fn intersect_rects(a: Rect, b: Rect) -> (Rect, bool) {
    let x1 = a.origin.x.max(b.origin.x);
    let y1 = a.origin.y.max(b.origin.y);
    let x2 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y2 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if x2 >= x1 && y2 >= y1 {
        (
            Rect::new(x1, y1, (x2 - x1).max(0.0), (y2 - y1).max(0.0)),
            true,
        )
    } else {
        (Rect::ZERO, false)
    }
}

/// Strict positive-area overlap check for IOv2 paint-order occlusion.
fn intersect_rects_strict(a: Rect, b: Rect) -> (Rect, bool) {
    let x1 = a.origin.x.max(b.origin.x);
    let y1 = a.origin.y.max(b.origin.y);
    let x2 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y2 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if x2 > x1 && y2 > y1 {
        (Rect::new(x1, y1, x2 - x1, y2 - y1), true)
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

// ---------------------------------------------------------------------------
// Entry Builders
// ---------------------------------------------------------------------------

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

fn alloc_readonly_dom_rect(vm: &mut JsVm, host: &SharedHost, r: Rect) -> HeapHandle {
    let proto = host
        .borrow()
        .cssom_geometry_state
        .borrow()
        .dom_rect_readonly_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let x = f64::from(r.origin.x);
    let y = f64::from(r.origin.y);
    let w = f64::from(r.size.width);
    let h = f64::from(r.size.height);
    for (name, val) in [
        ("x", x),
        ("y", y),
        ("width", w),
        ("height", h),
        ("left", x.min(x + w)),
        ("top", y.min(y + h)),
        ("right", x.max(x + w)),
        ("bottom", y.max(y + h)),
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

fn alloc_box_size_list(
    vm: &mut JsVm,
    sizes: &[(f64, f64)],
    size_proto: Option<HeapHandle>,
) -> HeapHandle {
    let arr = vm.alloc_array();
    let proto = size_proto.unwrap_or(vm.object_proto);
    for &(inline_s, block_s) in sizes {
        let item = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let ki = vm.heap.intern_key("inlineSize");
        let kb = vm.heap.intern_key("blockSize");
        vm.heap.object_define_data(
            item,
            ki,
            JsValue::from_f64(inline_s),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            item,
            kb,
            JsValue::from_f64(block_s),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_push_element(arr, JsValue::from_object(item));
    }
    arr
}

fn build_resize_entry(
    vm: &mut JsVm,
    host: &SharedHost,
    nid: NodeId,
    measured: &MeasuredResizeBox,
    entry_proto: Option<HeapHandle>,
    size_proto: Option<HeapHandle>,
) -> HeapHandle {
    let proto = entry_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let tgt = wrap_node(vm, host, nid);
    let cr = alloc_readonly_dom_rect(vm, host, measured.content_rect);
    let bbs = alloc_box_size_list(vm, &measured.border_sizes, size_proto);
    let cbs = alloc_box_size_list(vm, &measured.content_sizes, size_proto);
    let dpbs = alloc_box_size_list(vm, &measured.device_pixel_sizes, size_proto);
    for (name, val) in [
        ("target", tgt),
        ("contentRect", JsValue::from_object(cr)),
        ("borderBoxSize", JsValue::from_object(bbs)),
        ("contentBoxSize", JsValue::from_object(cbs)),
        ("devicePixelContentBoxSize", JsValue::from_object(dpbs)),
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
    entry_proto: Option<HeapHandle>,
) -> HeapHandle {
    let proto = entry_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let tgt = wrap_node(vm, host, snap.target);
    let br = alloc_readonly_dom_rect(vm, host, snap.bounding_rect);
    let rb = snap.root_bounds.map_or(JsValue::NULL, |r| {
        JsValue::from_object(alloc_readonly_dom_rect(vm, host, r))
    });
    let ir = alloc_readonly_dom_rect(vm, host, snap.intersection_rect);
    for (name, val) in [
        ("target", tgt),
        ("boundingClientRect", JsValue::from_object(br)),
        ("rootBounds", rb),
        ("intersectionRect", JsValue::from_object(ir)),
        (
            "intersectionRatio",
            JsValue::from_f64(snap.intersection_ratio),
        ),
        ("isIntersecting", JsValue::from_bool(snap.is_intersecting)),
        ("isVisible", JsValue::from_bool(snap.is_visible)),
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

fn install_illegal_interface_ctor(vm: &mut JsVm, name: &str) -> HeapHandle {
    let proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(proto, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    let ctor = vm.alloc_host_fn(name, |vm, _, _| Err(vm.type_error("Illegal constructor")));
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    let k_c = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        proto,
        k_c,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, name, JsValue::from_object(ctor));
    proto
}

fn install_resize_observer(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let entry_proto = install_illegal_interface_ctor(vm, "ResizeObserverEntry");
    let size_proto = install_illegal_interface_ctor(vm, "ResizeObserverSize");
    {
        let mut os = observers.borrow_mut();
        os.resize_entry_proto = Some(entry_proto);
        os.resize_size_proto = Some(size_proto);
    }

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
        if !matches!(
            h.borrow().document.borrow().get(target),
            Some(NodeData::Element(_))
        ) {
            return Err(vm.type_error("ResizeObserver.observe target must be an Element"));
        }
        let mut box_opt = ResizeBoxOption::Content;
        let opts = arg(args, 1);
        if !opts.is_undefined() && !opts.is_null() {
            let Some(oh) = opts.as_object() else {
                return Err(vm.type_error("ResizeObserver.observe options must be an object"));
            };
            let kb = vm.heap.intern_key("box");
            let bv = vm.get_property_by_key(oh, kb)?;
            if !bv.is_undefined() {
                let s = vm.value_to_rust_string(bv);
                box_opt = match s.as_str() {
                    "content-box" => ResizeBoxOption::Content,
                    "border-box" => ResizeBoxOption::Border,
                    "device-pixel-content-box" => ResizeBoxOption::DevicePixelContent,
                    _ => {
                        return Err(
                            vm.type_error(format!("Invalid ResizeObserverBoxOptions value '{s}'"))
                        );
                    }
                };
            }
        }
        if let Some(ro) = obs.borrow_mut().resize_observers.get_mut(&ro_h) {
            // W3C ResizeObserver §2.1: re-observing removes previous observation and adds new one
            // with the updated `box` option (triggering fresh observation with new box).
            ro.observations.retain(|o| o.target != target);
            ro.observations.push(ResizeTargetObservation {
                target,
                box_opt,
                last_reported: None,
            });
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
                ro.observations.retain(|o| o.target != target);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "disconnect", move |_vm, this_val, _| {
        if let Some(ro_h) = this_val.as_object() {
            if let Some(ro) = obs.borrow_mut().resize_observers.get_mut(&ro_h) {
                ro.observations.clear();
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
        let mut os = obs.borrow_mut();
        os.resize_order.push(obj);
        os.resize_observers.insert(
            obj,
            ResizeObserverEntry {
                observer_obj: obj,
                callback: cb,
                observations: Vec::new(),
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
    let k_c = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        proto,
        k_c,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "ResizeObserver", JsValue::from_object(ctor));
}

fn install_intersection_observer(vm: &mut JsVm, host: &SharedHost, observers: &SharedObservers) {
    let entry_proto = install_illegal_interface_ctor(vm, "IntersectionObserverEntry");
    observers.borrow_mut().intersection_entry_proto = Some(entry_proto);

    let proto = vm.heap.alloc_empty_object();
    vm.heap
        .object_set_prototype(proto, Some(vm.object_proto), &mut vm.shapes);
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;

    // Read-only IDL attributes: root, rootMargin, scrollMargin, thresholds, delay, trackVisibility
    let h_r = Rc::clone(host);
    let o_r = Rc::clone(observers);
    define_host_getter(vm, proto, "root", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let root = o_r
            .borrow()
            .intersection_observers
            .get(&io_h)
            .map(|io| io.root)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(root.map_or(JsValue::NULL, |r| wrap_node(vm, &h_r, r)))
    });

    let o_rm = Rc::clone(observers);
    define_host_getter(vm, proto, "rootMargin", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let s = o_rm
            .borrow()
            .intersection_observers
            .get(&io_h)
            .map(|io| io.root_margin.to_css_string())
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let o_sm = Rc::clone(observers);
    define_host_getter(vm, proto, "scrollMargin", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let s = o_sm
            .borrow()
            .intersection_observers
            .get(&io_h)
            .map(|io| io.scroll_margin.to_css_string())
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(&s)))
    });

    let o_th = Rc::clone(observers);
    define_host_getter(vm, proto, "thresholds", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let (cached, ths) = {
            let os = o_th.borrow();
            let Some(io) = os.intersection_observers.get(&io_h) else {
                return Err(vm.type_error("Illegal invocation"));
            };
            (io.thresholds_arr, io.thresholds.clone())
        };
        if let Some(arr) = cached {
            return Ok(JsValue::from_object(arr));
        }
        let arr = vm.alloc_array();
        for n in ths {
            vm.heap.object_push_element(arr, JsValue::from_f64(n));
        }
        // W3C IntersectionObserver: `thresholds` is a WebIDL `FrozenArray<double>`.
        let val = JsValue::from_object(arr);
        let k_obj = vm.heap.intern_key("Object");
        let k_freeze = vm.heap.intern_key("freeze");
        if let Ok(obj_ctor) = vm.get_property_by_key(vm.global, k_obj) {
            if let Some(oh) = obj_ctor.as_object() {
                if let Ok(fr) = vm.get_property_by_key(oh, k_freeze) {
                    let _ = vm.call_value(fr, obj_ctor, &[val]);
                }
            }
        }
        if let Some(io) = o_th.borrow_mut().intersection_observers.get_mut(&io_h) {
            io.thresholds_arr = Some(arr);
        }
        Ok(val)
    });

    let o_dl = Rc::clone(observers);
    define_host_getter(vm, proto, "delay", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let d = o_dl
            .borrow()
            .intersection_observers
            .get(&io_h)
            .map(|io| io.delay)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_f64(d))
    });

    let o_tv = Rc::clone(observers);
    define_host_getter(vm, proto, "trackVisibility", move |vm, t, _| {
        let Some(io_h) = t.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let tv = o_tv
            .borrow()
            .intersection_observers
            .get(&io_h)
            .map(|io| io.track_visibility)
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_bool(tv))
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "observe", move |vm, this_val, args| {
        let Some(io_h) = this_val.as_object() else {
            return Err(vm.type_error("IntersectionObserver.observe requires observer receiver"));
        };
        let Some(target) = h.borrow().node_for_value(vm, arg(args, 0)) else {
            return Err(vm.type_error("IntersectionObserver.observe target must be an Element"));
        };
        let is_valid = matches!(
            h.borrow().document.borrow().get(target),
            Some(NodeData::Element(_) | NodeData::Text(_))
        );
        if !is_valid {
            return Err(vm.type_error("IntersectionObserver.observe target must be an Element"));
        }
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
                io.last_notified_ms.remove(&target);
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
                io.last_notified_ms.clear();
                io.pending_records.clear();
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let obs = Rc::clone(observers);
    define_host_method(vm, proto, "takeRecords", move |vm, this_val, _| {
        let (snaps, entry_proto) = observers_take(&obs, this_val);
        let arr = vm.alloc_array();
        for s in snaps {
            let entry = build_intersection_entry(vm, &h, s, entry_proto);
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
        let mut root_margin = MarginBox::default();
        let mut scroll_margin = MarginBox::default();
        let mut thresholds = vec![0.0];
        let mut delay = 0.0f64;
        let mut track_visibility = false;

        if !opts.is_undefined() && !opts.is_null() {
            let Some(oh) = opts.as_object() else {
                return Err(vm.type_error("IntersectionObserver options must be an object"));
            };
            let kr = vm.heap.intern_key("root");
            let rv = vm.get_property_by_key(oh, kr)?;
            if !rv.is_undefined() && !rv.is_null() {
                let Some(nid) = h.borrow().node_for_value(vm, rv) else {
                    return Err(
                        vm.type_error("IntersectionObserver root must be an Element or Document")
                    );
                };
                if !matches!(
                    h.borrow().document.borrow().get(nid),
                    Some(NodeData::Element(_) | NodeData::Document)
                ) {
                    return Err(
                        vm.type_error("IntersectionObserver root must be an Element or Document")
                    );
                }
                root = Some(nid);
            }

            let krm = vm.heap.intern_key("rootMargin");
            let rmv = vm.get_property_by_key(oh, krm)?;
            if !rmv.is_undefined() {
                let s = vm.value_to_rust_string(rmv);
                root_margin =
                    parse_observer_margin(&s).map_err(|e| throw_syntax_dom_exception(vm, &e))?;
            }

            let ksm = vm.heap.intern_key("scrollMargin");
            let smv = vm.get_property_by_key(oh, ksm)?;
            if !smv.is_undefined() {
                let s = vm.value_to_rust_string(smv);
                scroll_margin =
                    parse_observer_margin(&s).map_err(|e| throw_syntax_dom_exception(vm, &e))?;
            }

            let kt = vm.heap.intern_key("threshold");
            let tv = vm.get_property_by_key(oh, kt)?;
            if !tv.is_undefined() {
                let mut raw_nums = Vec::new();
                if tv.is_object() {
                    let items = vm.collect_iterable(tv)?;
                    for it in items {
                        let n = vm.to_number(it)?;
                        raw_nums.push(n);
                    }
                } else {
                    let n = vm.to_number(tv)?;
                    raw_nums.push(n);
                }
                let mut parsed = Vec::with_capacity(raw_nums.len());
                for n in raw_nums {
                    if !n.is_finite() {
                        return Err(
                            vm.type_error("IntersectionObserver threshold must be a finite number")
                        );
                    }
                    if !(0.0..=1.0).contains(&n) {
                        return Err(
                            vm.range_error("IntersectionObserver thresholds must be in [0.0, 1.0]")
                        );
                    }
                    parsed.push(n);
                }
                if parsed.is_empty() {
                    parsed.push(0.0);
                } else {
                    parsed.sort_by(f64::total_cmp);
                }
                thresholds = parsed;
            }

            let ktv = vm.heap.intern_key("trackVisibility");
            let tv_v = vm.get_property_by_key(oh, ktv)?;
            if !tv_v.is_undefined() {
                track_visibility = vm.to_boolean(tv_v);
            }

            let kdl = vm.heap.intern_key("delay");
            let dl_v = vm.get_property_by_key(oh, kdl)?;
            if !dl_v.is_undefined() {
                let n = vm.to_number(dl_v)?;
                if n.is_finite() && n > 0.0 {
                    delay = n;
                }
            }
            if track_visibility && delay < 100.0 {
                return Err(throw_syntax_dom_exception(
                    vm,
                    "IntersectionObserver with trackVisibility requires delay >= 100",
                ));
            }
        }

        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let mut os = obs.borrow_mut();
        os.intersection_order.push(obj);
        os.intersection_observers.insert(
            obj,
            IntersectionObserverEntry {
                observer_obj: obj,
                callback: cb,
                root,
                root_margin,
                scroll_margin,
                thresholds,
                thresholds_arr: None,
                delay,
                track_visibility,
                targets: Vec::new(),
                last_state: HashMap::new(),
                last_notified_ms: HashMap::new(),
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
    let k_c = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        proto,
        k_c,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "IntersectionObserver", JsValue::from_object(ctor));
}

fn observers_take(
    obs: &SharedObservers,
    this_val: JsValue,
) -> (Vec<IntersectionSnapshot>, Option<HeapHandle>) {
    let mut os = obs.borrow_mut();
    let ep = os.intersection_entry_proto;
    let snaps = this_val
        .as_object()
        .and_then(|io_h| {
            os.intersection_observers
                .get_mut(&io_h)
                .map(|io| std::mem::take(&mut io.pending_records))
        })
        .unwrap_or_default();
    (snaps, ep)
}
