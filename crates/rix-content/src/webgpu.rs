//! Native W3C WebGPU (`navigator.gpu` + `GPUCanvasContext`) mapped cleanly to
//! `rix_render::wgpu` and composited via `DisplayItem::ExternalSurface` without
//! rebuilding `DisplayList`s or mutating DOM nodes.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #26.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU64;
use std::rc::Rc;
use std::sync::Arc;

use rix_dom::NodeId;
use rix_ipc::SurfaceId;
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::surface_id_for_node;
use rix_render::wgpu;
use rix_render::{
    readback_wgpu_buffer_range, readback_wgpu_texture_rgba, request_device_from_adapter,
    request_headless_adapter, wait_for_gpu_idle, write_wgpu_buffer_mapped, Compositor,
    SharedGpuDevice, VelloRenderer,
};

use crate::canvas2d::{canvas_attr_dim, SharedCanvas2D};
use crate::dom_bridge::{wrap_node, SharedHost};
use crate::web_apis::{
    alloc_array_buffer, arg, bind_ctor_proto, create_dom_exception, define_global,
    define_host_getter, define_host_method,
};
use crate::webgpu_types::*;

#[derive(Debug, Clone, Default)]
struct WebGpuPrototypes {
    gpu_proto: Option<HeapHandle>,
    adapter_proto: Option<HeapHandle>,
    device_proto: Option<HeapHandle>,
    queue_proto: Option<HeapHandle>,
    buffer_proto: Option<HeapHandle>,
    texture_proto: Option<HeapHandle>,
    texture_view_proto: Option<HeapHandle>,
    sampler_proto: Option<HeapHandle>,
    shader_module_proto: Option<HeapHandle>,
    bind_group_layout_proto: Option<HeapHandle>,
    pipeline_layout_proto: Option<HeapHandle>,
    bind_group_proto: Option<HeapHandle>,
    render_pipeline_proto: Option<HeapHandle>,
    compute_pipeline_proto: Option<HeapHandle>,
    command_encoder_proto: Option<HeapHandle>,
    render_pass_proto: Option<HeapHandle>,
    compute_pass_proto: Option<HeapHandle>,
    command_buffer_proto: Option<HeapHandle>,
    canvas_ctx_proto: Option<HeapHandle>,
}

struct GpuAdapterEntry {
    adapter: wgpu::Adapter,
    shared_override: Option<SharedGpuDevice>,
}

#[derive(Clone)]
struct GpuDeviceEntry {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer_token: u64,
    queue_handle: HeapHandle,
    error_scopes: Rc<RefCell<Vec<wgpu::ErrorScopeGuard>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BufferMapState {
    Unmapped,
    MappedAtCreation,
    MappedRead,
    MappedWrite,
}

impl BufferMapState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unmapped => "unmapped",
            Self::MappedAtCreation | Self::MappedRead | Self::MappedWrite => "mapped",
        }
    }
}

struct MappedBufferSlice {
    offset: u64,
    size: u64,
    array_buffer: HeapHandle,
}

struct GpuBufferEntry {
    device: wgpu::Device,
    buffer: wgpu::Buffer,
    size: u64,
    usage: u32,
    map_state: BufferMapState,
    mapped_range_start: u64,
    mapped_range_size: u64,
    mapped_read_bytes: Option<Vec<u8>>,
    mapped_slices: Vec<MappedBufferSlice>,
}

struct GpuTextureEntry {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
    depth_or_array_layers: u32,
    mip_level_count: u32,
    sample_count: u32,
    dimension: String,
    format: String,
    usage: u32,
    canvas_node: Option<NodeId>,
}

struct GpuTextureViewEntry {
    view: wgpu::TextureView,
    canvas_node: Option<NodeId>,
}

struct GpuShaderModuleEntry {
    module: wgpu::ShaderModule,
    compilation_errors: Vec<String>,
}

struct RenderPassEntry {
    encoder_handle: HeapHandle,
    label: Option<String>,
    color_attachments: Vec<Option<ColorAttachmentDesc>>,
    depth_stencil: Option<DepthStencilAttachmentDesc>,
    commands: Vec<RenderPassCmd>,
    ended: bool,
}

struct ComputePassEntry {
    encoder_handle: HeapHandle,
    label: Option<String>,
    commands: Vec<ComputePassCmd>,
    ended: bool,
}

struct GpuCommandEncoderEntry {
    encoder: Option<wgpu::CommandEncoder>,
    touched_canvases: Vec<NodeId>,
}

struct GpuCommandBufferEntry {
    buffer: Option<wgpu::CommandBuffer>,
    touched_canvases: Vec<NodeId>,
}

struct WebGpuCanvasContextEntry {
    surface_id: SurfaceId,
    width: u32,
    height: u32,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    renderer_token: u64,
    format: wgpu::TextureFormat,
    format_name: String,
    usage: wgpu::TextureUsages,
    current_texture: Option<wgpu::Texture>,
    dirty: bool,
}

/// Shared WebGPU runtime registry owned by [`crate::ContentRuntime`].
pub struct WebGpuSystem {
    shared_gpu: Option<SharedGpuDevice>,
    protos: WebGpuPrototypes,
    adapters: HashMap<HeapHandle, GpuAdapterEntry>,
    devices: HashMap<HeapHandle, GpuDeviceEntry>,
    queues: HashMap<HeapHandle, HeapHandle>,
    buffers: HashMap<HeapHandle, GpuBufferEntry>,
    textures: HashMap<HeapHandle, GpuTextureEntry>,
    texture_views: HashMap<HeapHandle, GpuTextureViewEntry>,
    samplers: HashMap<HeapHandle, wgpu::Sampler>,
    shader_modules: HashMap<HeapHandle, GpuShaderModuleEntry>,
    bind_group_layouts: HashMap<HeapHandle, wgpu::BindGroupLayout>,
    pipeline_layouts: HashMap<HeapHandle, wgpu::PipelineLayout>,
    bind_groups: HashMap<HeapHandle, wgpu::BindGroup>,
    render_pipelines: HashMap<HeapHandle, wgpu::RenderPipeline>,
    compute_pipelines: HashMap<HeapHandle, wgpu::ComputePipeline>,
    command_encoders: HashMap<HeapHandle, GpuCommandEncoderEntry>,
    render_passes: HashMap<HeapHandle, RenderPassEntry>,
    compute_passes: HashMap<HeapHandle, ComputePassEntry>,
    command_buffers: HashMap<HeapHandle, GpuCommandBufferEntry>,
    canvas_contexts: HashMap<NodeId, WebGpuCanvasContextEntry>,
    canvas_ctx_handle_to_node: HashMap<HeapHandle, NodeId>,
    canvas_node_to_ctx_handle: HashMap<NodeId, HeapHandle>,
}

/// Shared handle to [`WebGpuSystem`].
pub type SharedWebGpu = Rc<RefCell<WebGpuSystem>>;

impl Default for WebGpuSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl WebGpuSystem {
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared_gpu: None,
            protos: WebGpuPrototypes::default(),
            adapters: HashMap::new(),
            devices: HashMap::new(),
            queues: HashMap::new(),
            buffers: HashMap::new(),
            textures: HashMap::new(),
            texture_views: HashMap::new(),
            samplers: HashMap::new(),
            shader_modules: HashMap::new(),
            bind_group_layouts: HashMap::new(),
            pipeline_layouts: HashMap::new(),
            bind_groups: HashMap::new(),
            render_pipelines: HashMap::new(),
            compute_pipelines: HashMap::new(),
            command_encoders: HashMap::new(),
            render_passes: HashMap::new(),
            compute_passes: HashMap::new(),
            command_buffers: HashMap::new(),
            canvas_contexts: HashMap::new(),
            canvas_ctx_handle_to_node: HashMap::new(),
            canvas_node_to_ctx_handle: HashMap::new(),
        }
    }

    /// Attaches a compositor-owned [`SharedGpuDevice`] (from [`VelloRenderer::ensure_shared_gpu_device`])
    /// so `navigator.gpu` renders onto the same `wgpu::Device` as the compositor for zero-copy
    /// GPU-to-GPU `ExternalSurface` presentation.
    pub fn attach_shared_gpu(&mut self, shared: SharedGpuDevice) {
        self.shared_gpu = Some(shared);
    }

    /// Presents all dirty `GPUCanvasContext` swapchain textures into `compositor` with ZERO
    /// `DisplayList` rebuilds. Uses zero-copy `VelloRenderer::register_external_gpu_texture`
    /// whenever the canvas device matches `gpu`, falling back to RGBA readback otherwise.
    pub fn present_dirty_to_compositor(
        &mut self,
        compositor: &mut Compositor,
        mut gpu: Option<&mut VelloRenderer>,
    ) -> usize {
        let mut presented = 0usize;
        for ctx in self.canvas_contexts.values_mut() {
            if !ctx.dirty {
                continue;
            }
            let Some(tex) = ctx.current_texture.as_ref() else {
                ctx.dirty = false;
                continue;
            };
            let zero_copy_done = if let Some(ref mut renderer) = gpu {
                if ctx.renderer_token != 0 && ctx.renderer_token == renderer.renderer_token() {
                    if let Ok(img) =
                        renderer.register_external_gpu_texture(ctx.surface_id, tex.clone())
                    {
                        compositor.present_external_surface_image(ctx.surface_id, img);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !zero_copy_done {
                if let (Some(device), Some(queue)) = (ctx.device.as_ref(), ctx.queue.as_ref()) {
                    if let Ok(rgba) = readback_wgpu_texture_rgba(device, queue, tex) {
                        compositor.present_external_surface_rgba(
                            ctx.surface_id,
                            ctx.width,
                            ctx.height,
                            rgba,
                        );
                    }
                }
            }
            ctx.dirty = false;
            presented += 1;
        }
        presented
    }

    /// Reads back RGBA8 pixels for a WebGPU `<canvas>` node (used by `toDataURL`/`toBlob`).
    #[must_use]
    pub fn readback_canvas_rgba(&self, node: NodeId) -> Option<(u32, u32, Vec<u8>)> {
        let ctx = self.canvas_contexts.get(&node)?;
        let tex = ctx.current_texture.as_ref()?;
        let device = ctx.device.as_ref()?;
        let queue = ctx.queue.as_ref()?;
        let rgba = readback_wgpu_texture_rgba(device, queue, tex).ok()?;
        Some((ctx.width, ctx.height, rgba))
    }

    /// Resizes a WebGPU `<canvas>` swapchain when `canvas.width` or `canvas.height` changes.
    pub fn resize_canvas(&mut self, node: NodeId, width: u32, height: u32) {
        if let Some(ctx) = self.canvas_contexts.get_mut(&node) {
            let w = width.max(1);
            let h = height.max(1);
            if ctx.width != w || ctx.height != h {
                ctx.width = w;
                ctx.height = h;
                ctx.current_texture = None;
                ctx.dirty = true;
            }
        }
    }

    /// Collects GC roots for WebGPU prototypes, active device queues, mapped buffers, and canvas contexts.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.gpu_proto,
            self.protos.adapter_proto,
            self.protos.device_proto,
            self.protos.queue_proto,
            self.protos.buffer_proto,
            self.protos.texture_proto,
            self.protos.texture_view_proto,
            self.protos.sampler_proto,
            self.protos.shader_module_proto,
            self.protos.bind_group_layout_proto,
            self.protos.pipeline_layout_proto,
            self.protos.bind_group_proto,
            self.protos.render_pipeline_proto,
            self.protos.compute_pipeline_proto,
            self.protos.command_encoder_proto,
            self.protos.render_pass_proto,
            self.protos.compute_pass_proto,
            self.protos.command_buffer_proto,
            self.protos.canvas_ctx_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for dev in self.devices.values() {
            out.push(JsValue::from_object(dev.queue_handle));
        }
        for buf in self.buffers.values() {
            for s in &buf.mapped_slices {
                out.push(JsValue::from_object(s.array_buffer));
            }
        }
        for &h in self.canvas_node_to_ctx_handle.values() {
            out.push(JsValue::from_object(h));
        }
    }

    /// Sweeps native GPU resources whose JS wrappers were collected by Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.adapters.retain(|&h, _| heap.is_alive(h));
        self.devices.retain(|&h, _| heap.is_alive(h));
        self.queues.retain(|&h, _| heap.is_alive(h));
        self.buffers.retain(|&h, _| heap.is_alive(h));
        self.textures.retain(|&h, _| heap.is_alive(h));
        self.texture_views.retain(|&h, _| heap.is_alive(h));
        self.samplers.retain(|&h, _| heap.is_alive(h));
        self.shader_modules.retain(|&h, _| heap.is_alive(h));
        self.bind_group_layouts.retain(|&h, _| heap.is_alive(h));
        self.pipeline_layouts.retain(|&h, _| heap.is_alive(h));
        self.bind_groups.retain(|&h, _| heap.is_alive(h));
        self.render_pipelines.retain(|&h, _| heap.is_alive(h));
        self.compute_pipelines.retain(|&h, _| heap.is_alive(h));
        self.command_encoders.retain(|&h, _| heap.is_alive(h));
        self.render_passes.retain(|&h, _| heap.is_alive(h));
        self.compute_passes.retain(|&h, _| heap.is_alive(h));
        self.command_buffers.retain(|&h, _| heap.is_alive(h));
        self.canvas_ctx_handle_to_node
            .retain(|&h, _| heap.is_alive(h));
        self.canvas_node_to_ctx_handle
            .retain(|_, &mut h| heap.is_alive(h));
    }
}

