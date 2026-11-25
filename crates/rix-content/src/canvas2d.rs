//! JavaScript bindings for `HTMLCanvasElement`, `CanvasRenderingContext2D`,
//! `CanvasGradient`, `CanvasPattern`, and `ImageData` backed by GPU-resident
//! `vello::Scene` / `ExternalSurface` textures (zero RGBA pixel arrays stored
//! in DOM nodes, zero DisplayList rebuilds and zero DOM mutations per frame!).
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #18.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::{NodeData, NodeId};
use rix_ipc::SurfaceId;
use rix_js::heap::{TypedArrayKind, TypedArrayObject};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::surface_id_for_node;
use rix_render::{
    encode_rgba_to_png, encode_rgba_to_png_data_url, parse_css_color, Canvas2DContext,
    CanvasFillRule, CanvasGradient, CanvasPaint, CanvasPattern, CanvasPatternRepeat, Compositor,
    VelloRenderer,
};

use crate::dom_bridge::{set_element_attr, wrap_node, SharedHost};
use crate::web_apis::{
    alloc_array_buffer, arg, bind_ctor_proto, create_blob_object, create_dom_exception,
    define_global, define_host_accessor, define_host_getter, define_host_method, SharedWebApis,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct CanvasPrototypes {
    pub(crate) ctx2d_proto: Option<HeapHandle>,
    pub(crate) gradient_proto: Option<HeapHandle>,
    pub(crate) pattern_proto: Option<HeapHandle>,
    pub(crate) image_data_proto: Option<HeapHandle>,
    pub(crate) image_bitmap_proto: Option<HeapHandle>,
    pub(crate) offscreen_canvas_proto: Option<HeapHandle>,
}

#[derive(Debug, Clone)]
pub(crate) struct ImageBitmapEntry {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
    pub(crate) closed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OffscreenCanvasEntry {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
    pub(crate) detached: bool,
}

/// Hook invoked by `HTMLCanvasElement.prototype.getContext(kind, options)` for
/// non-2D contexts such as `"webgpu"`, `"webgl2"`, and `"webgl"`.
pub type CanvasContextHook =
    Rc<dyn Fn(&mut JsVm, NodeId, &str, JsValue) -> Result<Option<JsValue>, JsException>>;
/// Hook notified whenever `canvas.width` or `canvas.height` is reassigned.
pub type CanvasResizeHook = Rc<dyn Fn(NodeId, u32, u32)>;
/// Hook queried by `canvas.toDataURL()` / `canvas.toBlob()` for GPU-backed non-2D canvases.
pub type CanvasReadbackHook = Rc<dyn Fn(NodeId) -> Option<(u32, u32, Vec<u8>)>>;

/// Shared Canvas2D registry owned by [`crate::ContentRuntime`].
/// Keeps all GPU vector scenes completely outside `rix-dom` nodes.
pub struct Canvas2DSystem {
    pub(crate) contexts: HashMap<NodeId, Canvas2DContext>,
    ctx_handle_to_node: HashMap<HeapHandle, NodeId>,
    node_to_ctx_handle: HashMap<NodeId, HeapHandle>,
    gradients: HashMap<HeapHandle, CanvasGradient>,
    patterns: HashMap<HeapHandle, CanvasPattern>,
    pub(crate) image_data: HashMap<HeapHandle, (u32, u32, HeapHandle)>,
    pub(crate) image_bitmaps: HashMap<HeapHandle, ImageBitmapEntry>,
    pub(crate) offscreen_canvases: HashMap<HeapHandle, OffscreenCanvasEntry>,
    readback_renderer: Option<VelloRenderer>,
    pub(crate) protos: CanvasPrototypes,
    context_hooks: Vec<CanvasContextHook>,
    resize_hooks: Vec<CanvasResizeHook>,
    readback_hooks: Vec<CanvasReadbackHook>,
}

/// Shared handle to [`Canvas2DSystem`].
pub type SharedCanvas2D = Rc<RefCell<Canvas2DSystem>>;

impl Default for Canvas2DSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas2DSystem {
    #[must_use]
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            ctx_handle_to_node: HashMap::new(),
            node_to_ctx_handle: HashMap::new(),
            gradients: HashMap::new(),
            patterns: HashMap::new(),
            image_data: HashMap::new(),
            image_bitmaps: HashMap::new(),
            offscreen_canvases: HashMap::new(),
            readback_renderer: None,
            protos: CanvasPrototypes::default(),
            context_hooks: Vec::new(),
            resize_hooks: Vec::new(),
            readback_hooks: Vec::new(),
        }
    }

    /// Registers a `getContext(kind, options)` hook for GPU contexts (`webgpu`, `webgl2`).
    pub fn register_context_hook(&mut self, hook: CanvasContextHook) {
        self.context_hooks.push(hook);
    }

    /// Registers a resize listener notified when `canvas.width` / `canvas.height` change.
    pub fn register_resize_hook(&mut self, hook: CanvasResizeHook) {
        self.resize_hooks.push(hook);
    }

    /// Registers an RGBA readback hook used by `toDataURL()` / `toBlob()` for GPU canvases.
    pub fn register_readback_hook(&mut self, hook: CanvasReadbackHook) {
        self.readback_hooks.push(hook);
    }

    /// Returns a reference to the backing [`Canvas2DContext`] for a `<canvas>` DOM node.
    #[must_use]
    pub fn context_for_node(&self, node: NodeId) -> Option<&Canvas2DContext> {
        self.contexts.get(&node)
    }

    /// Returns a mutable reference to the backing [`Canvas2DContext`] for a `<canvas>` DOM node.
    pub fn context_for_node_mut(&mut self, node: NodeId) -> Option<&mut Canvas2DContext> {
        self.contexts.get_mut(&node)
    }

    /// Presents all dirty `Canvas2DContext` surfaces to `compositor` (and optionally renders
    /// into persistent GPU off-screen `wgpu::Texture`s via `gpu`) with ZERO DisplayList rebuilds.
    pub fn present_dirty_to_compositor(
        &mut self,
        compositor: &mut Compositor,
        mut gpu: Option<&mut VelloRenderer>,
    ) -> usize {
        let mut presented = 0usize;
        for ctx in self.contexts.values_mut() {
            if !ctx.is_dirty() {
                continue;
            }
            if let Some(ref mut renderer) = gpu {
                let _ = renderer.render_canvas_to_gpu_surface_blocking(ctx);
            }
            compositor.present_external_surface_scene(
                ctx.surface_id(),
                ctx.width(),
                ctx.height(),
                ctx.snapshot_scene(),
            );
            ctx.clear_dirty();
            presented += 1;
        }
        presented
    }

    /// Performs on-demand GPU readback of `node`'s canvas pixels via Vello (`getImageData` / `toDataURL`).
    pub fn readback_node_rgba(&mut self, node: NodeId) -> Option<(u32, u32, Vec<u8>)> {
        let (sid, w, h, snapshot) = {
            let ctx = self.contexts.get(&node)?;
            (
                ctx.surface_id(),
                ctx.width(),
                ctx.height(),
                ctx.snapshot_scene(),
            )
        };
        let renderer = self
            .readback_renderer
            .get_or_insert_with(VelloRenderer::new);
        if let Some(ctx) = self.contexts.get(&node) {
            if renderer.render_canvas_to_gpu_surface_blocking(ctx).is_ok() {
                if let Ok(res) = renderer.readback_gpu_surface_rgba_blocking(sid) {
                    return Some(res);
                }
            }
        }
        renderer
            .render_to_rgba_blocking(&snapshot, w, h, rix_render::Color::TRANSPARENT)
            .ok()
            .map(|px| (w, h, px))
    }

    /// Collects GC roots for Canvas2D prototypes, context wrappers, and ImageData buffers.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.ctx2d_proto,
            self.protos.gradient_proto,
            self.protos.pattern_proto,
            self.protos.image_data_proto,
            self.protos.image_bitmap_proto,
            self.protos.offscreen_canvas_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for &h in self.node_to_ctx_handle.values() {
            out.push(JsValue::from_object(h));
        }
        for &(_, _, data_h) in self.image_data.values() {
            out.push(JsValue::from_object(data_h));
        }
    }

    /// Sweeps dead native entries for GC-collected Canvas2D JS wrappers.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.gradients.retain(|&h, _| heap.is_alive(h));
        self.patterns.retain(|&h, _| heap.is_alive(h));
        self.image_data.retain(|&h, _| heap.is_alive(h));
        self.image_bitmaps.retain(|&h, _| heap.is_alive(h));
        self.offscreen_canvases.retain(|&h, _| heap.is_alive(h));
        self.ctx_handle_to_node.retain(|&h, _| heap.is_alive(h));
        self.node_to_ctx_handle.retain(|_, &mut h| heap.is_alive(h));
    }
}

/// Installs `HTMLCanvasElement`, `CanvasRenderingContext2D`, `CanvasGradient`,
/// `CanvasPattern`, and `ImageData` onto `vm`.
pub fn install_canvas2d(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
) -> SharedCanvas2D {
    let canvas_sys = Rc::new(RefCell::new(Canvas2DSystem::new()));
    install_image_data(vm, host, &canvas_sys);
    install_gradient_and_pattern(vm, &canvas_sys);
    install_context2d_proto(vm, host, &canvas_sys);
    install_html_canvas_element_proto(vm, host, web_apis, &canvas_sys);
    canvas_sys
}

pub(crate) fn canvas_attr_dim(host: &SharedHost, node: NodeId, attr: &str, default: u32) -> u32 {
    let doc = host.borrow().document.borrow().get(node).cloned();
    if let Some(NodeData::Element(el)) = doc {
        if let Some(raw) = el.get_attribute(attr) {
            if let Ok(v) = raw.trim().parse::<u32>() {
                return v;
            }
        }
    }
    default
}

fn ensure_context_for_node(
    host: &SharedHost,
    canvas_sys: &SharedCanvas2D,
    node: NodeId,
) -> SurfaceId {
    let sid = surface_id_for_node(node);
    let mut cs = canvas_sys.borrow_mut();
    cs.contexts.entry(node).or_insert_with(|| {
        let w = canvas_attr_dim(host, node, "width", 300);
        let h = canvas_attr_dim(host, node, "height", 150);
        Canvas2DContext::new(sid, w, h)
    });
    sid
}

fn require_canvas_node(
    vm: &mut JsVm,
    host: &SharedHost,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Incompatible HTMLCanvasElement receiver"));
    };
    let nid = host
        .borrow()
        .node_for_handle(vm, h)
        .ok_or_else(|| vm.type_error("Incompatible HTMLCanvasElement receiver"))?;
    let is_canvas = matches!(
        host.borrow().document.borrow().get(nid),
        Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("canvas")
    );
    if is_canvas {
        Ok(nid)
    } else {
        Err(vm.type_error("Receiver is not an HTMLCanvasElement"))
    }
}

fn require_ctx2d_node(
    vm: &mut JsVm,
    canvas_sys: &SharedCanvas2D,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Incompatible CanvasRenderingContext2D receiver"));
    };
    canvas_sys
        .borrow()
        .ctx_handle_to_node
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible CanvasRenderingContext2D receiver"))
}

fn alloc_uint8_clamped_array(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    let len = bytes.len() as u32;
    let buf = alloc_array_buffer(vm, bytes);
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(TypedArrayObject {
            kind: TypedArrayKind::Uint8Clamped,
            buffer: buf,
            byte_offset: 0,
            length: len,
        }),
        Some(vm.typed_array_protos[TypedArrayKind::Uint8Clamped as usize]),
    ))
}