/// Installs `navigator.gpu`, WebGPU usage/stage flag namespaces, WebGPU prototypes, and
/// `canvas.getContext("webgpu")` hooks onto `vm`.
pub fn install_webgpu(vm: &mut JsVm, host: &SharedHost, canvas2d: &SharedCanvas2D) -> SharedWebGpu {
    let sys = Rc::new(RefCell::new(WebGpuSystem::new()));
    install_webgpu_constants(vm);
    install_webgpu_prototypes(vm, host, &sys);
    attach_navigator_gpu(vm, &sys);
    register_canvas_webgpu_hooks(host, canvas2d, &sys);
    sys
}

// ---------------------------------------------------------------------------
// Object allocation helpers
// ---------------------------------------------------------------------------

fn req_obj(vm: &mut JsVm, val: JsValue, msg: &str) -> Result<HeapHandle, JsException> {
    val.as_object().ok_or_else(|| vm.type_error(msg))
}

fn opt_u64(vm: &mut JsVm, args: &[JsValue], idx: usize) -> Result<Option<u64>, JsException> {
    let v = arg(args, idx);
    if v.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(vm.to_number(v)?.max(0.0) as u64))
    }
}

fn opt_u32(vm: &mut JsVm, args: &[JsValue], idx: usize) -> Result<Option<u32>, JsException> {
    let v = arg(args, idx);
    if v.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(vm.to_u32(v)?))
    }
}

fn wrap_texture(vm: &mut JsVm, sys: &SharedWebGpu, entry: GpuTextureEntry) -> HeapHandle {
    let proto = sys.borrow().protos.texture_proto;
    let obj = alloc_with_proto(vm, proto);
    set_num_prop(vm, obj, "width", f64::from(entry.width));
    set_num_prop(vm, obj, "height", f64::from(entry.height));
    set_num_prop(
        vm,
        obj,
        "depthOrArrayLayers",
        f64::from(entry.depth_or_array_layers),
    );
    set_num_prop(vm, obj, "mipLevelCount", f64::from(entry.mip_level_count));
    set_num_prop(vm, obj, "sampleCount", f64::from(entry.sample_count));
    set_str_prop(vm, obj, "dimension", &entry.dimension);
    set_str_prop(vm, obj, "format", &entry.format);
    set_num_prop(vm, obj, "usage", f64::from(entry.usage));
    sys.borrow_mut().textures.insert(obj, entry);
    obj
}

fn require_device(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    this_val: JsValue,
) -> Result<GpuDeviceEntry, JsException> {
    let h = req_obj(vm, this_val, "Incompatible GPUDevice receiver")?;
    sys.borrow()
        .devices
        .get(&h)
        .cloned()
        .ok_or_else(|| vm.type_error("Incompatible GPUDevice receiver"))
}

fn require_queue_dev(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    this_val: JsValue,
) -> Result<GpuDeviceEntry, JsException> {
    let qh = req_obj(vm, this_val, "Incompatible GPUQueue receiver")?;
    let st = sys.borrow();
    let dh = *st
        .queues
        .get(&qh)
        .ok_or_else(|| vm.type_error("Incompatible GPUQueue receiver"))?;
    st.devices
        .get(&dh)
        .cloned()
        .ok_or_else(|| vm.type_error("Owning GPUDevice not found"))
}

fn require_canvas_ctx_node(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let ch = req_obj(vm, this_val, "Incompatible GPUCanvasContext receiver")?;
    sys.borrow()
        .canvas_ctx_handle_to_node
        .get(&ch)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible GPUCanvasContext receiver"))
}

struct ParsedTexCopy {
    texture: wgpu::Texture,
    mip_level: u32,
    origin: wgpu::Origin3d,
    aspect: wgpu::TextureAspect,
    canvas_node: Option<NodeId>,
}

impl ParsedTexCopy {
    fn as_info(&self) -> wgpu::TexelCopyTextureInfo<'_> {
        wgpu::TexelCopyTextureInfo {
            texture: &self.texture,
            mip_level: self.mip_level,
            origin: self.origin,
            aspect: self.aspect,
        }
    }
}

fn parse_tex_copy_target(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    val: JsValue,
) -> Result<ParsedTexCopy, JsException> {
    let obj = req_obj(vm, val, "Texture copy target object required")?;
    let tex_h = js_prop(vm, obj, "texture")
        .and_then(JsValue::as_object)
        .ok_or_else(|| vm.type_error("Texture copy target requires .texture"))?;
    let mip_level = js_prop_u32(vm, obj, "mipLevel").unwrap_or(0);
    let origin = js_prop(vm, obj, "origin")
        .map(|v| parse_origin_3d(vm, v))
        .unwrap_or(wgpu::Origin3d::ZERO);
    let aspect = js_prop_str(vm, obj, "aspect")
        .as_deref()
        .map_or(wgpu::TextureAspect::All, parse_texture_aspect);
    let st = sys.borrow();
    let te = st
        .textures
        .get(&tex_h)
        .ok_or_else(|| vm.type_error("Invalid GPUTexture"))?;
    Ok(ParsedTexCopy {
        texture: te.texture.clone(),
        mip_level,
        origin,
        aspect,
        canvas_node: te.canvas_node,
    })
}

// ---------------------------------------------------------------------------
// Prototype installation
// ---------------------------------------------------------------------------

fn install_webgpu_prototypes(vm: &mut JsVm, host: &SharedHost, sys: &SharedWebGpu) {
    let gpu_p = alloc_proto(vm);
    let adapter_p = alloc_proto(vm);
    let device_p = alloc_proto(vm);
    let queue_p = alloc_proto(vm);
    let buffer_p = alloc_proto(vm);
    let texture_p = alloc_proto(vm);
    let view_p = alloc_proto(vm);
    let sampler_p = alloc_proto(vm);
    let shader_p = alloc_proto(vm);
    let bgl_p = alloc_proto(vm);
    let pl_p = alloc_proto(vm);
    let bg_p = alloc_proto(vm);
    let rp_p = alloc_proto(vm);
    let cp_p = alloc_proto(vm);
    let enc_p = alloc_proto(vm);
    let rpass_p = alloc_proto(vm);
    let cpass_p = alloc_proto(vm);
    let cbuf_p = alloc_proto(vm);
    let cctx_p = alloc_proto(vm);

    {
        let mut st = sys.borrow_mut();
        st.protos = WebGpuPrototypes {
            gpu_proto: Some(gpu_p),
            adapter_proto: Some(adapter_p),
            device_proto: Some(device_p),
            queue_proto: Some(queue_p),
            buffer_proto: Some(buffer_p),
            texture_proto: Some(texture_p),
            texture_view_proto: Some(view_p),
            sampler_proto: Some(sampler_p),
            shader_module_proto: Some(shader_p),
            bind_group_layout_proto: Some(bgl_p),
            pipeline_layout_proto: Some(pl_p),
            bind_group_proto: Some(bg_p),
            render_pipeline_proto: Some(rp_p),
            compute_pipeline_proto: Some(cp_p),
            command_encoder_proto: Some(enc_p),
            render_pass_proto: Some(rpass_p),
            compute_pass_proto: Some(cpass_p),
            command_buffer_proto: Some(cbuf_p),
            canvas_ctx_proto: Some(cctx_p),
        };
    }

    install_gpu_and_adapter_methods(vm, sys, gpu_p, adapter_p);
    install_device_methods(vm, sys, device_p);
    install_queue_methods(vm, sys, queue_p);
    install_buffer_and_texture_methods(vm, sys, buffer_p, texture_p, shader_p, rp_p, cp_p);
    install_encoder_and_pass_methods(vm, sys, enc_p, rpass_p, cpass_p);
    install_canvas_context_methods(vm, host, sys, cctx_p);

    for (name, proto) in [
        ("GPU", gpu_p),
        ("GPUAdapter", adapter_p),
        ("GPUDevice", device_p),
        ("GPUQueue", queue_p),
        ("GPUBuffer", buffer_p),
        ("GPUTexture", texture_p),
        ("GPUTextureView", view_p),
        ("GPUSampler", sampler_p),
        ("GPUShaderModule", shader_p),
        ("GPUBindGroupLayout", bgl_p),
        ("GPUPipelineLayout", pl_p),
        ("GPUBindGroup", bg_p),
        ("GPURenderPipeline", rp_p),
        ("GPUComputePipeline", cp_p),
        ("GPUCommandEncoder", enc_p),
        ("GPURenderPassEncoder", rpass_p),
        ("GPUComputePassEncoder", cpass_p),
        ("GPUCommandBuffer", cbuf_p),
        ("GPUCanvasContext", cctx_p),
    ] {
        let ctor = vm.alloc_host_fn(name, move |vm, _, _| {
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        });
        bind_ctor_proto(vm, ctor, proto);
        define_global(vm, name, JsValue::from_object(ctor));
    }
}