pub(crate) fn create_image_data_obj(
    vm: &mut JsVm,
    canvas_sys: &SharedCanvas2D,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> HeapHandle {
    let data_arr = alloc_uint8_clamped_array(vm, rgba);
    let proto = canvas_sys
        .borrow()
        .protos
        .image_data_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    for (name, val) in [
        ("width", JsValue::from_i32(width as i32)),
        ("height", JsValue::from_i32(height as i32)),
        ("data", JsValue::from_object(data_arr)),
    ] {
        let k = vm.heap.intern_key(name);
        vm.heap
            .object_define_data(obj, k, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    canvas_sys
        .borrow_mut()
        .image_data
        .insert(obj, (width, height, data_arr));
    obj
}

fn extract_image_data_pixels(
    vm: &JsVm,
    canvas_sys: &SharedCanvas2D,
    val: JsValue,
) -> Option<(u32, u32, Vec<u8>)> {
    let h = val.as_object()?;
    let (w, h_px, data_h) = canvas_sys.borrow().image_data.get(&h).copied()?;
    let ObjectKind::TypedArray(ta) = vm.heap.get_object(data_h).map(|o| &o.kind)? else {
        return None;
    };
    let ta = *ta;
    let ObjectKind::ArrayBuffer(buf) = vm.heap.get_object(ta.buffer).map(|o| &o.kind)? else {
        return None;
    };
    let off = ta.byte_offset as usize;
    let len = ta.length as usize;
    let end = (off + len).min(buf.len());
    Some((w, h_px, buf[off.min(end)..end].to_vec()))
}

pub(crate) fn create_image_bitmap_obj(
    vm: &mut JsVm,
    canvas_sys: &SharedCanvas2D,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> HeapHandle {
    let proto = canvas_sys
        .borrow()
        .protos
        .image_bitmap_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    canvas_sys.borrow_mut().image_bitmaps.insert(
        obj,
        ImageBitmapEntry {
            width,
            height,
            rgba,
            closed: false,
        },
    );
    obj
}

pub(crate) fn create_offscreen_canvas_obj(
    vm: &mut JsVm,
    canvas_sys: &SharedCanvas2D,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> HeapHandle {
    let proto = canvas_sys
        .borrow()
        .protos
        .offscreen_canvas_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    canvas_sys.borrow_mut().offscreen_canvases.insert(
        obj,
        OffscreenCanvasEntry {
            width,
            height,
            rgba,
            detached: false,
        },
    );
    obj
}

fn install_image_data(vm: &mut JsVm, host: &SharedHost, canvas_sys: &SharedCanvas2D) {
    let idp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(idp).unwrap().is_prototype = true;
    crate::dom_interfaces::define_to_string_tag(vm, idp, "ImageData");
    canvas_sys.borrow_mut().protos.image_data_proto = Some(idp);

    let cs = Rc::clone(canvas_sys);
    let ctor = vm.alloc_host_fn("ImageData", move |vm, _, args| {
        let a0 = arg(args, 0);
        if let Some(arr_h) = a0.as_object() {
            if let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(arr_h).map(|o| &o.kind) {
                let ta = *ta;
                let bytes = match vm.heap.get_object(ta.buffer).map(|o| &o.kind) {
                    Some(ObjectKind::ArrayBuffer(b)) => {
                        let off = ta.byte_offset as usize;
                        let end = (off + ta.length as usize).min(b.len());
                        b[off.min(end)..end].to_vec()
                    }
                    _ => Vec::new(),
                };
                let w = vm.to_u32(arg(args, 1))?;
                if w == 0 || !bytes.len().is_multiple_of((w as usize) * 4) {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "Invalid ImageData dimensions",
                    ));
                }
                let h = if args.len() > 2 && !arg(args, 2).is_undefined() {
                    vm.to_u32(arg(args, 2))?
                } else {
                    (bytes.len() / ((w as usize) * 4)) as u32
                };
                if (w as usize) * (h as usize) * 4 != bytes.len() {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "ImageData buffer length does not match width * height * 4",
                    ));
                }
                return Ok(JsValue::from_object(create_image_data_obj(
                    vm, &cs, w, h, bytes,
                )));
            }
        }
        let w = vm.to_u32(a0)?;
        let h = vm.to_u32(arg(args, 1))?;
        if w == 0 || h == 0 {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "ImageData width and height must be positive",
            ));
        }
        let bytes = vec![0u8; (w as usize) * (h as usize) * 4];
        Ok(JsValue::from_object(create_image_data_obj(
            vm, &cs, w, h, bytes,
        )))
    });
    bind_ctor_proto(vm, ctor, idp);
    define_global(vm, "ImageData", JsValue::from_object(ctor));

    // ImageBitmap interface
    let ibp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(ibp).unwrap().is_prototype = true;
    crate::dom_interfaces::define_to_string_tag(vm, ibp, "ImageBitmap");
    canvas_sys.borrow_mut().protos.image_bitmap_proto = Some(ibp);
    {
        let cs = Rc::clone(canvas_sys);
        define_host_getter(vm, ibp, "width", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation on ImageBitmap"));
            };
            let w = cs
                .borrow()
                .image_bitmaps
                .get(&h)
                .map(|b| if b.closed { 0 } else { b.width })
                .ok_or_else(|| vm.type_error("Illegal invocation on ImageBitmap"))?;
            Ok(JsValue::from_f64(f64::from(w)))
        });
    }
    {
        let cs = Rc::clone(canvas_sys);
        define_host_getter(vm, ibp, "height", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation on ImageBitmap"));
            };
            let h_px = cs
                .borrow()
                .image_bitmaps
                .get(&h)
                .map(|b| if b.closed { 0 } else { b.height })
                .ok_or_else(|| vm.type_error("Illegal invocation on ImageBitmap"))?;
            Ok(JsValue::from_f64(f64::from(h_px)))
        });
    }
    {
        let cs = Rc::clone(canvas_sys);
        define_host_method(vm, ibp, "close", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation on ImageBitmap"));
            };
            let mut st = cs.borrow_mut();
            let entry = st
                .image_bitmaps
                .get_mut(&h)
                .ok_or_else(|| vm.type_error("Illegal invocation on ImageBitmap"))?;
            entry.closed = true;
            entry.rgba.clear();
            Ok(JsValue::UNDEFINED)
        });
    }
    let ib_ctor = vm.alloc_host_fn("ImageBitmap", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: ImageBitmap"))
    });
    bind_ctor_proto(vm, ib_ctor, ibp);
    define_global(vm, "ImageBitmap", JsValue::from_object(ib_ctor));

    // OffscreenCanvas interface (WHATWG HTML §4.12.5.14)
    let ocp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(ocp).unwrap().is_prototype = true;
    crate::dom_interfaces::define_to_string_tag(vm, ocp, "OffscreenCanvas");
    canvas_sys.borrow_mut().protos.offscreen_canvas_proto = Some(ocp);
    {
        let cs_g = Rc::clone(canvas_sys);
        let cs_s = Rc::clone(canvas_sys);
        define_host_accessor(
            vm,
            ocp,
            "width",
            move |vm, this_val, _| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
                };
                let w = cs_g
                    .borrow()
                    .offscreen_canvases
                    .get(&h)
                    .map(|c| c.width)
                    .ok_or_else(|| vm.type_error("Illegal invocation on OffscreenCanvas"))?;
                Ok(JsValue::from_f64(f64::from(w)))
            },
            move |vm, this_val, args| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
                };
                let w = vm.to_u32(arg(args, 0))?;
                let mut st = cs_s.borrow_mut();
                let entry = st
                    .offscreen_canvases
                    .get_mut(&h)
                    .ok_or_else(|| vm.type_error("Illegal invocation on OffscreenCanvas"))?;
                entry.width = w;
                entry.rgba = vec![0u8; (w as usize).saturating_mul(entry.height as usize) * 4];
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let cs_g = Rc::clone(canvas_sys);
        let cs_s = Rc::clone(canvas_sys);
        define_host_accessor(
            vm,
            ocp,
            "height",
            move |vm, this_val, _| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
                };
                let h_px = cs_g
                    .borrow()
                    .offscreen_canvases
                    .get(&h)
                    .map(|c| c.height)
                    .ok_or_else(|| vm.type_error("Illegal invocation on OffscreenCanvas"))?;
                Ok(JsValue::from_f64(f64::from(h_px)))
            },
            move |vm, this_val, args| {
                let Some(h) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
                };
                let h_px = vm.to_u32(arg(args, 0))?;
                let mut st = cs_s.borrow_mut();
                let entry = st
                    .offscreen_canvases
                    .get_mut(&h)
                    .ok_or_else(|| vm.type_error("Illegal invocation on OffscreenCanvas"))?;
                entry.height = h_px;
                entry.rgba = vec![0u8; (entry.width as usize).saturating_mul(h_px as usize) * 4];
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let cs = Rc::clone(canvas_sys);
        define_host_method(vm, ocp, "getContext", move |vm, this_val, args| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
            };
            if !cs.borrow().offscreen_canvases.contains_key(&h) {
                return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
            }
            let kind = vm.value_to_rust_string(arg(args, 0));
            if kind != "2d" {
                return Ok(JsValue::NULL);
            }
            let ctx = vm.alloc_ordinary_object();
            let k_c = vm.heap.intern_key("canvas");
            vm.heap.object_define_data(
                ctx,
                k_c,
                JsValue::from_object(h),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            let k_fs = vm.heap.intern_key("fillStyle");
            let black = JsValue::from_string(vm.heap.alloc_string("#000000"));
            vm.heap
                .object_define_data(ctx, k_fs, black, PropertyAttrs::DEFAULT, &mut vm.shapes);
            define_host_method(vm, ctx, "fillRect", |_, _, _| Ok(JsValue::UNDEFINED));
            define_host_method(vm, ctx, "clearRect", |_, _, _| Ok(JsValue::UNDEFINED));
            let cs_gi = Rc::clone(&cs);
            define_host_method(vm, ctx, "getImageData", move |vm, _, args| {
                let w = vm.to_u32(arg(args, 2)).unwrap_or(1).max(1);
                let h_px = vm.to_u32(arg(args, 3)).unwrap_or(1).max(1);
                let rgba = vec![0u8; (w as usize).saturating_mul(h_px as usize) * 4];
                Ok(JsValue::from_object(create_image_data_obj(
                    vm, &cs_gi, w, h_px, rgba,
                )))
            });
            define_host_method(vm, ctx, "putImageData", |_, _, _| Ok(JsValue::UNDEFINED));
            Ok(JsValue::from_object(ctx))
        });
    }
    {
        let cs = Rc::clone(canvas_sys);
        define_host_method(vm, ocp, "transferToImageBitmap", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation on OffscreenCanvas"));
            };
            let (w, h_px, rgba) = {
                let st = cs.borrow();
                let entry = st
                    .offscreen_canvases
                    .get(&h)
                    .ok_or_else(|| vm.type_error("Illegal invocation on OffscreenCanvas"))?;
                (entry.width, entry.height, entry.rgba.clone())
            };
            Ok(JsValue::from_object(create_image_bitmap_obj(
                vm, &cs, w, h_px, rgba,
            )))
        });
    }
    {
        let cs = Rc::clone(canvas_sys);
        let oc_ctor = vm.alloc_host_fn("OffscreenCanvas", move |vm, _, args| {
            if vm.native_new_target.is_undefined() {
                return Err(vm.type_error("OffscreenCanvas constructor requires 'new'"));
            }
            let w = vm.to_u32(arg(args, 0))?;
            let h_px = vm.to_u32(arg(args, 1))?;
            let rgba = vec![0u8; (w as usize).saturating_mul(h_px as usize).min(16_777_216) * 4];
            Ok(JsValue::from_object(create_offscreen_canvas_obj(
                vm, &cs, w, h_px, rgba,
            )))
        });
        bind_ctor_proto(vm, oc_ctor, ocp);
        define_global(vm, "OffscreenCanvas", JsValue::from_object(oc_ctor));
    }

    // global `createImageBitmap(image)` -> Promise<ImageBitmap>
    {
        let cs = Rc::clone(canvas_sys);
        let h_ib = Rc::clone(host);
        let g = vm.global;
        define_host_method(vm, g, "createImageBitmap", move |vm, _, args| {
            let src = arg(args, 0);
            let dims = src.as_object().and_then(|sh| {
                if let Some(oc) = cs.borrow().offscreen_canvases.get(&sh) {
                    return Some((oc.width, oc.height, oc.rgba.clone()));
                }
                if let Some(ib) = cs.borrow().image_bitmaps.get(&sh) {
                    if !ib.closed {
                        return Some((ib.width, ib.height, ib.rgba.clone()));
                    }
                }
                if let Some((w, h_px, rgba)) = extract_image_data_pixels(vm, &cs, src) {
                    return Some((w, h_px, rgba));
                }
                if let Some(nid) = h_ib.borrow().node_for_handle(vm, sh) {
                    let is_canvas = matches!(
                        h_ib.borrow().document.borrow().get(nid),
                        Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("canvas")
                    );
                    if is_canvas {
                        let w = canvas_attr_dim(&h_ib, nid, "width", 300);
                        let h_px = canvas_attr_dim(&h_ib, nid, "height", 150);
                        let rgba = vec![0u8; (w as usize).saturating_mul(h_px as usize).min(16_777_216) * 4];
                        return Some((w, h_px, rgba));
                    }
                }
                None
            });
            let p = rix_js::promise::alloc_promise(vm);
            if let Some((w, h_px, rgba)) = dims {
                let bm = create_image_bitmap_obj(vm, &cs, w, h_px, rgba);
                rix_js::promise::resolve_promise(vm, p, JsValue::from_object(bm));
            } else {
                let err = create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Invalid createImageBitmap source",
                );
                rix_js::promise::reject_promise(vm, p, err.value);
            }
            Ok(JsValue::from_object(p))
        });
    }
}