fn install_gpu_and_adapter_methods(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    gpu_p: HeapHandle,
    adapter_p: HeapHandle,
) {
    // GPU.prototype.getPreferredCanvasFormat() -> "rgba8unorm"
    define_host_method(vm, gpu_p, "getPreferredCanvasFormat", |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("rgba8unorm")))
    });

    // GPU.prototype.requestAdapter(options?) -> Promise<GPUAdapter | null>
    let s = Rc::clone(sys);
    define_host_method(vm, gpu_p, "requestAdapter", move |vm, _, args| {
        let p = alloc_promise(vm);
        let (power_pref, force_fallback) = if let Some(opts) = arg(args, 0).as_object() {
            let pp = match js_prop_str(vm, opts, "powerPreference").as_deref() {
                Some("low-power") => wgpu::PowerPreference::LowPower,
                Some("high-performance") => wgpu::PowerPreference::HighPerformance,
                _ => wgpu::PowerPreference::None,
            };
            let fb = js_prop_bool(vm, opts, "forceFallbackAdapter").unwrap_or(false);
            (pp, fb)
        } else {
            (wgpu::PowerPreference::None, false)
        };

        let shared_opt = if !force_fallback {
            s.borrow().shared_gpu.clone()
        } else {
            None
        };

        let adapter_res = if let Some(ref shared) = shared_opt {
            Ok(shared.adapter.clone())
        } else {
            request_headless_adapter(power_pref, force_fallback)
        };

        match adapter_res {
            Ok(adapter) => {
                let info = adapter.get_info();
                let limits = adapter.limits();
                let is_fallback = info.device_type == wgpu::DeviceType::Cpu;
                let proto = s.borrow().protos.adapter_proto;
                let obj = alloc_with_proto(vm, proto);
                let info_obj = vm.alloc_ordinary_object();
                set_str_prop(vm, info_obj, "vendor", &format!("0x{:x}", info.vendor));
                set_str_prop(vm, info_obj, "architecture", &format!("{:?}", info.backend));
                set_str_prop(vm, info_obj, "device", &info.name);
                set_str_prop(vm, info_obj, "description", &info.driver_info);
                let k_info = vm.heap.intern_key("info");
                vm.heap.object_define_data(
                    obj,
                    k_info,
                    JsValue::from_object(info_obj),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                let limits_obj = build_limits_object(vm, &limits);
                let k_lim = vm.heap.intern_key("limits");
                vm.heap.object_define_data(
                    obj,
                    k_lim,
                    JsValue::from_object(limits_obj),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                let k_fb = vm.heap.intern_key("isFallbackAdapter");
                vm.heap.object_define_data(
                    obj,
                    k_fb,
                    JsValue::from_bool(is_fallback),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                let features_obj = vm.alloc_ordinary_object();
                let k_feat = vm.heap.intern_key("features");
                vm.heap.object_define_data(
                    obj,
                    k_feat,
                    JsValue::from_object(features_obj),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                s.borrow_mut().adapters.insert(
                    obj,
                    GpuAdapterEntry {
                        adapter,
                        shared_override: shared_opt,
                    },
                );
                resolve_promise(vm, p, JsValue::from_object(obj));
            }
            Err(_) => {
                resolve_promise(vm, p, JsValue::NULL);
            }
        }
        Ok(JsValue::from_object(p))
    });

    // GPUAdapter.prototype.requestDevice(descriptor?) -> Promise<GPUDevice>
    let s = Rc::clone(sys);
    define_host_method(vm, adapter_p, "requestDevice", move |vm, this_val, args| {
        let p = alloc_promise(vm);
        let Some(ah) = this_val.as_object() else {
            let err = create_dom_exception(vm, "TypeError", "Incompatible GPUAdapter receiver");
            reject_promise(vm, p, err.value);
            return Ok(JsValue::from_object(p));
        };
        let label = arg(args, 0)
            .as_object()
            .and_then(|d| js_prop_str(vm, d, "label"));

        let (adapter, shared_override) = {
            let st = s.borrow();
            let Some(entry) = st.adapters.get(&ah) else {
                let err = create_dom_exception(vm, "TypeError", "Incompatible GPUAdapter receiver");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            (entry.adapter.clone(), entry.shared_override.clone())
        };

        let dev_res = if let Some(shared) = shared_override {
            Ok((shared.device, shared.queue, shared.renderer_token))
        } else {
            request_device_from_adapter(
                &adapter,
                label.as_deref(),
                wgpu::Features::empty(),
                adapter.limits(),
            )
            .map(|(d, q)| (d, q, 0u64))
        };

        match dev_res {
            Ok((device, queue, renderer_token)) => {
                device.on_uncaptured_error(Arc::new(|err| {
                    tracing::warn!(target: "rix_webgpu", %err, "uncaptured WebGPU error");
                }));

                let (dev_proto, queue_proto) = {
                    let st = s.borrow();
                    (st.protos.device_proto, st.protos.queue_proto)
                };
                let dev_obj = alloc_with_proto(vm, dev_proto);
                let queue_obj = alloc_with_proto(vm, queue_proto);
                let limits_obj = build_limits_object(vm, &device.limits());
                let lost_p = alloc_promise(vm);

                let k_q = vm.heap.intern_key("queue");
                vm.heap.object_define_data(
                    dev_obj,
                    k_q,
                    JsValue::from_object(queue_obj),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                let k_lim = vm.heap.intern_key("limits");
                vm.heap.object_define_data(
                    dev_obj,
                    k_lim,
                    JsValue::from_object(limits_obj),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                let k_lost = vm.heap.intern_key("lost");
                vm.heap.object_define_data(
                    dev_obj,
                    k_lost,
                    JsValue::from_object(lost_p),
                    PropertyAttrs::READONLY,
                    &mut vm.shapes,
                );
                set_str_prop(vm, dev_obj, "label", label.as_deref().unwrap_or(""));

                {
                    let mut st = s.borrow_mut();
                    st.devices.insert(
                        dev_obj,
                        GpuDeviceEntry {
                            device,
                            queue,
                            renderer_token,
                            queue_handle: queue_obj,
                            error_scopes: Rc::new(RefCell::new(Vec::new())),
                        },
                    );
                    st.queues.insert(queue_obj, dev_obj);
                }
                resolve_promise(vm, p, JsValue::from_object(dev_obj));
            }
            Err(err) => {
                let dom_err = create_dom_exception(vm, "OperationError", &err.to_string());
                reject_promise(vm, p, dom_err.value);
            }
        }
        Ok(JsValue::from_object(p))
    });
}

fn install_device_methods(vm: &mut JsVm, sys: &SharedWebGpu, device_p: HeapHandle) {
    // GPUDevice.prototype.destroy()
    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "destroy", move |vm, this_val, _| {
        let dev = require_device(vm, &s, this_val)?;
        dev.device.destroy();
        Ok(JsValue::UNDEFINED)
    });

    // GPUDevice.prototype.pushErrorScope(filter) / popErrorScope()
    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "pushErrorScope", move |vm, this_val, args| {
        let dev = require_device(vm, &s, this_val)?;
        let filter = match vm.value_to_rust_string(arg(args, 0)).as_str() {
            "out-of-memory" => wgpu::ErrorFilter::OutOfMemory,
            "internal" => wgpu::ErrorFilter::Internal,
            _ => wgpu::ErrorFilter::Validation,
        };
        let guard = dev.device.push_error_scope(filter);
        dev.error_scopes.borrow_mut().push(guard);
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "popErrorScope", move |vm, this_val, _| {
        let dev = require_device(vm, &s, this_val)?;
        let p = alloc_promise(vm);
        let guard = dev.error_scopes.borrow_mut().pop();
        let err_opt = guard.and_then(|g| {
            let scope = g.pop();
            let _ = dev.device.poll(wgpu::PollType::Poll);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let mut fut = std::pin::pin!(scope);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => v,
                std::task::Poll::Pending => None,
            }
        });
        match err_opt {
            Some(err) => {
                let eo = vm.alloc_ordinary_object();
                set_str_prop(vm, eo, "message", &err.to_string());
                resolve_promise(vm, p, JsValue::from_object(eo));
            }
            None => resolve_promise(vm, p, JsValue::NULL),
        }
        Ok(JsValue::from_object(p))
    });

    // GPUDevice.prototype.createBuffer(descriptor)
    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "createBuffer", move |vm, this_val, args| {
        let dev = require_device(vm, &s, this_val)?;
        let desc_h = req_obj(
            vm,
            arg(args, 0),
            "createBuffer requires a descriptor object",
        )?;
        let size = js_prop_u64(vm, desc_h, "size").unwrap_or(0);
        let usage_bits = js_prop_u32(vm, desc_h, "usage").unwrap_or(0);
        let mapped_at_creation = js_prop_bool(vm, desc_h, "mappedAtCreation").unwrap_or(false);
        let label = js_prop_str(vm, desc_h, "label");
        let aligned_size = if size == 0 { 4 } else { size.div_ceil(4) * 4 };
        let buffer = dev.device.create_buffer(&wgpu::BufferDescriptor {
            label: label.as_deref(),
            size: aligned_size,
            usage: parse_buffer_usages(usage_bits),
            mapped_at_creation,
        });
        let proto = s.borrow().protos.buffer_proto;
        let obj = alloc_with_proto(vm, proto);
        set_num_prop(vm, obj, "size", aligned_size as f64);
        set_num_prop(vm, obj, "usage", f64::from(usage_bits));
        set_str_prop(vm, obj, "label", label.as_deref().unwrap_or(""));
        let map_state = if mapped_at_creation {
            BufferMapState::MappedAtCreation
        } else {
            BufferMapState::Unmapped
        };
        s.borrow_mut().buffers.insert(
            obj,
            GpuBufferEntry {
                device: dev.device,
                buffer,
                size: aligned_size,
                usage: usage_bits,
                map_state,
                mapped_range_start: 0,
                mapped_range_size: if mapped_at_creation { aligned_size } else { 0 },
                mapped_read_bytes: None,
                mapped_slices: Vec::new(),
            },
        );
        Ok(JsValue::from_object(obj))
    });

    // GPUDevice.prototype.createTexture(descriptor)
    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "createTexture", move |vm, this_val, args| {
        let dev = require_device(vm, &s, this_val)?;
        let desc_h = req_obj(
            vm,
            arg(args, 0),
            "createTexture requires a descriptor object",
        )?;
        let label = js_prop_str(vm, desc_h, "label");
        let size = js_prop(vm, desc_h, "size")
            .map(|v| parse_extent_3d(vm, v))
            .unwrap_or(wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            });
        let format_str =
            js_prop_str(vm, desc_h, "format").unwrap_or_else(|| "rgba8unorm".to_string());
        let format = parse_texture_format(&format_str)
            .ok_or_else(|| vm.type_error(format!("Unsupported GPUTextureFormat: {format_str}")))?;
        let usage_bits = js_prop_u32(vm, desc_h, "usage").unwrap_or(0);
        let mip_level_count = js_prop_u32(vm, desc_h, "mipLevelCount").unwrap_or(1).max(1);
        let sample_count = js_prop_u32(vm, desc_h, "sampleCount").unwrap_or(1).max(1);
        let dim_str = js_prop_str(vm, desc_h, "dimension").unwrap_or_else(|| "2d".to_string());
        let dimension = match dim_str.as_str() {
            "1d" => wgpu::TextureDimension::D1,
            "3d" => wgpu::TextureDimension::D3,
            _ => wgpu::TextureDimension::D2,
        };
        let texture = dev.device.create_texture(&wgpu::TextureDescriptor {
            label: label.as_deref(),
            size,
            mip_level_count,
            sample_count,
            dimension,
            format,
            usage: parse_texture_usages(usage_bits),
            view_formats: &[],
        });
        let obj = wrap_texture(
            vm,
            &s,
            GpuTextureEntry {
                texture,
                width: size.width,
                height: size.height,
                depth_or_array_layers: size.depth_or_array_layers,
                mip_level_count,
                sample_count,
                dimension: dim_str,
                format: format_str,
                usage: usage_bits,
                canvas_node: None,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    // GPUDevice.prototype.createSampler(descriptor?)
    let s = Rc::clone(sys);
    define_host_method(vm, device_p, "createSampler", move |vm, this_val, args| {
        let dev = require_device(vm, &s, this_val)?;
        let mut desc = wgpu::SamplerDescriptor {
            label: None,
            ..Default::default()
        };
        let label_owned = arg(args, 0)
            .as_object()
            .and_then(|h| js_prop_str(vm, h, "label"));
        desc.label = label_owned.as_deref();
        if let Some(h) = arg(args, 0).as_object() {
            if let Some(v) = js_prop_str(vm, h, "addressModeU") {
                desc.address_mode_u = parse_address_mode(&v);
            }
            if let Some(v) = js_prop_str(vm, h, "addressModeV") {
                desc.address_mode_v = parse_address_mode(&v);
            }
            if let Some(v) = js_prop_str(vm, h, "addressModeW") {
                desc.address_mode_w = parse_address_mode(&v);
            }
            if let Some(v) = js_prop_str(vm, h, "magFilter") {
                desc.mag_filter = parse_filter_mode(&v);
            }
            if let Some(v) = js_prop_str(vm, h, "minFilter") {
                desc.min_filter = parse_filter_mode(&v);
            }
            if let Some(v) = js_prop_str(vm, h, "mipmapFilter") {
                desc.mipmap_filter = parse_mipmap_filter_mode(&v);
            }
            if let Some(v) = js_prop_f64(vm, h, "lodMinClamp") {
                desc.lod_min_clamp = v as f32;
            }
            if let Some(v) = js_prop_f64(vm, h, "lodMaxClamp") {
                desc.lod_max_clamp = v as f32;
            }
            if let Some(v) = js_prop_str(vm, h, "compare") {
                desc.compare = Some(parse_compare_function(&v));
            }
            if let Some(v) = js_prop_u32(vm, h, "maxAnisotropy") {
                desc.anisotropy_clamp = (v.clamp(1, 16)) as u16;
            }
        }
        let sampler = dev.device.create_sampler(&desc);
        let proto = s.borrow().protos.sampler_proto;
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().samplers.insert(obj, sampler);
        Ok(JsValue::from_object(obj))
    });

    // GPUDevice.prototype.createShaderModule({ code, label? })
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createShaderModule",
        move |vm, this_val, args| {
            let dev = require_device(vm, &s, this_val)?;
            let desc_h = req_obj(
                vm,
                arg(args, 0),
                "createShaderModule requires a descriptor object",
            )?;
            let code = js_prop_str(vm, desc_h, "code")
                .ok_or_else(|| vm.type_error("createShaderModule requires a WGSL 'code' string"))?;
            let label = js_prop_str(vm, desc_h, "label");
            let scope_guard = dev.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let module = dev
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: label.as_deref(),
                    source: wgpu::ShaderSource::Wgsl(code.into()),
                });
            let scope = scope_guard.pop();
            let mut compilation_errors = Vec::new();
            let _ = dev.device.poll(wgpu::PollType::Poll);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let mut fut = std::pin::pin!(scope);
            if let std::task::Poll::Ready(Some(err)) = fut.as_mut().poll(&mut cx) {
                compilation_errors.push(err.to_string());
            }
            let proto = s.borrow().protos.shader_module_proto;
            let obj = alloc_with_proto(vm, proto);
            set_str_prop(vm, obj, "label", label.as_deref().unwrap_or(""));
            s.borrow_mut().shader_modules.insert(
                obj,
                GpuShaderModuleEntry {
                    module,
                    compilation_errors,
                },
            );
            Ok(JsValue::from_object(obj))
        },
    );

    // GPUDevice.prototype.createBindGroupLayout(descriptor)
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createBindGroupLayout",
        move |vm, this_val, args| {
            let dev = require_device(vm, &s, this_val)?;
            let desc_h = req_obj(
                vm,
                arg(args, 0),
                "createBindGroupLayout requires a descriptor",
            )?;
            let label = js_prop_str(vm, desc_h, "label");
            let entries_val = js_prop(vm, desc_h, "entries").unwrap_or(JsValue::UNDEFINED);
            let mut entries = Vec::new();
            for item in js_array_elems(vm, entries_val) {
                let Some(eh) = item.as_object() else { continue };
                let binding = js_prop_u32(vm, eh, "binding").unwrap_or(0);
                let visibility =
                    parse_shader_stages(js_prop_u32(vm, eh, "visibility").unwrap_or(0));
                let ty = if let Some(bh) = js_prop(vm, eh, "buffer").and_then(JsValue::as_object) {
                    let bty = match js_prop_str(vm, bh, "type").as_deref() {
                        Some("storage") => wgpu::BufferBindingType::Storage { read_only: false },
                        Some("read-only-storage") => {
                            wgpu::BufferBindingType::Storage { read_only: true }
                        }
                        _ => wgpu::BufferBindingType::Uniform,
                    };
                    let has_dynamic_offset =
                        js_prop_bool(vm, bh, "hasDynamicOffset").unwrap_or(false);
                    let min_binding_size =
                        js_prop_u64(vm, bh, "minBindingSize").and_then(NonZeroU64::new);
                    wgpu::BindingType::Buffer {
                        ty: bty,
                        has_dynamic_offset,
                        min_binding_size,
                    }
                } else if let Some(sh) = js_prop(vm, eh, "sampler").and_then(JsValue::as_object) {
                    let sty = match js_prop_str(vm, sh, "type").as_deref() {
                        Some("non-filtering") => wgpu::SamplerBindingType::NonFiltering,
                        Some("comparison") => wgpu::SamplerBindingType::Comparison,
                        _ => wgpu::SamplerBindingType::Filtering,
                    };
                    wgpu::BindingType::Sampler(sty)
                } else if let Some(th) = js_prop(vm, eh, "texture").and_then(JsValue::as_object) {
                    let sample_type = match js_prop_str(vm, th, "sampleType").as_deref() {
                        Some("unfilterable-float") => {
                            wgpu::TextureSampleType::Float { filterable: false }
                        }
                        Some("depth") => wgpu::TextureSampleType::Depth,
                        Some("sint") => wgpu::TextureSampleType::Sint,
                        Some("uint") => wgpu::TextureSampleType::Uint,
                        _ => wgpu::TextureSampleType::Float { filterable: true },
                    };
                    let view_dimension = js_prop_str(vm, th, "viewDimension")
                        .as_deref()
                        .map_or(wgpu::TextureViewDimension::D2, parse_texture_view_dimension);
                    let multisampled = js_prop_bool(vm, th, "multisampled").unwrap_or(false);
                    wgpu::BindingType::Texture {
                        sample_type,
                        view_dimension,
                        multisampled,
                    }
                } else if let Some(sth) =
                    js_prop(vm, eh, "storageTexture").and_then(JsValue::as_object)
                {
                    let access = match js_prop_str(vm, sth, "access").as_deref() {
                        Some("read-only") => wgpu::StorageTextureAccess::ReadOnly,
                        Some("read-write") => wgpu::StorageTextureAccess::ReadWrite,
                        _ => wgpu::StorageTextureAccess::WriteOnly,
                    };
                    let format = js_prop_str(vm, sth, "format")
                        .as_deref()
                        .and_then(parse_texture_format)
                        .unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
                    let view_dimension = js_prop_str(vm, sth, "viewDimension")
                        .as_deref()
                        .map_or(wgpu::TextureViewDimension::D2, parse_texture_view_dimension);
                    wgpu::BindingType::StorageTexture {
                        access,
                        format,
                        view_dimension,
                    }
                } else {
                    continue;
                };
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility,
                    ty,
                    count: None,
                });
            }
            let bgl = dev
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: label.as_deref(),
                    entries: &entries,
                });
            let proto = s.borrow().protos.bind_group_layout_proto;
            let obj = alloc_with_proto(vm, proto);
            s.borrow_mut().bind_group_layouts.insert(obj, bgl);
            Ok(JsValue::from_object(obj))
        },
    );

    // GPUDevice.prototype.createPipelineLayout(descriptor)
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createPipelineLayout",
        move |vm, this_val, args| {
            let dev = require_device(vm, &s, this_val)?;
            let desc_h = req_obj(
                vm,
                arg(args, 0),
                "createPipelineLayout requires a descriptor",
            )?;
            let label = js_prop_str(vm, desc_h, "label");
            let bgls_val = js_prop(vm, desc_h, "bindGroupLayouts").unwrap_or(JsValue::UNDEFINED);
            let bgl_handles = js_array_elems(vm, bgls_val);
            let bgls_owned: Vec<Option<wgpu::BindGroupLayout>> = {
                let st = s.borrow();
                bgl_handles
                    .iter()
                    .map(|v| {
                        v.as_object()
                            .and_then(|h| st.bind_group_layouts.get(&h).cloned())
                    })
                    .collect()
            };
            let bgl_refs: Vec<Option<&wgpu::BindGroupLayout>> =
                bgls_owned.iter().map(|o| o.as_ref()).collect();
            let pl = dev
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: label.as_deref(),
                    bind_group_layouts: &bgl_refs,
                    immediate_size: 0,
                });
            let proto = s.borrow().protos.pipeline_layout_proto;
            let obj = alloc_with_proto(vm, proto);
            s.borrow_mut().pipeline_layouts.insert(obj, pl);
            Ok(JsValue::from_object(obj))
        },
    );

    // GPUDevice.prototype.createBindGroup(descriptor)
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createBindGroup",
        move |vm, this_val, args| {
            let dev = require_device(vm, &s, this_val)?;
            let desc_h = req_obj(vm, arg(args, 0), "createBindGroup requires a descriptor")?;
            let label = js_prop_str(vm, desc_h, "label");
            let layout_h = js_prop(vm, desc_h, "layout")
                .and_then(JsValue::as_object)
                .ok_or_else(|| vm.type_error("createBindGroup requires a valid layout"))?;
            let layout = s
                .borrow()
                .bind_group_layouts
                .get(&layout_h)
                .cloned()
                .ok_or_else(|| vm.type_error("Invalid GPUBindGroupLayout"))?;

            enum OwnedRes {
                Buffer(wgpu::Buffer, u64, Option<NonZeroU64>),
                Sampler(wgpu::Sampler),
                TextureView(wgpu::TextureView),
            }

            let entries_val = js_prop(vm, desc_h, "entries").unwrap_or(JsValue::UNDEFINED);
            let mut owned: Vec<(u32, OwnedRes)> = Vec::new();
            for item in js_array_elems(vm, entries_val) {
                let Some(eh) = item.as_object() else { continue };
                let binding = js_prop_u32(vm, eh, "binding").unwrap_or(0);
                let Some(res_h) = js_prop(vm, eh, "resource").and_then(JsValue::as_object) else {
                    continue;
                };
                if let Some(sampler) = s.borrow().samplers.get(&res_h).cloned() {
                    owned.push((binding, OwnedRes::Sampler(sampler)));
                } else if let Some(tv) = s.borrow().texture_views.get(&res_h) {
                    owned.push((binding, OwnedRes::TextureView(tv.view.clone())));
                } else if let Some(buf_h) =
                    js_prop(vm, res_h, "buffer").and_then(JsValue::as_object)
                {
                    let offset = js_prop_u64(vm, res_h, "offset").unwrap_or(0);
                    let size = js_prop_u64(vm, res_h, "size").and_then(NonZeroU64::new);
                    if let Some(b) = s.borrow().buffers.get(&buf_h) {
                        owned.push((binding, OwnedRes::Buffer(b.buffer.clone(), offset, size)));
                    }
                }
            }
            let bg_entries: Vec<wgpu::BindGroupEntry<'_>> = owned
                .iter()
                .map(|(binding, res)| wgpu::BindGroupEntry {
                    binding: *binding,
                    resource: match res {
                        OwnedRes::Buffer(b, offset, size) => {
                            wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: b,
                                offset: *offset,
                                size: *size,
                            })
                        }
                        OwnedRes::Sampler(samp) => wgpu::BindingResource::Sampler(samp),
                        OwnedRes::TextureView(tv) => wgpu::BindingResource::TextureView(tv),
                    },
                })
                .collect();
            let bg = dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: label.as_deref(),
                layout: &layout,
                entries: &bg_entries,
            });
            let proto = s.borrow().protos.bind_group_proto;
            let obj = alloc_with_proto(vm, proto);
            s.borrow_mut().bind_groups.insert(obj, bg);
            Ok(JsValue::from_object(obj))
        },
    );

    // GPUDevice.prototype.createRenderPipeline & createRenderPipelineAsync
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createRenderPipeline",
        move |vm, this_val, args| {
            let obj = create_render_pipeline_impl(vm, &s, this_val, arg(args, 0))?;
            Ok(JsValue::from_object(obj))
        },
    );
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createRenderPipelineAsync",
        move |vm, this_val, args| {
            let p = alloc_promise(vm);
            match create_render_pipeline_impl(vm, &s, this_val, arg(args, 0)) {
                Ok(obj) => resolve_promise(vm, p, JsValue::from_object(obj)),
                Err(e) => reject_promise(vm, p, e.value),
            }
            Ok(JsValue::from_object(p))
        },
    );

    // GPUDevice.prototype.createComputePipeline & createComputePipelineAsync
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createComputePipeline",
        move |vm, this_val, args| {
            let obj = create_compute_pipeline_impl(vm, &s, this_val, arg(args, 0))?;
            Ok(JsValue::from_object(obj))
        },
    );
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createComputePipelineAsync",
        move |vm, this_val, args| {
            let p = alloc_promise(vm);
            match create_compute_pipeline_impl(vm, &s, this_val, arg(args, 0)) {
                Ok(obj) => resolve_promise(vm, p, JsValue::from_object(obj)),
                Err(e) => reject_promise(vm, p, e.value),
            }
            Ok(JsValue::from_object(p))
        },
    );

    // GPUDevice.prototype.createCommandEncoder(descriptor?)
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        device_p,
        "createCommandEncoder",
        move |vm, this_val, args| {
            let dev = require_device(vm, &s, this_val)?;
            let label = arg(args, 0)
                .as_object()
                .and_then(|d| js_prop_str(vm, d, "label"));
            let encoder = dev
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: label.as_deref(),
                });
            let proto = s.borrow().protos.command_encoder_proto;
            let obj = alloc_with_proto(vm, proto);
            s.borrow_mut().command_encoders.insert(
                obj,
                GpuCommandEncoderEntry {
                    encoder: Some(encoder),
                    touched_canvases: Vec::new(),
                },
            );
            Ok(JsValue::from_object(obj))
        },
    );
}