fn install_gradient_and_pattern(vm: &mut JsVm, canvas_sys: &SharedCanvas2D) {
    let gp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(gp).unwrap().is_prototype = true;
    canvas_sys.borrow_mut().protos.gradient_proto = Some(gp);

    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, gp, "addColorStop", move |vm, this_val, args| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible CanvasGradient receiver"));
        };
        let offset = vm.to_number(arg(args, 0))? as f32;
        let color_str = vm.value_to_rust_string(arg(args, 1));
        let Some(color) = parse_css_color(&color_str) else {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Invalid CSS color: {color_str}"),
            ));
        };
        let ok = cs
            .borrow_mut()
            .gradients
            .get_mut(&h)
            .ok_or_else(|| vm.type_error("Incompatible CanvasGradient receiver"))?
            .add_color_stop(offset, color);
        if !ok {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "Color stop offset must be in [0, 1]",
            ));
        }
        Ok(JsValue::UNDEFINED)
    });

    let g_ctor = vm.alloc_host_fn("CanvasGradient", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: CanvasGradient"))
    });
    bind_ctor_proto(vm, g_ctor, gp);
    define_global(vm, "CanvasGradient", JsValue::from_object(g_ctor));

    let pp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(pp).unwrap().is_prototype = true;
    canvas_sys.borrow_mut().protos.pattern_proto = Some(pp);

    let p_ctor = vm.alloc_host_fn("CanvasPattern", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: CanvasPattern"))
    });
    bind_ctor_proto(vm, p_ctor, pp);
    define_global(vm, "CanvasPattern", JsValue::from_object(p_ctor));
}

fn install_html_canvas_element_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    web_apis: &SharedWebApis,
    canvas_sys: &SharedCanvas2D,
) {
    let cp = host.borrow().prototypes.html_canvas_element_proto;

    // width & height attributes synced with backing Canvas2DContext
    for (attr, default_dim) in [("width", 300u32), ("height", 150u32)] {
        let h_get = Rc::clone(host);
        let h_set = Rc::clone(host);
        let cs_set = Rc::clone(canvas_sys);
        define_host_accessor(
            vm,
            cp,
            attr,
            move |vm, this_val, _| {
                let nid = require_canvas_node(vm, &h_get, this_val)?;
                let val = canvas_attr_dim(&h_get, nid, attr, default_dim);
                Ok(JsValue::from_i32(val as i32))
            },
            move |vm, this_val, args| {
                let nid = require_canvas_node(vm, &h_set, this_val)?;
                let next = vm.to_u32(arg(args, 0))?;
                set_element_attr(vm, &h_set, nid, attr, &next.to_string());
                let new_w = canvas_attr_dim(&h_set, nid, "width", 300);
                let new_h = canvas_attr_dim(&h_set, nid, "height", 150);
                if let Some(ctx) = cs_set.borrow_mut().contexts.get_mut(&nid) {
                    ctx.resize(new_w, new_h);
                }
                let hooks: Vec<CanvasResizeHook> = cs_set.borrow().resize_hooks.clone();
                for hook in hooks {
                    hook(nid, new_w, new_h);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // getContext("2d" | "webgpu" | "webgl2" | "webgl")
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, cp, "getContext", move |vm, this_val, args| {
        let nid = require_canvas_node(vm, &h, this_val)?;
        let kind = vm.value_to_rust_string(arg(args, 0));
        if !kind.eq_ignore_ascii_case("2d") {
            let opts = arg(args, 1);
            let hooks: Vec<CanvasContextHook> = cs.borrow().context_hooks.clone();
            for hook in hooks {
                if let Some(ctx_val) = hook(vm, nid, &kind, opts)? {
                    return Ok(ctx_val);
                }
            }
            return Ok(JsValue::NULL);
        }
        ensure_context_for_node(&h, &cs, nid);
        if let Some(&existing) = cs.borrow().node_to_ctx_handle.get(&nid) {
            if vm.heap.is_alive(existing) {
                return Ok(JsValue::from_object(existing));
            }
        }
        let proto = cs.borrow().protos.ctx2d_proto.unwrap_or(vm.object_proto);
        let ctx_obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let mut st = cs.borrow_mut();
        st.ctx_handle_to_node.insert(ctx_obj, nid);
        st.node_to_ctx_handle.insert(nid, ctx_obj);
        Ok(JsValue::from_object(ctx_obj))
    });

    // toDataURL(type?)
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, cp, "toDataURL", move |vm, this_val, _| {
        let nid = require_canvas_node(vm, &h, this_val)?;
        let hooks: Vec<CanvasReadbackHook> = cs.borrow().readback_hooks.clone();
        let hook_pixels = hooks.into_iter().find_map(|hook| hook(nid));
        let pixels = if let Some(px) = hook_pixels {
            Some(px)
        } else {
            ensure_context_for_node(&h, &cs, nid);
            cs.borrow_mut().readback_node_rgba(nid)
        };
        let Some((w, h_px, rgba)) = pixels else {
            return Ok(JsValue::from_string(vm.heap.alloc_string("data:,")));
        };
        let url =
            encode_rgba_to_png_data_url(w, h_px, &rgba).unwrap_or_else(|_| "data:,".to_string());
        Ok(JsValue::from_string(vm.heap.alloc_string(url)))
    });

    // toBlob(callback, type?)
    let h = Rc::clone(host);
    let wa = Rc::clone(web_apis);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, cp, "toBlob", move |vm, this_val, args| {
        let nid = require_canvas_node(vm, &h, this_val)?;
        let cb = arg(args, 0);
        if !cb.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
            return Err(vm.type_error("HTMLCanvasElement.toBlob requires a callback function"));
        }
        let hooks: Vec<CanvasReadbackHook> = cs.borrow().readback_hooks.clone();
        let hook_pixels = hooks.into_iter().find_map(|hook| hook(nid));
        let pixels = if let Some(px) = hook_pixels {
            Some(px)
        } else {
            ensure_context_for_node(&h, &cs, nid);
            cs.borrow_mut().readback_node_rgba(nid)
        };
        let blob_val = match pixels {
            Some((w, h_px, rgba)) => match encode_rgba_to_png(w, h_px, &rgba) {
                Ok(png) => {
                    JsValue::from_object(create_blob_object(vm, &wa, png, "image/png".to_string()))
                }
                Err(_) => JsValue::NULL,
            },
            None => JsValue::NULL,
        };
        let _ = vm.call_value(cb, JsValue::UNDEFINED, &[blob_val]);
        Ok(JsValue::UNDEFINED)
    });
}

fn parse_js_paint(vm: &JsVm, canvas_sys: &SharedCanvas2D, val: JsValue) -> Option<CanvasPaint> {
    if let Some(h) = val.as_object() {
        let cs = canvas_sys.borrow();
        if let Some(g) = cs.gradients.get(&h) {
            return Some(CanvasPaint::Gradient(g.clone()));
        }
        if let Some(p) = cs.patterns.get(&h) {
            return Some(CanvasPaint::Pattern(p.clone()));
        }
    }
    let s = vm.value_to_rust_string(val);
    parse_css_color(&s).map(CanvasPaint::Color)
}

fn format_paint_for_js(vm: &mut JsVm, paint: &CanvasPaint) -> JsValue {
    match paint {
        CanvasPaint::Color(c) => {
            let [r, g, b, a] = c.to_rgba8().to_u8_array();
            let s = if a == 255 {
                format!("#{r:02x}{g:02x}{b:02x}")
            } else {
                format!("rgba({r}, {g}, {b}, {:.3})", f32::from(a) / 255.0)
            };
            JsValue::from_string(vm.heap.alloc_string(s))
        }
        CanvasPaint::Gradient(_) => JsValue::from_string(vm.heap.alloc_string("[CanvasGradient]")),
        CanvasPaint::Pattern(_) => JsValue::from_string(vm.heap.alloc_string("[CanvasPattern]")),
    }
}