fn create_render_pipeline_impl(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    this_val: JsValue,
    desc_val: JsValue,
) -> Result<HeapHandle, JsException> {
    let dev = require_device(vm, sys, this_val)?;
    let desc_h = req_obj(
        vm,
        desc_val,
        "createRenderPipeline requires a descriptor object",
    )?;
    let label = js_prop_str(vm, desc_h, "label");

    let layout = js_prop(vm, desc_h, "layout")
        .and_then(JsValue::as_object)
        .and_then(|lh| sys.borrow().pipeline_layouts.get(&lh).cloned());

    // Vertex state
    let vert_h = js_prop(vm, desc_h, "vertex")
        .and_then(JsValue::as_object)
        .ok_or_else(|| vm.type_error("createRenderPipeline requires descriptor.vertex"))?;
    let v_mod_h = js_prop(vm, vert_h, "module")
        .and_then(JsValue::as_object)
        .ok_or_else(|| vm.type_error("vertex.module is required"))?;
    let v_module = sys
        .borrow()
        .shader_modules
        .get(&v_mod_h)
        .map(|e| e.module.clone())
        .ok_or_else(|| vm.type_error("Invalid vertex GPUShaderModule"))?;
    let v_entry = js_prop_str(vm, vert_h, "entryPoint");

    let mut owned_attrs: Vec<(u64, wgpu::VertexStepMode, Vec<wgpu::VertexAttribute>)> = Vec::new();
    if let Some(bufs_val) = js_prop(vm, vert_h, "buffers") {
        for buf_item in js_array_elems(vm, bufs_val) {
            let Some(bh) = buf_item.as_object() else {
                continue;
            };
            let stride = js_prop_u64(vm, bh, "arrayStride").unwrap_or(0);
            let step_mode = match js_prop_str(vm, bh, "stepMode").as_deref() {
                Some("instance") => wgpu::VertexStepMode::Instance,
                _ => wgpu::VertexStepMode::Vertex,
            };
            let mut attrs = Vec::new();
            if let Some(av) = js_prop(vm, bh, "attributes") {
                for ai in js_array_elems(vm, av) {
                    let Some(ah) = ai.as_object() else { continue };
                    let fmt_str = js_prop_str(vm, ah, "format").unwrap_or_default();
                    let Some(format) = parse_vertex_format(&fmt_str) else {
                        continue;
                    };
                    let offset = js_prop_u64(vm, ah, "offset").unwrap_or(0);
                    let shader_location = js_prop_u32(vm, ah, "shaderLocation").unwrap_or(0);
                    attrs.push(wgpu::VertexAttribute {
                        format,
                        offset,
                        shader_location,
                    });
                }
            }
            owned_attrs.push((stride, step_mode, attrs));
        }
    }
    let vbuf_layouts: Vec<wgpu::VertexBufferLayout<'_>> = owned_attrs
        .iter()
        .map(|(stride, step_mode, attrs)| wgpu::VertexBufferLayout {
            array_stride: *stride,
            step_mode: *step_mode,
            attributes: attrs.as_slice(),
        })
        .collect();

    // Primitive state
    let mut primitive = wgpu::PrimitiveState::default();
    if let Some(ph) = js_prop(vm, desc_h, "primitive").and_then(JsValue::as_object) {
        if let Some(t) = js_prop_str(vm, ph, "topology") {
            primitive.topology = parse_topology(&t);
        }
        if let Some(sf) = js_prop_str(vm, ph, "stripIndexFormat") {
            primitive.strip_index_format = match sf.as_str() {
                "uint16" => Some(wgpu::IndexFormat::Uint16),
                "uint32" => Some(wgpu::IndexFormat::Uint32),
                _ => None,
            };
        }
        if let Some(ff) = js_prop_str(vm, ph, "frontFace") {
            primitive.front_face = match ff.as_str() {
                "cw" => wgpu::FrontFace::Cw,
                _ => wgpu::FrontFace::Ccw,
            };
        }
        if let Some(cm) = js_prop_str(vm, ph, "cullMode") {
            primitive.cull_mode = match cm.as_str() {
                "front" => Some(wgpu::Face::Front),
                "back" => Some(wgpu::Face::Back),
                _ => None,
            };
        }
    }

    // DepthStencil state
    let depth_stencil = js_prop(vm, desc_h, "depthStencil")
        .and_then(JsValue::as_object)
        .and_then(|dsh| {
            let fmt = js_prop_str(vm, dsh, "format")
                .as_deref()
                .and_then(parse_texture_format)?;
            let depth_write_enabled = js_prop_bool(vm, dsh, "depthWriteEnabled");
            let depth_compare = js_prop_str(vm, dsh, "depthCompare")
                .as_deref()
                .map(parse_compare_function);
            Some(wgpu::DepthStencilState {
                format: fmt,
                depth_write_enabled,
                depth_compare,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            })
        });

    // Multisample state
    let mut multisample = wgpu::MultisampleState::default();
    if let Some(msh) = js_prop(vm, desc_h, "multisample").and_then(JsValue::as_object) {
        if let Some(c) = js_prop_u32(vm, msh, "count") {
            multisample.count = c.max(1);
        }
        if let Some(m) = js_prop_u64(vm, msh, "mask") {
            multisample.mask = m;
        }
        if let Some(a) = js_prop_bool(vm, msh, "alphaToCoverageEnabled") {
            multisample.alpha_to_coverage_enabled = a;
        }
    }

    // Fragment state
    let mut frag_data: Option<(
        wgpu::ShaderModule,
        Option<String>,
        Vec<Option<wgpu::ColorTargetState>>,
    )> = None;
    if let Some(fh) = js_prop(vm, desc_h, "fragment").and_then(JsValue::as_object) {
        let f_mod_h = js_prop(vm, fh, "module")
            .and_then(JsValue::as_object)
            .ok_or_else(|| vm.type_error("fragment.module is required"))?;
        let f_module = sys
            .borrow()
            .shader_modules
            .get(&f_mod_h)
            .map(|e| e.module.clone())
            .ok_or_else(|| vm.type_error("Invalid fragment GPUShaderModule"))?;
        let f_entry = js_prop_str(vm, fh, "entryPoint");
        let mut targets = Vec::new();
        if let Some(tv) = js_prop(vm, fh, "targets") {
            for ti in js_array_elems(vm, tv) {
                let Some(th) = ti.as_object() else {
                    targets.push(None);
                    continue;
                };
                let fmt = js_prop_str(vm, th, "format")
                    .as_deref()
                    .and_then(parse_texture_format)
                    .unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
                let blend = js_prop(vm, th, "blend")
                    .and_then(JsValue::as_object)
                    .map(|bh| wgpu::BlendState {
                        color: js_prop(vm, bh, "color")
                            .map(|v| parse_blend_component(vm, v))
                            .unwrap_or(wgpu::BlendComponent::REPLACE),
                        alpha: js_prop(vm, bh, "alpha")
                            .map(|v| parse_blend_component(vm, v))
                            .unwrap_or(wgpu::BlendComponent::REPLACE),
                    });
                let write_mask = js_prop_u32(vm, th, "writeMask")
                    .map_or(wgpu::ColorWrites::ALL, |bits| {
                        wgpu::ColorWrites::from_bits_truncate(bits)
                    });
                targets.push(Some(wgpu::ColorTargetState {
                    format: fmt,
                    blend,
                    write_mask,
                }));
            }
        }
        frag_data = Some((f_module, f_entry, targets));
    }

    let fragment = frag_data
        .as_ref()
        .map(|(m, ep, targets)| wgpu::FragmentState {
            module: m,
            entry_point: ep.as_deref(),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: targets.as_slice(),
        });

    let pipeline = dev
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: label.as_deref(),
            layout: layout.as_ref(),
            vertex: wgpu::VertexState {
                module: &v_module,
                entry_point: v_entry.as_deref(),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &vbuf_layouts,
            },
            primitive,
            depth_stencil,
            multisample,
            fragment,
            multiview_mask: None,
            cache: None,
        });
    let proto = sys.borrow().protos.render_pipeline_proto;
    let obj = alloc_with_proto(vm, proto);
    sys.borrow_mut().render_pipelines.insert(obj, pipeline);
    Ok(obj)
}