fn install_context2d_proto(vm: &mut JsVm, host: &SharedHost, canvas_sys: &SharedCanvas2D) {
    let p = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(p).unwrap().is_prototype = true;
    canvas_sys.borrow_mut().protos.ctx2d_proto = Some(p);

    // canvas getter
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_getter(vm, p, "canvas", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        Ok(wrap_node(vm, &h, nid))
    });

    // State stack
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "save", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.save();
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "restore", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.restore();
        }
        Ok(JsValue::UNDEFINED)
    });

    // Transforms
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "translate", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y) = (vm.to_number(arg(args, 0))?, vm.to_number(arg(args, 1))?);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.translate(x, y);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "rotate", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let angle = vm.to_number(arg(args, 0))?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.rotate(angle);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "scale", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y) = (vm.to_number(arg(args, 0))?, vm.to_number(arg(args, 1))?);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.scale(x, y);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "transform", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let vals = [
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
            vm.to_number(arg(args, 5))?,
        ];
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.transform(vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "setTransform", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let vals = [
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
            vm.to_number(arg(args, 5))?,
        ];
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.set_transform(vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "resetTransform", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.reset_transform();
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "getTransform", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let [a, b, c, d, e, f] = cs.borrow().contexts.get(&nid).map_or(
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            Canvas2DContext::get_transform,
        );
        let mat = vm.alloc_ordinary_object();
        for (name, v) in [
            ("a", a),
            ("b", b),
            ("c", c),
            ("d", d),
            ("e", e),
            ("f", f),
            ("m11", a),
            ("m12", b),
            ("m21", c),
            ("m22", d),
            ("m41", e),
            ("m42", f),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                mat,
                k,
                JsValue::from_f64(v),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        Ok(JsValue::from_object(mat))
    });

    // Style & Compositing Accessors
    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "fillStyle",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let paint = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map(|c| c.state().fill_style.clone())
                .unwrap_or_default();
            Ok(format_paint_for_js(vm, &paint))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            if let Some(paint) = parse_js_paint(vm, &cs_s, arg(args, 0)) {
                if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                    ctx.set_fill_style(paint);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "strokeStyle",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let paint = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map(|c| c.state().stroke_style.clone())
                .unwrap_or_default();
            Ok(format_paint_for_js(vm, &paint))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            if let Some(paint) = parse_js_paint(vm, &cs_s, arg(args, 0)) {
                if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                    ctx.set_stroke_style(paint);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "globalAlpha",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let a = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or(1.0, |c| f64::from(c.state().global_alpha));
            Ok(JsValue::from_f64(a))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let a = vm.to_number(arg(args, 0))? as f32;
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_global_alpha(a);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "lineWidth",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let lw = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or(1.0, |c| c.state().line_width);
            Ok(JsValue::from_f64(lw))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let lw = vm.to_number(arg(args, 0))?;
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_line_width(lw);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "lineCap",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let cap = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or("butt", Canvas2DContext::line_cap_str);
            Ok(JsValue::from_string(vm.heap.alloc_string(cap)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let cap = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_line_cap(&cap);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "lineJoin",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let join = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or("miter", Canvas2DContext::line_join_str);
            Ok(JsValue::from_string(vm.heap.alloc_string(join)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let join = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_line_join(&join);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "miterLimit",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let ml = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or(10.0, |c| c.state().miter_limit);
            Ok(JsValue::from_f64(ml))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let ml = vm.to_number(arg(args, 0))?;
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_miter_limit(ml);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "lineDashOffset",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let off = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or(0.0, |c| c.state().line_dash_offset);
            Ok(JsValue::from_f64(off))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let off = vm.to_number(arg(args, 0))?;
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_line_dash_offset(off);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "setLineDash", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let items = vm.collect_iterable(arg(args, 0))?;
        let mut segs = Vec::with_capacity(items.len());
        for v in items {
            segs.push(vm.to_number(v)?);
        }
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.set_line_dash(segs);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "getLineDash", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let segs = cs
            .borrow()
            .contexts
            .get(&nid)
            .map(|c| c.state().line_dash.clone())
            .unwrap_or_default();
        let arr = vm.alloc_array();
        for v in segs {
            vm.heap.object_push_element(arr, JsValue::from_f64(v));
        }
        Ok(JsValue::from_object(arr))
    });

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "globalCompositeOperation",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let op = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map(|c| c.state().composite_op.clone())
                .unwrap_or_else(|| "source-over".to_string());
            Ok(JsValue::from_string(vm.heap.alloc_string(op)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let op = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_global_composite_operation(&op);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    // Font / Text Accessors
    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "font",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let f = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map(|c| c.state().font_raw.clone())
                .unwrap_or_else(|| "10px sans-serif".to_string());
            Ok(JsValue::from_string(vm.heap.alloc_string(f)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let f = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_font(&f);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "textAlign",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let s = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or("start", |c| c.state().text_align.as_str());
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let s = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_text_align(&s);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let cs_g = Rc::clone(canvas_sys);
    let cs_s = Rc::clone(canvas_sys);
    define_host_accessor(
        vm,
        p,
        "textBaseline",
        move |vm, this_val, _| {
            let nid = require_ctx2d_node(vm, &cs_g, this_val)?;
            let s = cs_g
                .borrow()
                .contexts
                .get(&nid)
                .map_or("alphabetic", |c| c.state().text_baseline.as_str());
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        },
        move |vm, this_val, args| {
            let nid = require_ctx2d_node(vm, &cs_s, this_val)?;
            let s = vm.value_to_rust_string(arg(args, 0));
            if let Some(ctx) = cs_s.borrow_mut().contexts.get_mut(&nid) {
                ctx.set_text_baseline(&s);
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    // Path methods
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "beginPath", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.begin_path();
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "closePath", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.close_path();
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "moveTo", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y) = (vm.to_number(arg(args, 0))?, vm.to_number(arg(args, 1))?);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.move_to(x, y);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "lineTo", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y) = (vm.to_number(arg(args, 0))?, vm.to_number(arg(args, 1))?);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.line_to(x, y);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "quadraticCurveTo", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (cpx, cpy, x, y) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.quadratic_curve_to(cpx, cpy, x, y);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "bezierCurveTo", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let v = [
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
            vm.to_number(arg(args, 5))?,
        ];
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.bezier_curve_to(v[0], v[1], v[2], v[3], v[4], v[5]);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "arc", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, r, sa, ea) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
        );
        let ccw = vm.to_boolean(arg(args, 5));
        let res = cs
            .borrow_mut()
            .contexts
            .get_mut(&nid)
            .map_or(Ok(()), |ctx| ctx.arc(x, y, r, sa, ea, ccw));
        res.map_err(|msg| create_dom_exception(vm, "IndexSizeError", msg))?;
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "arcTo", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x1, y1, x2, y2, r) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
        );
        let res = cs
            .borrow_mut()
            .contexts
            .get_mut(&nid)
            .map_or(Ok(()), |ctx| ctx.arc_to(x1, y1, x2, y2, r));
        res.map_err(|msg| create_dom_exception(vm, "IndexSizeError", msg))?;
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "ellipse", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, rx, ry, rot, sa, ea) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
            vm.to_number(arg(args, 5))?,
            vm.to_number(arg(args, 6))?,
        );
        let ccw = vm.to_boolean(arg(args, 7));
        let res = cs
            .borrow_mut()
            .contexts
            .get_mut(&nid)
            .map_or(Ok(()), |ctx| ctx.ellipse(x, y, rx, ry, rot, sa, ea, ccw));
        res.map_err(|msg| create_dom_exception(vm, "IndexSizeError", msg))?;
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "rect", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, w, h) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.rect(x, y, w, h);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "roundRect", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, w, h) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        let r_arg = arg(args, 4);
        let radii = if r_arg.is_undefined() {
            [0.0; 4]
        } else if r_arg.as_object().is_some() {
            let items = vm.collect_iterable(r_arg)?;
            let mut vals = Vec::with_capacity(items.len());
            for v in items {
                vals.push(vm.to_number(v)?);
            }
            match vals.as_slice() {
                [] => [0.0; 4],
                [a] => [*a; 4],
                [a, b] => [*a, *b, *a, *b],
                [a, b, c] => [*a, *b, *c, *b],
                [a, b, c, d, ..] => [*a, *b, *c, *d],
            }
        } else {
            [vm.to_number(r_arg)?; 4]
        };
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.round_rect(x, y, w, h, radii);
        }
        Ok(JsValue::UNDEFINED)
    });

    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "fill", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let rule = if arg(args, 0).is_undefined() {
            CanvasFillRule::NonZero
        } else {
            CanvasFillRule::from_str_rule(&vm.value_to_rust_string(arg(args, 0)))
        };
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.fill(rule);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "stroke", move |vm, this_val, _| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.stroke();
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "clip", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let rule = if arg(args, 0).is_undefined() {
            CanvasFillRule::NonZero
        } else {
            CanvasFillRule::from_str_rule(&vm.value_to_rust_string(arg(args, 0)))
        };
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.clip(rule);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "isPointInPath", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y) = (vm.to_number(arg(args, 0))?, vm.to_number(arg(args, 1))?);
        let rule = if arg(args, 2).is_undefined() {
            CanvasFillRule::NonZero
        } else {
            CanvasFillRule::from_str_rule(&vm.value_to_rust_string(arg(args, 2)))
        };
        let hit = cs
            .borrow()
            .contexts
            .get(&nid)
            .is_some_and(|c| c.is_point_in_path(x, y, rule));
        Ok(JsValue::from_bool(hit))
    });

    // Rectangles
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "fillRect", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, w, h) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.fill_rect(x, y, w, h);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "strokeRect", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, w, h) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.stroke_rect(x, y, w, h);
        }
        Ok(JsValue::UNDEFINED)
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "clearRect", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let (x, y, w, h) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.clear_rect(x, y, w, h);
        }
        Ok(JsValue::UNDEFINED)
    });

    // Gradients & Patterns
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "createLinearGradient", move |vm, this_val, args| {
        let _ = require_ctx2d_node(vm, &cs, this_val)?;
        let (x0, y0, x1, y1) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
        );
        let gp = cs.borrow().protos.gradient_proto.unwrap_or(vm.object_proto);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(gp)));
        cs.borrow_mut()
            .gradients
            .insert(obj, CanvasGradient::linear(x0, y0, x1, y1));
        Ok(JsValue::from_object(obj))
    });
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "createRadialGradient", move |vm, this_val, args| {
        let _ = require_ctx2d_node(vm, &cs, this_val)?;
        let (x0, y0, r0, x1, y1, r1) = (
            vm.to_number(arg(args, 0))?,
            vm.to_number(arg(args, 1))?,
            vm.to_number(arg(args, 2))?,
            vm.to_number(arg(args, 3))?,
            vm.to_number(arg(args, 4))?,
            vm.to_number(arg(args, 5))?,
        );
        if r0 < 0.0 || r1 < 0.0 {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "Radial gradient radius must be non-negative",
            ));
        }
        let gp = cs.borrow().protos.gradient_proto.unwrap_or(vm.object_proto);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(gp)));
        cs.borrow_mut()
            .gradients
            .insert(obj, CanvasGradient::radial(x0, y0, r0, x1, y1, r1));
        Ok(JsValue::from_object(obj))
    });
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "createPattern", move |vm, this_val, args| {
        let _ = require_ctx2d_node(vm, &cs, this_val)?;
        let src = arg(args, 0);
        let rep_str = if arg(args, 1).is_undefined() {
            "repeat".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let repeat = CanvasPatternRepeat::parse(&rep_str).ok_or_else(|| {
            create_dom_exception(vm, "SyntaxError", &format!("Invalid repetition: {rep_str}"))
        })?;
        let rgba_opt = if let Some((w, h_px, px)) = extract_image_data_pixels(vm, &cs, src) {
            Some((w, h_px, px))
        } else if let Ok(src_nid) = require_canvas_node(vm, &h, src) {
            ensure_context_for_node(&h, &cs, src_nid);
            cs.borrow_mut().readback_node_rgba(src_nid)
        } else {
            None
        };
        let Some((w, h_px, px)) = rgba_opt else {
            return Ok(JsValue::NULL);
        };
        let pp = cs.borrow().protos.pattern_proto.unwrap_or(vm.object_proto);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(pp)));
        cs.borrow_mut()
            .patterns
            .insert(obj, CanvasPattern::from_rgba8(w, h_px, px, repeat));
        Ok(JsValue::from_object(obj))
    });

    // Text
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "fillText", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let text = vm.value_to_rust_string(arg(args, 0));
        let (x, y) = (vm.to_number(arg(args, 1))?, vm.to_number(arg(args, 2))?);
        let max_w = if args.len() > 3 && !arg(args, 3).is_undefined() {
            Some(vm.to_number(arg(args, 3))?)
        } else {
            None
        };
        let font_db = std::sync::Arc::clone(&h.borrow().font_db);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.fill_text(&font_db, &text, x, y, max_w);
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "strokeText", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let text = vm.value_to_rust_string(arg(args, 0));
        let (x, y) = (vm.to_number(arg(args, 1))?, vm.to_number(arg(args, 2))?);
        let max_w = if args.len() > 3 && !arg(args, 3).is_undefined() {
            Some(vm.to_number(arg(args, 3))?)
        } else {
            None
        };
        let font_db = std::sync::Arc::clone(&h.borrow().font_db);
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.stroke_text(&font_db, &text, x, y, max_w);
        }
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "measureText", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let text = vm.value_to_rust_string(arg(args, 0));
        let font_db = std::sync::Arc::clone(&h.borrow().font_db);
        let metrics = cs
            .borrow()
            .contexts
            .get(&nid)
            .map(|c| c.measure_text(&font_db, &text))
            .unwrap_or_default();
        let obj = vm.alloc_ordinary_object();
        for (name, v) in [
            ("width", metrics.width),
            (
                "actualBoundingBoxAscent",
                metrics.actual_bounding_box_ascent,
            ),
            (
                "actualBoundingBoxDescent",
                metrics.actual_bounding_box_descent,
            ),
            ("fontBoundingBoxAscent", metrics.font_bounding_box_ascent),
            ("fontBoundingBoxDescent", metrics.font_bounding_box_descent),
        ] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                obj,
                k,
                JsValue::from_f64(v),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        Ok(JsValue::from_object(obj))
    });

    // drawImage (canvas-to-canvas zero-readback GPU scene append OR ImageData)
    let h = Rc::clone(host);
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "drawImage", move |vm, this_val, args| {
        let dst_nid = require_ctx2d_node(vm, &cs, this_val)?;
        let src = arg(args, 0);
        let parse_rects =
            |vm: &mut JsVm, nat_w: f64, nat_h: f64| -> Result<[f64; 8], JsException> {
                match args.len() {
                    3 => Ok([
                        0.0,
                        0.0,
                        nat_w,
                        nat_h,
                        vm.to_number(arg(args, 1))?,
                        vm.to_number(arg(args, 2))?,
                        nat_w,
                        nat_h,
                    ]),
                    5 => Ok([
                        0.0,
                        0.0,
                        nat_w,
                        nat_h,
                        vm.to_number(arg(args, 1))?,
                        vm.to_number(arg(args, 2))?,
                        vm.to_number(arg(args, 3))?,
                        vm.to_number(arg(args, 4))?,
                    ]),
                    9.. => Ok([
                        vm.to_number(arg(args, 1))?,
                        vm.to_number(arg(args, 2))?,
                        vm.to_number(arg(args, 3))?,
                        vm.to_number(arg(args, 4))?,
                        vm.to_number(arg(args, 5))?,
                        vm.to_number(arg(args, 6))?,
                        vm.to_number(arg(args, 7))?,
                        vm.to_number(arg(args, 8))?,
                    ]),
                    _ => Err(vm.type_error("drawImage expects 3, 5, or 9 arguments")),
                }
            };

        if let Ok(src_nid) = require_canvas_node(vm, &h, src) {
            ensure_context_for_node(&h, &cs, src_nid);
            let (sw_nat, sh_nat) = cs
                .borrow()
                .contexts
                .get(&src_nid)
                .map_or((300.0, 150.0), |c| {
                    (f64::from(c.width()), f64::from(c.height()))
                });
            let r = parse_rects(vm, sw_nat, sh_nat)?;
            if src_nid != dst_nid {
                let mut st = cs.borrow_mut();
                // Snapshot source canvas scene cleanly without overlapping borrows!
                let src_snap = st.contexts.get(&src_nid).map(|c| {
                    let mut clone_ctx = Canvas2DContext::new(c.surface_id(), c.width(), c.height());
                    clone_ctx.draw_canvas(
                        c,
                        0.0,
                        0.0,
                        f64::from(c.width()),
                        f64::from(c.height()),
                        0.0,
                        0.0,
                        f64::from(c.width()),
                        f64::from(c.height()),
                    );
                    clone_ctx
                });
                if let (Some(src_ctx), Some(dst_ctx)) = (src_snap, st.contexts.get_mut(&dst_nid)) {
                    dst_ctx.draw_canvas(&src_ctx, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
                }
            }
            return Ok(JsValue::UNDEFINED);
        }

        if let Some((iw, ih, px)) = extract_image_data_pixels(vm, &cs, src) {
            let r = parse_rects(vm, f64::from(iw), f64::from(ih))?;
            if let Some(dst_ctx) = cs.borrow_mut().contexts.get_mut(&dst_nid) {
                dst_ctx.draw_image_rgba(iw, ih, px, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // ImageData operations: createImageData / getImageData / putImageData
    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "createImageData", move |vm, this_val, args| {
        let _ = require_ctx2d_node(vm, &cs, this_val)?;
        let (w, h) = if let Some((ew, eh, _)) = extract_image_data_pixels(vm, &cs, arg(args, 0)) {
            (ew, eh)
        } else {
            (
                vm.to_number(arg(args, 0))?.abs().trunc() as u32,
                vm.to_number(arg(args, 1))?.abs().trunc() as u32,
            )
        };
        if w == 0 || h == 0 {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "ImageData dimensions must be non-zero",
            ));
        }
        let bytes = vec![0u8; (w as usize) * (h as usize) * 4];
        Ok(JsValue::from_object(create_image_data_obj(
            vm, &cs, w, h, bytes,
        )))
    });

    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "getImageData", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let sx = vm.to_number(arg(args, 0))?.trunc() as i64;
        let sy = vm.to_number(arg(args, 1))?.trunc() as i64;
        let sw = vm.to_number(arg(args, 2))?.trunc() as i64;
        let sh = vm.to_number(arg(args, 3))?.trunc() as i64;
        if sw == 0 || sh == 0 {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "getImageData width/height cannot be zero",
            ));
        }
        let (x0, w) = if sw > 0 {
            (sx, sw as u32)
        } else {
            (sx + sw, (-sw) as u32)
        };
        let (y0, h) = if sh > 0 {
            (sy, sh as u32)
        } else {
            (sy + sh, (-sh) as u32)
        };
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        if let Some((cw, ch, full)) = cs.borrow_mut().readback_node_rgba(nid) {
            for row in 0..i64::from(h) {
                let src_y = y0 + row;
                if src_y < 0 || src_y >= i64::from(ch) {
                    continue;
                }
                for col in 0..i64::from(w) {
                    let src_x = x0 + col;
                    if src_x < 0 || src_x >= i64::from(cw) {
                        continue;
                    }
                    let si = ((src_y as usize) * (cw as usize) + (src_x as usize)) * 4;
                    let di = ((row as usize) * (w as usize) + (col as usize)) * 4;
                    out[di..di + 4].copy_from_slice(&full[si..si + 4]);
                }
            }
        }
        Ok(JsValue::from_object(create_image_data_obj(
            vm, &cs, w, h, out,
        )))
    });

    let cs = Rc::clone(canvas_sys);
    define_host_method(vm, p, "putImageData", move |vm, this_val, args| {
        let nid = require_ctx2d_node(vm, &cs, this_val)?;
        let Some((w, h, pixels)) = extract_image_data_pixels(vm, &cs, arg(args, 0)) else {
            return Err(vm.type_error("putImageData requires an ImageData object"));
        };
        let dx = vm.to_number(arg(args, 1))?;
        let dy = vm.to_number(arg(args, 2))?;
        if let Some(ctx) = cs.borrow_mut().contexts.get_mut(&nid) {
            ctx.put_image_data(w, h, pixels, dx, dy);
        }
        Ok(JsValue::UNDEFINED)
    });

    let ctor = vm.alloc_host_fn("CanvasRenderingContext2D", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: CanvasRenderingContext2D"))
    });
    bind_ctor_proto(vm, ctor, p);
    define_global(vm, "CanvasRenderingContext2D", JsValue::from_object(ctor));
}