fn create_compute_pipeline_impl(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    this_val: JsValue,
    desc_val: JsValue,
) -> Result<HeapHandle, JsException> {
    let dev = require_device(vm, sys, this_val)?;
    let desc_h = req_obj(
        vm,
        desc_val,
        "createComputePipeline requires a descriptor object",
    )?;
    let label = js_prop_str(vm, desc_h, "label");
    let layout = js_prop(vm, desc_h, "layout")
        .and_then(JsValue::as_object)
        .and_then(|lh| sys.borrow().pipeline_layouts.get(&lh).cloned());
    let comp_h = js_prop(vm, desc_h, "compute")
        .and_then(JsValue::as_object)
        .ok_or_else(|| vm.type_error("createComputePipeline requires descriptor.compute"))?;
    let mod_h = js_prop(vm, comp_h, "module")
        .and_then(JsValue::as_object)
        .ok_or_else(|| vm.type_error("compute.module is required"))?;
    let module = sys
        .borrow()
        .shader_modules
        .get(&mod_h)
        .map(|e| e.module.clone())
        .ok_or_else(|| vm.type_error("Invalid compute GPUShaderModule"))?;
    let entry_point = js_prop_str(vm, comp_h, "entryPoint");

    let pipeline = dev
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: label.as_deref(),
            layout: layout.as_ref(),
            module: &module,
            entry_point: entry_point.as_deref(),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
    let proto = sys.borrow().protos.compute_pipeline_proto;
    let obj = alloc_with_proto(vm, proto);
    sys.borrow_mut().compute_pipelines.insert(obj, pipeline);
    Ok(obj)
}

fn install_queue_methods(vm: &mut JsVm, sys: &SharedWebGpu, queue_p: HeapHandle) {
    let s = Rc::clone(sys);
    define_host_method(vm, queue_p, "submit", move |vm, this_val, args| {
        let dev = require_queue_dev(vm, &s, this_val)?;
        let elems = js_array_elems(vm, arg(args, 0));
        let mut buffers = Vec::with_capacity(elems.len());
        let mut dirty_nodes = Vec::new();
        {
            let mut st = s.borrow_mut();
            for item in elems {
                if let Some(ch) = item.as_object() {
                    if let Some(entry) = st.command_buffers.get_mut(&ch) {
                        if let Some(cb) = entry.buffer.take() {
                            buffers.push(cb);
                        }
                        dirty_nodes.extend(entry.touched_canvases.iter().copied());
                    }
                }
            }
        }
        dev.queue.submit(buffers);
        let mut st = s.borrow_mut();
        for nid in dirty_nodes {
            if let Some(ctx) = st.canvas_contexts.get_mut(&nid) {
                ctx.dirty = true;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, queue_p, "writeBuffer", move |vm, this_val, args| {
        let dev = require_queue_dev(vm, &s, this_val)?;
        let buf_h = req_obj(vm, arg(args, 0), "writeBuffer requires a GPUBuffer")?;
        let buffer_offset = vm.to_number(arg(args, 1))?.max(0.0) as u64;
        let data_offset = opt_u64(vm, args, 3)?.map(|v| v as usize);
        let size = opt_u64(vm, args, 4)?.map(|v| v as usize);
        let bytes = extract_gpu_buffer_source(vm, arg(args, 2), data_offset, size)
            .ok_or_else(|| vm.type_error("writeBuffer data must be a BufferSource"))?;
        let buffer = s
            .borrow()
            .buffers
            .get(&buf_h)
            .map(|b| b.buffer.clone())
            .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
        dev.queue.write_buffer(&buffer, buffer_offset, &bytes);
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, queue_p, "writeTexture", move |vm, this_val, args| {
        let dev = require_queue_dev(vm, &s, this_val)?;
        let dst = parse_tex_copy_target(vm, &s, arg(args, 0))?;
        let bytes = extract_gpu_buffer_source(vm, arg(args, 1), None, None)
            .ok_or_else(|| vm.type_error("writeTexture data must be a BufferSource"))?;
        let layout = arg(args, 2)
            .as_object()
            .map(|lh| parse_texel_copy_buffer_layout(vm, lh))
            .unwrap_or_default();
        let copy_size = parse_extent_3d(vm, arg(args, 3));
        dev.queue
            .write_texture(dst.as_info(), &bytes, layout, copy_size);
        if let Some(nid) = dst.canvas_node {
            if let Some(ctx) = s.borrow_mut().canvas_contexts.get_mut(&nid) {
                ctx.dirty = true;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPUQueue.prototype.onSubmittedWorkDone() -> Promise<undefined>
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        queue_p,
        "onSubmittedWorkDone",
        move |vm, this_val, _| {
            let p = alloc_promise(vm);
            if let Some(qh) = this_val.as_object() {
                let dev_opt = {
                    let st = s.borrow();
                    st.queues
                        .get(&qh)
                        .and_then(|dh| st.devices.get(dh).map(|d| d.device.clone()))
                };
                if let Some(device) = dev_opt {
                    let _ = wait_for_gpu_idle(&device);
                }
            }
            resolve_promise(vm, p, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        },
    );
}

fn install_buffer_and_texture_methods(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    buffer_p: HeapHandle,
    texture_p: HeapHandle,
    shader_p: HeapHandle,
    rp_p: HeapHandle,
    cp_p: HeapHandle,
) {
    // GPUBuffer.prototype.mapState getter
    let s = Rc::clone(sys);
    define_host_getter(vm, buffer_p, "mapState", move |vm, this_val, _| {
        let bh = req_obj(vm, this_val, "Incompatible GPUBuffer receiver")?;
        let state = s
            .borrow()
            .buffers
            .get(&bh)
            .map(|b| b.map_state.as_str())
            .ok_or_else(|| vm.type_error("Incompatible GPUBuffer receiver"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(state)))
    });

    // GPUBuffer.prototype.mapAsync(mode, offset?, size?) -> Promise<undefined>
    let s = Rc::clone(sys);
    define_host_method(vm, buffer_p, "mapAsync", move |vm, this_val, args| {
        let p = alloc_promise(vm);
        let Some(bh) = this_val.as_object() else {
            let err = create_dom_exception(vm, "TypeError", "Incompatible GPUBuffer receiver");
            reject_promise(vm, p, err.value);
            return Ok(JsValue::from_object(p));
        };
        let mode = vm.to_number(arg(args, 0)).unwrap_or(0.0) as u32;
        let offset = if !arg(args, 1).is_undefined() {
            vm.to_number(arg(args, 1)).unwrap_or(0.0).max(0.0) as u64
        } else {
            0
        };
        let req_size = if !arg(args, 2).is_undefined() {
            Some(vm.to_number(arg(args, 2)).unwrap_or(0.0).max(0.0) as u64)
        } else {
            None
        };

        let prep = {
            let st = s.borrow();
            let Some(entry) = st.buffers.get(&bh) else {
                let err = create_dom_exception(vm, "TypeError", "Invalid GPUBuffer");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            let size = req_size.unwrap_or_else(|| entry.size.saturating_sub(offset));
            (
                entry.device.clone(),
                entry.buffer.clone(),
                size,
                entry.usage,
            )
        };
        let (device, buffer, size, usage) = prep;

        if mode == 0x1 {
            if usage & 0x0001 == 0 {
                let err = create_dom_exception(
                    vm,
                    "OperationError",
                    "Buffer does not have GPUBufferUsage.MAP_READ",
                );
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            }
            match readback_wgpu_buffer_range(&device, &buffer, offset, size) {
                Ok(bytes) => {
                    if let Some(entry) = s.borrow_mut().buffers.get_mut(&bh) {
                        entry.map_state = BufferMapState::MappedRead;
                        entry.mapped_range_start = offset;
                        entry.mapped_range_size = size;
                        entry.mapped_read_bytes = Some(bytes);
                        entry.mapped_slices.clear();
                    }
                    resolve_promise(vm, p, JsValue::UNDEFINED);
                }
                Err(e) => {
                    let err = create_dom_exception(vm, "OperationError", &e.to_string());
                    reject_promise(vm, p, err.value);
                }
            }
        } else if mode == 0x2 {
            if usage & 0x0002 == 0 {
                let err = create_dom_exception(
                    vm,
                    "OperationError",
                    "Buffer does not have GPUBufferUsage.MAP_WRITE",
                );
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            }
            if let Some(entry) = s.borrow_mut().buffers.get_mut(&bh) {
                entry.map_state = BufferMapState::MappedWrite;
                entry.mapped_range_start = offset;
                entry.mapped_range_size = size;
                entry.mapped_read_bytes = None;
                entry.mapped_slices.clear();
            }
            resolve_promise(vm, p, JsValue::UNDEFINED);
        } else {
            let err = create_dom_exception(vm, "OperationError", "Invalid GPUMapMode");
            reject_promise(vm, p, err.value);
        }
        Ok(JsValue::from_object(p))
    });

    // GPUBuffer.prototype.getMappedRange(offset?, size?) -> ArrayBuffer
    let s = Rc::clone(sys);
    define_host_method(vm, buffer_p, "getMappedRange", move |vm, this_val, args| {
        let bh = req_obj(vm, this_val, "Incompatible GPUBuffer receiver")?;
        let offset = if !arg(args, 0).is_undefined() {
            vm.to_number(arg(args, 0))?.max(0.0) as u64
        } else {
            s.borrow()
                .buffers
                .get(&bh)
                .map_or(0, |b| b.mapped_range_start)
        };
        let req_size = opt_u64(vm, args, 1)?;
        let (bytes, actual_size) = {
            let st = s.borrow();
            let entry = st
                .buffers
                .get(&bh)
                .ok_or_else(|| vm.type_error("Incompatible GPUBuffer receiver"))?;
            if entry.map_state == BufferMapState::Unmapped {
                return Err(create_dom_exception(
                    vm,
                    "OperationError",
                    "Buffer is not mapped",
                ));
            }
            let mapped_end = entry.mapped_range_start + entry.mapped_range_size;
            let size = req_size.unwrap_or_else(|| mapped_end.saturating_sub(offset));
            if offset < entry.mapped_range_start || offset + size > mapped_end {
                return Err(create_dom_exception(
                    vm,
                    "RangeError",
                    "getMappedRange out of mapped bounds",
                ));
            }
            let slice_bytes = if let Some(ref rd) = entry.mapped_read_bytes {
                let rel = (offset - entry.mapped_range_start) as usize;
                let end = (rel + size as usize).min(rd.len());
                rd[rel.min(end)..end].to_vec()
            } else {
                vec![0u8; size as usize]
            };
            (slice_bytes, size)
        };
        let ab = alloc_array_buffer(vm, bytes);
        if let Some(entry) = s.borrow_mut().buffers.get_mut(&bh) {
            entry.mapped_slices.push(MappedBufferSlice {
                offset,
                size: actual_size,
                array_buffer: ab,
            });
        }
        Ok(JsValue::from_object(ab))
    });

    // GPUBuffer.prototype.unmap()
    let s = Rc::clone(sys);
    define_host_method(vm, buffer_p, "unmap", move |vm, this_val, _| {
        let bh = req_obj(vm, this_val, "Incompatible GPUBuffer receiver")?;
        let (state, device, buffer, slices) = {
            let mut st = s.borrow_mut();
            let entry = st
                .buffers
                .get_mut(&bh)
                .ok_or_else(|| vm.type_error("Incompatible GPUBuffer receiver"))?;
            let state = entry.map_state;
            let slices = std::mem::take(&mut entry.mapped_slices);
            entry.mapped_read_bytes = None;
            entry.map_state = BufferMapState::Unmapped;
            entry.mapped_range_start = 0;
            entry.mapped_range_size = 0;
            (state, entry.device.clone(), entry.buffer.clone(), slices)
        };

        let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
        for sl in slices {
            if let Some(ObjectKind::ArrayBuffer(buf)) =
                vm.heap.get_object(sl.array_buffer).map(|o| &o.kind)
            {
                let len = (sl.size as usize).min(buf.len());
                writes.push((sl.offset, buf[..len].to_vec()));
            }
        }
        match state {
            BufferMapState::MappedAtCreation => {
                for (off, data) in writes {
                    let end = off + data.len() as u64;
                    if end <= buffer.size() && !data.is_empty() {
                        let mut view = buffer.slice(off..end).get_mapped_range_mut();
                        view.copy_from_slice(&data);
                    }
                }
                buffer.unmap();
            }
            BufferMapState::MappedWrite => {
                for (off, data) in writes {
                    if !data.is_empty() {
                        let _ = write_wgpu_buffer_mapped(&device, &buffer, off, &data);
                    }
                }
            }
            BufferMapState::MappedRead | BufferMapState::Unmapped => {}
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPUBuffer.prototype.destroy()
    let s = Rc::clone(sys);
    define_host_method(vm, buffer_p, "destroy", move |vm, this_val, _| {
        let bh = req_obj(vm, this_val, "Incompatible GPUBuffer receiver")?;
        if let Some(entry) = s.borrow_mut().buffers.get_mut(&bh) {
            entry.buffer.destroy();
            entry.map_state = BufferMapState::Unmapped;
            entry.mapped_slices.clear();
            entry.mapped_read_bytes = None;
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPUTexture.prototype.createView(descriptor?)
    let s = Rc::clone(sys);
    define_host_method(vm, texture_p, "createView", move |vm, this_val, args| {
        let th = req_obj(vm, this_val, "Incompatible GPUTexture receiver")?;
        let mut desc = wgpu::TextureViewDescriptor::default();
        let label_owned = arg(args, 0)
            .as_object()
            .and_then(|dh| js_prop_str(vm, dh, "label"));
        desc.label = label_owned.as_deref();
        if let Some(dh) = arg(args, 0).as_object() {
            if let Some(f) = js_prop_str(vm, dh, "format") {
                desc.format = parse_texture_format(&f);
            }
            if let Some(d) = js_prop_str(vm, dh, "dimension") {
                desc.dimension = Some(parse_texture_view_dimension(&d));
            }
            if let Some(a) = js_prop_str(vm, dh, "aspect") {
                desc.aspect = parse_texture_aspect(&a);
            }
            if let Some(bm) = js_prop_u32(vm, dh, "baseMipLevel") {
                desc.base_mip_level = bm;
            }
            if let Some(mc) = js_prop_u32(vm, dh, "mipLevelCount") {
                desc.mip_level_count = Some(mc);
            }
            if let Some(bl) = js_prop_u32(vm, dh, "baseArrayLayer") {
                desc.base_array_layer = bl;
            }
            if let Some(lc) = js_prop_u32(vm, dh, "arrayLayerCount") {
                desc.array_layer_count = Some(lc);
            }
        }
        let (view, canvas_node, proto) = {
            let st = s.borrow();
            let te = st
                .textures
                .get(&th)
                .ok_or_else(|| vm.type_error("Incompatible GPUTexture receiver"))?;
            (
                te.texture.create_view(&desc),
                te.canvas_node,
                st.protos.texture_view_proto,
            )
        };
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut()
            .texture_views
            .insert(obj, GpuTextureViewEntry { view, canvas_node });
        Ok(JsValue::from_object(obj))
    });

    // GPUTexture.prototype.destroy()
    let s = Rc::clone(sys);
    define_host_method(vm, texture_p, "destroy", move |vm, this_val, _| {
        let th = req_obj(vm, this_val, "Incompatible GPUTexture receiver")?;
        if let Some(entry) = s.borrow().textures.get(&th) {
            entry.texture.destroy();
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPUShaderModule.prototype.getCompilationInfo() -> Promise<{ messages: [...] }>
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        shader_p,
        "getCompilationInfo",
        move |vm, this_val, _| {
            let p = alloc_promise(vm);
            let errs = this_val
                .as_object()
                .and_then(|h| {
                    s.borrow()
                        .shader_modules
                        .get(&h)
                        .map(|e| e.compilation_errors.clone())
                })
                .unwrap_or_default();
            let arr = vm.alloc_array();
            for msg in errs {
                let m = vm.alloc_ordinary_object();
                set_str_prop(vm, m, "type", "error");
                set_str_prop(vm, m, "message", &msg);
                set_num_prop(vm, m, "lineNum", 0.0);
                set_num_prop(vm, m, "linePos", 0.0);
                vm.heap.object_push_element(arr, JsValue::from_object(m));
            }
            let info = vm.alloc_ordinary_object();
            let k = vm.heap.intern_key("messages");
            vm.heap.object_define_data(
                info,
                k,
                JsValue::from_object(arr),
                PropertyAttrs::READONLY,
                &mut vm.shapes,
            );
            resolve_promise(vm, p, JsValue::from_object(info));
            Ok(JsValue::from_object(p))
        },
    );

    // GPURenderPipeline.prototype.getBindGroupLayout(index)
    let s = Rc::clone(sys);
    define_host_method(vm, rp_p, "getBindGroupLayout", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPipeline receiver")?;
        let index = vm.to_u32(arg(args, 0))?;
        let (bgl, proto) = {
            let st = s.borrow();
            let pipe = st
                .render_pipelines
                .get(&ph)
                .ok_or_else(|| vm.type_error("Incompatible GPURenderPipeline receiver"))?;
            (
                pipe.get_bind_group_layout(index),
                st.protos.bind_group_layout_proto,
            )
        };
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().bind_group_layouts.insert(obj, bgl);
        Ok(JsValue::from_object(obj))
    });

    // GPUComputePipeline.prototype.getBindGroupLayout(index)
    let s = Rc::clone(sys);
    define_host_method(vm, cp_p, "getBindGroupLayout", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPUComputePipeline receiver")?;
        let index = vm.to_u32(arg(args, 0))?;
        let (bgl, proto) = {
            let st = s.borrow();
            let pipe = st
                .compute_pipelines
                .get(&ph)
                .ok_or_else(|| vm.type_error("Incompatible GPUComputePipeline receiver"))?;
            (
                pipe.get_bind_group_layout(index),
                st.protos.bind_group_layout_proto,
            )
        };
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().bind_group_layouts.insert(obj, bgl);
        Ok(JsValue::from_object(obj))
    });
}

fn install_encoder_and_pass_methods(
    vm: &mut JsVm,
    sys: &SharedWebGpu,
    enc_p: HeapHandle,
    rpass_p: HeapHandle,
    cpass_p: HeapHandle,
) {
    // GPUCommandEncoder.prototype.beginRenderPass(descriptor)
    let s = Rc::clone(sys);
    define_host_method(vm, enc_p, "beginRenderPass", move |vm, this_val, args| {
        let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
        if !s.borrow().command_encoders.contains_key(&eh) {
            return Err(vm.type_error("Incompatible GPUCommandEncoder receiver"));
        }
        let desc_h = req_obj(
            vm,
            arg(args, 0),
            "beginRenderPass requires a descriptor object",
        )?;
        let label = js_prop_str(vm, desc_h, "label");
        let mut color_attachments = Vec::new();
        let mut touched_nodes = Vec::new();
        if let Some(cav) = js_prop(vm, desc_h, "colorAttachments") {
            for item in js_array_elems(vm, cav) {
                let Some(ah) = item.as_object() else {
                    color_attachments.push(None);
                    continue;
                };
                let view_h = js_prop(vm, ah, "view")
                    .and_then(JsValue::as_object)
                    .ok_or_else(|| vm.type_error("colorAttachment.view is required"))?;
                let (view, canvas_node) = {
                    let st = s.borrow();
                    let tv = st
                        .texture_views
                        .get(&view_h)
                        .ok_or_else(|| vm.type_error("Invalid GPUTextureView"))?;
                    (tv.view.clone(), tv.canvas_node)
                };
                if let Some(nid) = canvas_node {
                    touched_nodes.push(nid);
                }
                let resolve_target = js_prop(vm, ah, "resolveTarget")
                    .and_then(JsValue::as_object)
                    .and_then(|rh| s.borrow().texture_views.get(&rh).map(|t| t.view.clone()));
                let load = match js_prop_str(vm, ah, "loadOp").as_deref() {
                    Some("load") => wgpu::LoadOp::Load,
                    _ => {
                        let cv = js_prop(vm, ah, "clearValue")
                            .map(|v| parse_color(vm, v))
                            .unwrap_or(wgpu::Color::TRANSPARENT);
                        wgpu::LoadOp::Clear(cv)
                    }
                };
                let store = match js_prop_str(vm, ah, "storeOp").as_deref() {
                    Some("discard") => wgpu::StoreOp::Discard,
                    _ => wgpu::StoreOp::Store,
                };
                color_attachments.push(Some(ColorAttachmentDesc {
                    view,
                    resolve_target,
                    ops: wgpu::Operations { load, store },
                }));
            }
        }
        let depth_stencil = js_prop(vm, desc_h, "depthStencilAttachment")
            .and_then(JsValue::as_object)
            .and_then(|dsh| {
                let vh = js_prop(vm, dsh, "view").and_then(JsValue::as_object)?;
                let view = s.borrow().texture_views.get(&vh)?.view.clone();
                let depth_ops = js_prop_str(vm, dsh, "depthLoadOp").map(|op| {
                    let load = if op == "load" {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(
                            js_prop_f64(vm, dsh, "depthClearValue").unwrap_or(1.0) as f32
                        )
                    };
                    let store = match js_prop_str(vm, dsh, "depthStoreOp").as_deref() {
                        Some("discard") => wgpu::StoreOp::Discard,
                        _ => wgpu::StoreOp::Store,
                    };
                    wgpu::Operations { load, store }
                });
                let stencil_ops = js_prop_str(vm, dsh, "stencilLoadOp").map(|op| {
                    let load = if op == "load" {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(js_prop_u32(vm, dsh, "stencilClearValue").unwrap_or(0))
                    };
                    let store = match js_prop_str(vm, dsh, "stencilStoreOp").as_deref() {
                        Some("discard") => wgpu::StoreOp::Discard,
                        _ => wgpu::StoreOp::Store,
                    };
                    wgpu::Operations { load, store }
                });
                Some(DepthStencilAttachmentDesc {
                    view,
                    depth_ops,
                    stencil_ops,
                })
            });

        if let Some(enc) = s.borrow_mut().command_encoders.get_mut(&eh) {
            enc.touched_canvases.extend(touched_nodes);
        }
        let proto = s.borrow().protos.render_pass_proto;
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().render_passes.insert(
            obj,
            RenderPassEntry {
                encoder_handle: eh,
                label,
                color_attachments,
                depth_stencil,
                commands: Vec::new(),
                ended: false,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    // GPUCommandEncoder.prototype.beginComputePass(descriptor?)
    let s = Rc::clone(sys);
    define_host_method(vm, enc_p, "beginComputePass", move |vm, this_val, args| {
        let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
        if !s.borrow().command_encoders.contains_key(&eh) {
            return Err(vm.type_error("Incompatible GPUCommandEncoder receiver"));
        }
        let label = arg(args, 0)
            .as_object()
            .and_then(|d| js_prop_str(vm, d, "label"));
        let proto = s.borrow().protos.compute_pass_proto;
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().compute_passes.insert(
            obj,
            ComputePassEntry {
                encoder_handle: eh,
                label,
                commands: Vec::new(),
                ended: false,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    // GPUCommandEncoder.prototype.copyBufferToBuffer(src, srcOffset, dst, dstOffset, size)
    let s = Rc::clone(sys);
    define_host_method(
        vm,
        enc_p,
        "copyBufferToBuffer",
        move |vm, this_val, args| {
            let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
            let src_h = req_obj(vm, arg(args, 0), "source GPUBuffer required")?;
            let src_off = vm.to_number(arg(args, 1))?.max(0.0) as u64;
            let dst_h = req_obj(vm, arg(args, 2), "destination GPUBuffer required")?;
            let dst_off = vm.to_number(arg(args, 3))?.max(0.0) as u64;
            let size = vm.to_number(arg(args, 4))?.max(0.0) as u64;
            let (src_buf, dst_buf) = {
                let st = s.borrow();
                let sb = st
                    .buffers
                    .get(&src_h)
                    .map(|b| b.buffer.clone())
                    .ok_or_else(|| vm.type_error("Invalid source GPUBuffer"))?;
                let db = st
                    .buffers
                    .get(&dst_h)
                    .map(|b| b.buffer.clone())
                    .ok_or_else(|| vm.type_error("Invalid destination GPUBuffer"))?;
                (sb, db)
            };
            if let Some(enc) = s
                .borrow_mut()
                .command_encoders
                .get_mut(&eh)
                .and_then(|e| e.encoder.as_mut())
            {
                enc.copy_buffer_to_buffer(&src_buf, src_off, &dst_buf, dst_off, Some(size));
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        enc_p,
        "copyBufferToTexture",
        move |vm, this_val, args| {
            let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
            let src_h = req_obj(vm, arg(args, 0), "source object required")?;
            let dst = parse_tex_copy_target(vm, &s, arg(args, 1))?;
            let copy_size = parse_extent_3d(vm, arg(args, 2));
            let buf_h = js_prop(vm, src_h, "buffer")
                .and_then(JsValue::as_object)
                .ok_or_else(|| vm.type_error("source.buffer required"))?;
            let layout = parse_texel_copy_buffer_layout(vm, src_h);
            let buf = s
                .borrow()
                .buffers
                .get(&buf_h)
                .map(|b| b.buffer.clone())
                .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
            if let Some(entry) = s.borrow_mut().command_encoders.get_mut(&eh) {
                if let Some(enc) = entry.encoder.as_mut() {
                    enc.copy_buffer_to_texture(
                        wgpu::TexelCopyBufferInfo {
                            buffer: &buf,
                            layout,
                        },
                        dst.as_info(),
                        copy_size,
                    );
                }
                if let Some(nid) = dst.canvas_node {
                    entry.touched_canvases.push(nid);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        enc_p,
        "copyTextureToBuffer",
        move |vm, this_val, args| {
            let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
            let src = parse_tex_copy_target(vm, &s, arg(args, 0))?;
            let dst_h = req_obj(vm, arg(args, 1), "destination object required")?;
            let copy_size = parse_extent_3d(vm, arg(args, 2));
            let buf_h = js_prop(vm, dst_h, "buffer")
                .and_then(JsValue::as_object)
                .ok_or_else(|| vm.type_error("destination.buffer required"))?;
            let layout = parse_texel_copy_buffer_layout(vm, dst_h);
            let buf = s
                .borrow()
                .buffers
                .get(&buf_h)
                .map(|b| b.buffer.clone())
                .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
            if let Some(enc) = s
                .borrow_mut()
                .command_encoders
                .get_mut(&eh)
                .and_then(|e| e.encoder.as_mut())
            {
                enc.copy_texture_to_buffer(
                    src.as_info(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buf,
                        layout,
                    },
                    copy_size,
                );
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        enc_p,
        "copyTextureToTexture",
        move |vm, this_val, args| {
            let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
            let src = parse_tex_copy_target(vm, &s, arg(args, 0))?;
            let dst = parse_tex_copy_target(vm, &s, arg(args, 1))?;
            let copy_size = parse_extent_3d(vm, arg(args, 2));
            if let Some(entry) = s.borrow_mut().command_encoders.get_mut(&eh) {
                if let Some(enc) = entry.encoder.as_mut() {
                    enc.copy_texture_to_texture(src.as_info(), dst.as_info(), copy_size);
                }
                if let Some(nid) = dst.canvas_node {
                    entry.touched_canvases.push(nid);
                }
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    // GPUCommandEncoder.prototype.clearBuffer(buffer, offset?, size?)
    let s = Rc::clone(sys);
    define_host_method(vm, enc_p, "clearBuffer", move |vm, this_val, args| {
        let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
        let buf_h = req_obj(vm, arg(args, 0), "GPUBuffer required")?;
        let offset = opt_u64(vm, args, 1)?.unwrap_or(0);
        let size = opt_u64(vm, args, 2)?;
        let buf = s
            .borrow()
            .buffers
            .get(&buf_h)
            .map(|b| b.buffer.clone())
            .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
        if let Some(enc) = s
            .borrow_mut()
            .command_encoders
            .get_mut(&eh)
            .and_then(|e| e.encoder.as_mut())
        {
            enc.clear_buffer(&buf, offset, size);
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPUCommandEncoder.prototype.finish(descriptor?) -> GPUCommandBuffer
    let s = Rc::clone(sys);
    define_host_method(vm, enc_p, "finish", move |vm, this_val, _| {
        let eh = req_obj(vm, this_val, "Incompatible GPUCommandEncoder receiver")?;
        let (cbuf, touched, proto) = {
            let mut st = s.borrow_mut();
            let proto = st.protos.command_buffer_proto;
            let entry = st
                .command_encoders
                .get_mut(&eh)
                .ok_or_else(|| vm.type_error("Incompatible GPUCommandEncoder receiver"))?;
            let enc = entry
                .encoder
                .take()
                .ok_or_else(|| create_dom_exception(vm, "InvalidStateError", "Encoder finished"))?;
            let touched = std::mem::take(&mut entry.touched_canvases);
            (enc.finish(), touched, proto)
        };
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().command_buffers.insert(
            obj,
            GpuCommandBufferEntry {
                buffer: Some(cbuf),
                touched_canvases: touched,
            },
        );
        Ok(JsValue::from_object(obj))
    });

    // GPURenderPassEncoder methods
    install_render_pass_methods(vm, sys, rpass_p);

    // GPUComputePassEncoder methods
    install_compute_pass_methods(vm, sys, cpass_p);
}

fn install_render_pass_methods(vm: &mut JsVm, sys: &SharedWebGpu, rpass_p: HeapHandle) {
    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setPipeline", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let pipe_h = req_obj(vm, arg(args, 0), "GPURenderPipeline required")?;
        let pipe = s
            .borrow()
            .render_pipelines
            .get(&pipe_h)
            .cloned()
            .ok_or_else(|| vm.type_error("Invalid GPURenderPipeline"))?;
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands.push(RenderPassCmd::SetPipeline(pipe));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setBindGroup", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let idx = vm.to_u32(arg(args, 0))?;
        let bg = arg(args, 1)
            .as_object()
            .and_then(|h| s.borrow().bind_groups.get(&h).cloned());
        let dyn_offs: Vec<u32> = js_array_elems(vm, arg(args, 2))
            .into_iter()
            .filter_map(|v| vm.to_number(v).ok().map(|n| n.max(0.0) as u32))
            .collect();
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands
                .push(RenderPassCmd::SetBindGroup(idx, bg, dyn_offs));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setVertexBuffer", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let slot = vm.to_u32(arg(args, 0))?;
        let buf_h = req_obj(vm, arg(args, 1), "GPUBuffer required")?;
        let offset = opt_u64(vm, args, 2)?.unwrap_or(0);
        let size = opt_u64(vm, args, 3)?;
        let buf = s
            .borrow()
            .buffers
            .get(&buf_h)
            .map(|b| b.buffer.clone())
            .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands
                .push(RenderPassCmd::SetVertexBuffer(slot, buf, offset, size));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setIndexBuffer", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let buf_h = req_obj(vm, arg(args, 0), "GPUBuffer required")?;
        let fmt = match vm.value_to_rust_string(arg(args, 1)).as_str() {
            "uint32" => wgpu::IndexFormat::Uint32,
            _ => wgpu::IndexFormat::Uint16,
        };
        let offset = opt_u64(vm, args, 2)?.unwrap_or(0);
        let size = opt_u64(vm, args, 3)?;
        let buf = s
            .borrow()
            .buffers
            .get(&buf_h)
            .map(|b| b.buffer.clone())
            .ok_or_else(|| vm.type_error("Invalid GPUBuffer"))?;
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands
                .push(RenderPassCmd::SetIndexBuffer(buf, fmt, offset, size));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setViewport", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let x = vm.to_number(arg(args, 0))? as f32;
        let y = vm.to_number(arg(args, 1))? as f32;
        let w = vm.to_number(arg(args, 2))? as f32;
        let h = vm.to_number(arg(args, 3))? as f32;
        let min_d = vm.to_number(arg(args, 4))? as f32;
        let max_d = vm.to_number(arg(args, 5))? as f32;
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands
                .push(RenderPassCmd::SetViewport(x, y, w, h, min_d, max_d));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "setScissorRect", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let x = vm.to_u32(arg(args, 0))?;
        let y = vm.to_u32(arg(args, 1))?;
        let w = vm.to_u32(arg(args, 2))?;
        let h = vm.to_u32(arg(args, 3))?;
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands.push(RenderPassCmd::SetScissorRect(x, y, w, h));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        rpass_p,
        "setBlendConstant",
        move |vm, this_val, args| {
            let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
            let color = parse_color(vm, arg(args, 0));
            if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
                rp.commands.push(RenderPassCmd::SetBlendConstant(color));
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        rpass_p,
        "setStencilReference",
        move |vm, this_val, args| {
            let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
            let r = vm.to_u32(arg(args, 0))?;
            if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
                rp.commands.push(RenderPassCmd::SetStencilReference(r));
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "draw", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let vertex_count = vm.to_u32(arg(args, 0))?;
        let instance_count = opt_u32(vm, args, 1)?.unwrap_or(1);
        let first_vertex = opt_u32(vm, args, 2)?.unwrap_or(0);
        let first_instance = opt_u32(vm, args, 3)?.unwrap_or(0);
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands.push(RenderPassCmd::Draw(
                vertex_count,
                instance_count,
                first_vertex,
                first_instance,
            ));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "drawIndexed", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let index_count = vm.to_u32(arg(args, 0))?;
        let instance_count = opt_u32(vm, args, 1)?.unwrap_or(1);
        let first_index = opt_u32(vm, args, 2)?.unwrap_or(0);
        let base_vertex = if !arg(args, 3).is_undefined() {
            vm.to_number(arg(args, 3))?.trunc() as i32
        } else {
            0
        };
        let first_instance = opt_u32(vm, args, 4)?.unwrap_or(0);
        if let Some(rp) = s.borrow_mut().render_passes.get_mut(&ph) {
            rp.commands.push(RenderPassCmd::DrawIndexed(
                index_count,
                instance_count,
                first_index,
                base_vertex,
                first_instance,
            ));
        }
        Ok(JsValue::UNDEFINED)
    });

    // GPURenderPassEncoder.prototype.end() -> replays recorded commands onto real wgpu::CommandEncoder!
    let s = Rc::clone(sys);
    define_host_method(vm, rpass_p, "end", move |vm, this_val, _| {
        let ph = req_obj(vm, this_val, "Incompatible GPURenderPassEncoder")?;
        let mut st = s.borrow_mut();
        let Some(rp) = st.render_passes.get_mut(&ph) else {
            return Ok(JsValue::UNDEFINED);
        };
        if rp.ended {
            return Ok(JsValue::UNDEFINED);
        }
        rp.ended = true;
        let enc_h = rp.encoder_handle;
        let label = rp.label.clone();
        let colors = std::mem::take(&mut rp.color_attachments);
        let depth_stencil = rp.depth_stencil.take();
        let cmds = std::mem::take(&mut rp.commands);

        if let Some(enc) = st
            .command_encoders
            .get_mut(&enc_h)
            .and_then(|e| e.encoder.as_mut())
        {
            replay_render_pass(
                enc,
                label.as_deref(),
                &colors,
                depth_stencil.as_ref(),
                &cmds,
            );
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_compute_pass_methods(vm: &mut JsVm, sys: &SharedWebGpu, cpass_p: HeapHandle) {
    let s = Rc::clone(sys);
    define_host_method(vm, cpass_p, "setPipeline", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPUComputePassEncoder")?;
        let pipe_h = req_obj(vm, arg(args, 0), "GPUComputePipeline required")?;
        let pipe = s
            .borrow()
            .compute_pipelines
            .get(&pipe_h)
            .cloned()
            .ok_or_else(|| vm.type_error("Invalid GPUComputePipeline"))?;
        if let Some(cp) = s.borrow_mut().compute_passes.get_mut(&ph) {
            cp.commands.push(ComputePassCmd::SetPipeline(pipe));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, cpass_p, "setBindGroup", move |vm, this_val, args| {
        let ph = req_obj(vm, this_val, "Incompatible GPUComputePassEncoder")?;
        let idx = vm.to_u32(arg(args, 0))?;
        let bg = arg(args, 1)
            .as_object()
            .and_then(|h| s.borrow().bind_groups.get(&h).cloned());
        let dyn_offs: Vec<u32> = js_array_elems(vm, arg(args, 2))
            .into_iter()
            .filter_map(|v| vm.to_number(v).ok().map(|n| n.max(0.0) as u32))
            .collect();
        if let Some(cp) = s.borrow_mut().compute_passes.get_mut(&ph) {
            cp.commands
                .push(ComputePassCmd::SetBindGroup(idx, bg, dyn_offs));
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(
        vm,
        cpass_p,
        "dispatchWorkgroups",
        move |vm, this_val, args| {
            let ph = req_obj(vm, this_val, "Incompatible GPUComputePassEncoder")?;
            let x = vm.to_u32(arg(args, 0))?.max(1);
            let y = if !arg(args, 1).is_undefined() {
                vm.to_u32(arg(args, 1))?.max(1)
            } else {
                1
            };
            let z = if !arg(args, 2).is_undefined() {
                vm.to_u32(arg(args, 2))?.max(1)
            } else {
                1
            };
            if let Some(cp) = s.borrow_mut().compute_passes.get_mut(&ph) {
                cp.commands.push(ComputePassCmd::Dispatch(x, y, z));
            }
            Ok(JsValue::UNDEFINED)
        },
    );

    let s = Rc::clone(sys);
    define_host_method(vm, cpass_p, "end", move |vm, this_val, _| {
        let ph = req_obj(vm, this_val, "Incompatible GPUComputePassEncoder")?;
        let mut st = s.borrow_mut();
        let Some(cp) = st.compute_passes.get_mut(&ph) else {
            return Ok(JsValue::UNDEFINED);
        };
        if cp.ended {
            return Ok(JsValue::UNDEFINED);
        }
        cp.ended = true;
        let enc_h = cp.encoder_handle;
        let label = cp.label.clone();
        let cmds = std::mem::take(&mut cp.commands);
        if let Some(enc) = st
            .command_encoders
            .get_mut(&enc_h)
            .and_then(|e| e.encoder.as_mut())
        {
            replay_compute_pass(enc, label.as_deref(), &cmds);
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn install_canvas_context_methods(
    vm: &mut JsVm,
    host: &SharedHost,
    sys: &SharedWebGpu,
    cctx_p: HeapHandle,
) {
    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    define_host_getter(vm, cctx_p, "canvas", move |vm, this_val, _| {
        let nid = require_canvas_ctx_node(vm, &s, this_val)?;
        Ok(wrap_node(vm, &h, nid))
    });

    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    define_host_method(vm, cctx_p, "configure", move |vm, this_val, args| {
        let nid = require_canvas_ctx_node(vm, &s, this_val)?;
        let cfg_h = req_obj(
            vm,
            arg(args, 0),
            "GPUCanvasContext.configure requires a configuration",
        )?;
        let dev_h = js_prop(vm, cfg_h, "device")
            .and_then(JsValue::as_object)
            .ok_or_else(|| vm.type_error("configuration.device is required"))?;
        let dev = s
            .borrow()
            .devices
            .get(&dev_h)
            .cloned()
            .ok_or_else(|| vm.type_error("Invalid GPUDevice"))?;
        let fmt_str = js_prop_str(vm, cfg_h, "format").unwrap_or_else(|| "rgba8unorm".to_string());
        let format = parse_texture_format(&fmt_str).ok_or_else(|| {
            vm.type_error(format!("Unsupported canvas texture format: {fmt_str}"))
        })?;
        let usage = parse_texture_usages(js_prop_u32(vm, cfg_h, "usage").unwrap_or(0x10))
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::TEXTURE_BINDING;
        let w = canvas_attr_dim(&h, nid, "width", 300).max(1);
        let h_px = canvas_attr_dim(&h, nid, "height", 150).max(1);

        if let Some(ctx) = s.borrow_mut().canvas_contexts.get_mut(&nid) {
            ctx.width = w;
            ctx.height = h_px;
            ctx.device = Some(dev.device);
            ctx.queue = Some(dev.queue);
            ctx.renderer_token = dev.renderer_token;
            ctx.format = format;
            ctx.format_name = fmt_str;
            ctx.usage = usage;
            ctx.current_texture = None;
            ctx.dirty = true;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, cctx_p, "unconfigure", move |vm, this_val, _| {
        let nid = require_canvas_ctx_node(vm, &s, this_val)?;
        if let Some(ctx) = s.borrow_mut().canvas_contexts.get_mut(&nid) {
            ctx.device = None;
            ctx.queue = None;
            ctx.current_texture = None;
            ctx.dirty = false;
        }
        Ok(JsValue::UNDEFINED)
    });

    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    define_host_method(vm, cctx_p, "getCurrentTexture", move |vm, this_val, _| {
        let nid = require_canvas_ctx_node(vm, &s, this_val)?;
        let w = canvas_attr_dim(&h, nid, "width", 300).max(1);
        let h_px = canvas_attr_dim(&h, nid, "height", 150).max(1);
        let entry = {
            let mut st = s.borrow_mut();
            let ctx = st
                .canvas_contexts
                .get_mut(&nid)
                .ok_or_else(|| vm.type_error("Incompatible GPUCanvasContext receiver"))?;
            let device = ctx.device.as_ref().ok_or_else(|| {
                create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "GPUCanvasContext is not configured",
                )
            })?;
            if ctx.current_texture.is_none() || ctx.width != w || ctx.height != h_px {
                ctx.width = w;
                ctx.height = h_px;
                ctx.current_texture = Some(device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("rix-webgpu-canvas-swapchain"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h_px,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: ctx.format,
                    usage: ctx.usage,
                    view_formats: &[],
                }));
            }
            GpuTextureEntry {
                texture: ctx.current_texture.as_ref().unwrap().clone(),
                width: ctx.width,
                height: ctx.height,
                depth_or_array_layers: 1,
                mip_level_count: 1,
                sample_count: 1,
                dimension: "2d".to_string(),
                format: ctx.format_name.clone(),
                usage: ctx.usage.bits(),
                canvas_node: Some(nid),
            }
        };
        Ok(JsValue::from_object(wrap_texture(vm, &s, entry)))
    });
}

fn attach_navigator_gpu(vm: &mut JsVm, sys: &SharedWebGpu) {
    let gpu_proto = sys.borrow().protos.gpu_proto;
    let gpu_obj = alloc_with_proto(vm, gpu_proto);
    let nav_k = vm.heap.intern_key("navigator");
    let nav_h = vm
        .get_property_by_key(vm.global, nav_k)
        .ok()
        .and_then(JsValue::as_object)
        .unwrap_or_else(|| {
            let nh = vm.alloc_ordinary_object();
            define_global(vm, "navigator", JsValue::from_object(nh));
            nh
        });
    let gpu_k = vm.heap.intern_key("gpu");
    vm.heap.object_define_data(
        nav_h,
        gpu_k,
        JsValue::from_object(gpu_obj),
        PropertyAttrs::READONLY,
        &mut vm.shapes,
    );
}

fn register_canvas_webgpu_hooks(host: &SharedHost, canvas2d: &SharedCanvas2D, sys: &SharedWebGpu) {
    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    canvas2d
        .borrow_mut()
        .register_context_hook(Rc::new(move |vm, nid, kind, _opts| {
            if !kind.eq_ignore_ascii_case("webgpu") {
                return Ok(None);
            }
            if let Some(&existing) = s.borrow().canvas_node_to_ctx_handle.get(&nid) {
                if vm.heap.is_alive(existing) {
                    return Ok(Some(JsValue::from_object(existing)));
                }
            }
            let w = canvas_attr_dim(&h, nid, "width", 300).max(1);
            let h_px = canvas_attr_dim(&h, nid, "height", 150).max(1);
            let proto = s.borrow().protos.canvas_ctx_proto;
            let obj = alloc_with_proto(vm, proto);
            let mut st = s.borrow_mut();
            st.canvas_contexts
                .entry(nid)
                .or_insert_with(|| WebGpuCanvasContextEntry {
                    surface_id: surface_id_for_node(nid),
                    width: w,
                    height: h_px,
                    device: None,
                    queue: None,
                    renderer_token: 0,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    format_name: "rgba8unorm".to_string(),
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    current_texture: None,
                    dirty: false,
                });
            st.canvas_ctx_handle_to_node.insert(obj, nid);
            st.canvas_node_to_ctx_handle.insert(nid, obj);
            Ok(Some(JsValue::from_object(obj)))
        }));

    let s = Rc::clone(sys);
    canvas2d
        .borrow_mut()
        .register_resize_hook(Rc::new(move |nid, w, h_px| {
            s.borrow_mut().resize_canvas(nid, w, h_px);
        }));

    let s = Rc::clone(sys);
    canvas2d
        .borrow_mut()
        .register_readback_hook(Rc::new(move |nid| s.borrow().readback_canvas_rgba(nid)));
}
