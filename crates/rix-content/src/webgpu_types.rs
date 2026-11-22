//! WebGPU descriptor parsers, JS buffer/dictionary extraction helpers, flag constants,
//! and recorded render/compute pass command replay over `rix_render::wgpu`.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #26.

use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_render::wgpu;

use crate::web_apis::define_global;

/// Recorded draw/state command inside a `GPURenderPassEncoder` replayed on `end()`.
pub enum RenderPassCmd {
    SetPipeline(wgpu::RenderPipeline),
    SetBindGroup(u32, Option<wgpu::BindGroup>, Vec<u32>),
    SetVertexBuffer(u32, wgpu::Buffer, u64, Option<u64>),
    SetIndexBuffer(wgpu::Buffer, wgpu::IndexFormat, u64, Option<u64>),
    SetViewport(f32, f32, f32, f32, f32, f32),
    SetScissorRect(u32, u32, u32, u32),
    SetBlendConstant(wgpu::Color),
    SetStencilReference(u32),
    Draw(u32, u32, u32, u32),
    DrawIndexed(u32, u32, u32, i32, u32),
}

/// Owned color attachment descriptor captured by `beginRenderPass()`.
pub struct ColorAttachmentDesc {
    pub view: wgpu::TextureView,
    pub resolve_target: Option<wgpu::TextureView>,
    pub ops: wgpu::Operations<wgpu::Color>,
}

/// Owned depth/stencil attachment descriptor captured by `beginRenderPass()`.
pub struct DepthStencilAttachmentDesc {
    pub view: wgpu::TextureView,
    pub depth_ops: Option<wgpu::Operations<f32>>,
    pub stencil_ops: Option<wgpu::Operations<u32>>,
}

/// Recorded compute command inside a `GPUComputePassEncoder` replayed on `end()`.
pub enum ComputePassCmd {
    SetPipeline(wgpu::ComputePipeline),
    SetBindGroup(u32, Option<wgpu::BindGroup>, Vec<u32>),
    Dispatch(u32, u32, u32),
}

/// Replays a recorded WebGPU render pass onto a real `wgpu::CommandEncoder`.
pub fn replay_render_pass(
    encoder: &mut wgpu::CommandEncoder,
    label: Option<&str>,
    colors: &[Option<ColorAttachmentDesc>],
    depth_stencil: Option<&DepthStencilAttachmentDesc>,
    cmds: &[RenderPassCmd],
) {
    let color_refs: Vec<Option<wgpu::RenderPassColorAttachment<'_>>> = colors
        .iter()
        .map(|opt| {
            opt.as_ref().map(|c| wgpu::RenderPassColorAttachment {
                view: &c.view,
                depth_slice: None,
                resolve_target: c.resolve_target.as_ref(),
                ops: c.ops,
            })
        })
        .collect();
    let ds_ref = depth_stencil.map(|d| wgpu::RenderPassDepthStencilAttachment {
        view: &d.view,
        depth_ops: d.depth_ops,
        stencil_ops: d.stencil_ops,
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label,
        color_attachments: &color_refs,
        depth_stencil_attachment: ds_ref,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    for cmd in cmds {
        match cmd {
            RenderPassCmd::SetPipeline(pipe) => pass.set_pipeline(pipe),
            RenderPassCmd::SetBindGroup(idx, bg, offs) => {
                pass.set_bind_group(*idx, bg.as_ref(), offs);
            }
            RenderPassCmd::SetVertexBuffer(slot, buf, off, sz) => {
                let slice = match sz {
                    Some(s) => buf.slice(*off..(*off + *s)),
                    None => buf.slice(*off..),
                };
                pass.set_vertex_buffer(*slot, slice);
            }
            RenderPassCmd::SetIndexBuffer(buf, fmt, off, sz) => {
                let slice = match sz {
                    Some(s) => buf.slice(*off..(*off + *s)),
                    None => buf.slice(*off..),
                };
                pass.set_index_buffer(slice, *fmt);
            }
            RenderPassCmd::SetViewport(x, y, w, h, mn, mx) => {
                pass.set_viewport(*x, *y, *w, *h, *mn, *mx);
            }
            RenderPassCmd::SetScissorRect(x, y, w, h) => {
                pass.set_scissor_rect(*x, *y, *w, *h);
            }
            RenderPassCmd::SetBlendConstant(c) => pass.set_blend_constant(*c),
            RenderPassCmd::SetStencilReference(r) => pass.set_stencil_reference(*r),
            RenderPassCmd::Draw(vc, ic, fv, fi) => {
                pass.draw(*fv..(*fv + *vc), *fi..(*fi + *ic));
            }
            RenderPassCmd::DrawIndexed(ic, instc, fi, bv, finst) => {
                pass.draw_indexed(*fi..(*fi + *ic), *bv, *finst..(*finst + *instc));
            }
        }
    }
}

/// Replays a recorded WebGPU compute pass onto a real `wgpu::CommandEncoder`.
pub fn replay_compute_pass(
    encoder: &mut wgpu::CommandEncoder,
    label: Option<&str>,
    cmds: &[ComputePassCmd],
) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label,
        timestamp_writes: None,
    });
    for cmd in cmds {
        match cmd {
            ComputePassCmd::SetPipeline(pipe) => pass.set_pipeline(pipe),
            ComputePassCmd::SetBindGroup(idx, bg, offs) => {
                pass.set_bind_group(*idx, bg.as_ref(), offs);
            }
            ComputePassCmd::Dispatch(x, y, z) => pass.dispatch_workgroups(*x, *y, *z),
        }
    }
}

/// Allocates an ordinary prototype object on `vm`.
pub fn alloc_proto(vm: &mut JsVm) -> HeapHandle {
    let p = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(p).unwrap().is_prototype = true;
    p
}

/// Allocates an ordinary object inheriting from `proto` (or `Object.prototype`).
pub fn alloc_with_proto(vm: &mut JsVm, proto: Option<HeapHandle>) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(proto.unwrap_or(vm.object_proto)),
    ))
}

fn define_const_obj(vm: &mut JsVm, global_name: &str, entries: &[(&str, u32)]) {
    let obj = vm.alloc_ordinary_object();
    for &(name, val) in entries {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            obj,
            k,
            JsValue::from_number(f64::from(val)),
            PropertyAttrs::READONLY,
            &mut vm.shapes,
        );
    }
    define_global(vm, global_name, JsValue::from_object(obj));
}

/// Installs global W3C WebGPU bitflag namespaces (`GPUBufferUsage`, `GPUTextureUsage`,
/// `GPUShaderStage`, `GPUMapMode`, `GPUColorWrite`).
pub fn install_webgpu_constants(vm: &mut JsVm) {
    define_const_obj(
        vm,
        "GPUBufferUsage",
        &[
            ("MAP_READ", 0x0001),
            ("MAP_WRITE", 0x0002),
            ("COPY_SRC", 0x0004),
            ("COPY_DST", 0x0008),
            ("INDEX", 0x0010),
            ("VERTEX", 0x0020),
            ("UNIFORM", 0x0040),
            ("STORAGE", 0x0080),
            ("INDIRECT", 0x0100),
            ("QUERY_RESOLVE", 0x0200),
        ],
    );
    define_const_obj(
        vm,
        "GPUTextureUsage",
        &[
            ("COPY_SRC", 0x01),
            ("COPY_DST", 0x02),
            ("TEXTURE_BINDING", 0x04),
            ("STORAGE_BINDING", 0x08),
            ("RENDER_ATTACHMENT", 0x10),
        ],
    );
    define_const_obj(
        vm,
        "GPUShaderStage",
        &[("VERTEX", 0x1), ("FRAGMENT", 0x2), ("COMPUTE", 0x4)],
    );
    define_const_obj(vm, "GPUMapMode", &[("READ", 0x1), ("WRITE", 0x2)]);
    define_const_obj(
        vm,
        "GPUColorWrite",
        &[
            ("RED", 0x1),
            ("GREEN", 0x2),
            ("BLUE", 0x4),
            ("ALPHA", 0x8),
            ("ALL", 0xF),
        ],
    );
}

pub fn js_prop(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<JsValue> {
    let k = vm.heap.intern_key(name);
    let val = vm.get_property_by_key(obj, k).ok()?;
    if val.is_undefined() || val.is_null() {
        None
    } else {
        Some(val)
    }
}

pub fn js_prop_str(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<String> {
    js_prop(vm, obj, name).map(|v| vm.value_to_rust_string(v))
}

pub fn js_prop_u32(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<u32> {
    js_prop(vm, obj, name).and_then(|v| vm.to_number(v).ok().map(|n| n.max(0.0).trunc() as u32))
}

pub fn js_prop_u64(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<u64> {
    js_prop(vm, obj, name).and_then(|v| vm.to_number(v).ok().map(|n| n.max(0.0).trunc() as u64))
}

pub fn js_prop_f64(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<f64> {
    js_prop(vm, obj, name).and_then(|v| vm.to_number(v).ok())
}

pub fn js_prop_bool(vm: &mut JsVm, obj: HeapHandle, name: &str) -> Option<bool> {
    js_prop(vm, obj, name).map(|v| vm.to_boolean(v))
}

pub fn js_array_elems(vm: &mut JsVm, val: JsValue) -> Vec<JsValue> {
    let Some(h) = val.as_object() else {
        return Vec::new();
    };
    if let Some(obj) = vm.heap.get_object(h) {
        if !obj.elements.is_empty() {
            let len = obj.elements.len();
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                out.push(obj.elements.get(i).unwrap_or(JsValue::UNDEFINED));
            }
            return out;
        }
    }
    let len = js_prop_u32(vm, h, "length").unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let k = vm.heap.intern_key(&i.to_string());
        out.push(vm.get_property_by_key(h, k).unwrap_or(JsValue::UNDEFINED));
    }
    out
}

/// Extracts a byte slice from a JS `BufferSource` (`ArrayBuffer`, `TypedArray`, `DataView`).
/// For `TypedArray`, `data_offset` and `size` are measured in **elements** per WebGPU spec.
pub fn extract_gpu_buffer_source(
    vm: &JsVm,
    val: JsValue,
    data_offset: Option<usize>,
    size: Option<usize>,
) -> Option<Vec<u8>> {
    let h = val.as_object()?;
    match vm.heap.get_object(h).map(|o| &o.kind)? {
        ObjectKind::ArrayBuffer(buf) => {
            let start = data_offset.unwrap_or(0).min(buf.len());
            let end = size.map_or(buf.len(), |s| start.saturating_add(s).min(buf.len()));
            Some(buf[start..end].to_vec())
        }
        ObjectKind::TypedArray(ta) => {
            let ta = *ta;
            let elem_bytes = ta.kind.bytes_per_element();
            let total_elems = ta.length as usize;
            let elem_start = data_offset.unwrap_or(0).min(total_elems);
            let elem_count = size.unwrap_or(total_elems.saturating_sub(elem_start));
            let byte_start = (ta.byte_offset as usize).saturating_add(elem_start * elem_bytes);
            let byte_len = elem_count.saturating_mul(elem_bytes);
            let ObjectKind::ArrayBuffer(buf) = vm.heap.get_object(ta.buffer).map(|o| &o.kind)?
            else {
                return None;
            };
            let start = byte_start.min(buf.len());
            let end = start.saturating_add(byte_len).min(buf.len());
            Some(buf[start..end].to_vec())
        }
        ObjectKind::DataView(dv) => {
            let dv = *dv;
            let start = (dv.byte_offset as usize).saturating_add(data_offset.unwrap_or(0));
            let avail = (dv.byte_length as usize).saturating_sub(data_offset.unwrap_or(0));
            let len = size.unwrap_or(avail).min(avail);
            let ObjectKind::ArrayBuffer(buf) = vm.heap.get_object(dv.buffer).map(|o| &o.kind)?
            else {
                return None;
            };
            let s = start.min(buf.len());
            let e = s.saturating_add(len).min(buf.len());
            Some(buf[s..e].to_vec())
        }
        _ => None,
    }
}

pub fn set_str_prop(vm: &mut JsVm, obj: HeapHandle, name: &str, val: &str) {
    let k = vm.heap.intern_key(name);
    let s = JsValue::from_string(vm.heap.alloc_string(val));
    vm.heap
        .object_define_data(obj, k, s, PropertyAttrs::READONLY, &mut vm.shapes);
}

pub fn set_num_prop(vm: &mut JsVm, obj: HeapHandle, name: &str, val: f64) {
    let k = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        obj,
        k,
        JsValue::from_number(val),
        PropertyAttrs::READONLY,
        &mut vm.shapes,
    );
}

pub fn build_limits_object(vm: &mut JsVm, limits: &wgpu::Limits) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    set_num_prop(
        vm,
        obj,
        "maxTextureDimension1D",
        f64::from(limits.max_texture_dimension_1d),
    );
    set_num_prop(
        vm,
        obj,
        "maxTextureDimension2D",
        f64::from(limits.max_texture_dimension_2d),
    );
    set_num_prop(
        vm,
        obj,
        "maxTextureDimension3D",
        f64::from(limits.max_texture_dimension_3d),
    );
    set_num_prop(
        vm,
        obj,
        "maxTextureArrayLayers",
        f64::from(limits.max_texture_array_layers),
    );
    set_num_prop(vm, obj, "maxBindGroups", f64::from(limits.max_bind_groups));
    set_num_prop(
        vm,
        obj,
        "maxUniformBufferBindingSize",
        limits.max_uniform_buffer_binding_size as f64,
    );
    set_num_prop(
        vm,
        obj,
        "maxStorageBufferBindingSize",
        limits.max_storage_buffer_binding_size as f64,
    );
    set_num_prop(
        vm,
        obj,
        "maxVertexBuffers",
        f64::from(limits.max_vertex_buffers),
    );
    set_num_prop(
        vm,
        obj,
        "maxVertexAttributes",
        f64::from(limits.max_vertex_attributes),
    );
    set_num_prop(vm, obj, "maxBufferSize", limits.max_buffer_size as f64);
    set_num_prop(
        vm,
        obj,
        "maxComputeWorkgroupSizeX",
        f64::from(limits.max_compute_workgroup_size_x),
    );
    set_num_prop(
        vm,
        obj,
        "maxComputeWorkgroupSizeY",
        f64::from(limits.max_compute_workgroup_size_y),
    );
    set_num_prop(
        vm,
        obj,
        "maxComputeWorkgroupSizeZ",
        f64::from(limits.max_compute_workgroup_size_z),
    );
    set_num_prop(
        vm,
        obj,
        "maxComputeWorkgroupsPerDimension",
        f64::from(limits.max_compute_workgroups_per_dimension),
    );
    obj
}

pub fn parse_texture_format(s: &str) -> Option<wgpu::TextureFormat> {
    Some(match s {
        "r8unorm" => wgpu::TextureFormat::R8Unorm,
        "r8snorm" => wgpu::TextureFormat::R8Snorm,
        "r8uint" => wgpu::TextureFormat::R8Uint,
        "r8sint" => wgpu::TextureFormat::R8Sint,
        "r16uint" => wgpu::TextureFormat::R16Uint,
        "r16sint" => wgpu::TextureFormat::R16Sint,
        "r16float" => wgpu::TextureFormat::R16Float,
        "rg8unorm" => wgpu::TextureFormat::Rg8Unorm,
        "rg8snorm" => wgpu::TextureFormat::Rg8Snorm,
        "rg8uint" => wgpu::TextureFormat::Rg8Uint,
        "rg8sint" => wgpu::TextureFormat::Rg8Sint,
        "r32uint" => wgpu::TextureFormat::R32Uint,
        "r32sint" => wgpu::TextureFormat::R32Sint,
        "r32float" => wgpu::TextureFormat::R32Float,
        "rg16uint" => wgpu::TextureFormat::Rg16Uint,
        "rg16sint" => wgpu::TextureFormat::Rg16Sint,
        "rg16float" => wgpu::TextureFormat::Rg16Float,
        "rgba8unorm" => wgpu::TextureFormat::Rgba8Unorm,
        "rgba8unorm-srgb" => wgpu::TextureFormat::Rgba8UnormSrgb,
        "rgba8snorm" => wgpu::TextureFormat::Rgba8Snorm,
        "rgba8uint" => wgpu::TextureFormat::Rgba8Uint,
        "rgba8sint" => wgpu::TextureFormat::Rgba8Sint,
        "bgra8unorm" => wgpu::TextureFormat::Bgra8Unorm,
        "bgra8unorm-srgb" => wgpu::TextureFormat::Bgra8UnormSrgb,
        "rg32uint" => wgpu::TextureFormat::Rg32Uint,
        "rg32sint" => wgpu::TextureFormat::Rg32Sint,
        "rg32float" => wgpu::TextureFormat::Rg32Float,
        "rgba16uint" => wgpu::TextureFormat::Rgba16Uint,
        "rgba16sint" => wgpu::TextureFormat::Rgba16Sint,
        "rgba16float" => wgpu::TextureFormat::Rgba16Float,
        "rgba32uint" => wgpu::TextureFormat::Rgba32Uint,
        "rgba32sint" => wgpu::TextureFormat::Rgba32Sint,
        "rgba32float" => wgpu::TextureFormat::Rgba32Float,
        "depth16unorm" => wgpu::TextureFormat::Depth16Unorm,
        "depth24plus" => wgpu::TextureFormat::Depth24Plus,
        "depth24plus-stencil8" => wgpu::TextureFormat::Depth24PlusStencil8,
        "depth32float" => wgpu::TextureFormat::Depth32Float,
        "stencil8" => wgpu::TextureFormat::Stencil8,
        _ => return None,
    })
}

pub fn parse_vertex_format(s: &str) -> Option<wgpu::VertexFormat> {
    Some(match s {
        "uint8x2" => wgpu::VertexFormat::Uint8x2,
        "uint8x4" => wgpu::VertexFormat::Uint8x4,
        "sint8x2" => wgpu::VertexFormat::Sint8x2,
        "sint8x4" => wgpu::VertexFormat::Sint8x4,
        "unorm8x2" => wgpu::VertexFormat::Unorm8x2,
        "unorm8x4" => wgpu::VertexFormat::Unorm8x4,
        "snorm8x2" => wgpu::VertexFormat::Snorm8x2,
        "snorm8x4" => wgpu::VertexFormat::Snorm8x4,
        "uint16x2" => wgpu::VertexFormat::Uint16x2,
        "uint16x4" => wgpu::VertexFormat::Uint16x4,
        "sint16x2" => wgpu::VertexFormat::Sint16x2,
        "sint16x4" => wgpu::VertexFormat::Sint16x4,
        "unorm16x2" => wgpu::VertexFormat::Unorm16x2,
        "unorm16x4" => wgpu::VertexFormat::Unorm16x4,
        "snorm16x2" => wgpu::VertexFormat::Snorm16x2,
        "snorm16x4" => wgpu::VertexFormat::Snorm16x4,
        "float16x2" => wgpu::VertexFormat::Float16x2,
        "float16x4" => wgpu::VertexFormat::Float16x4,
        "float32" => wgpu::VertexFormat::Float32,
        "float32x2" => wgpu::VertexFormat::Float32x2,
        "float32x3" => wgpu::VertexFormat::Float32x3,
        "float32x4" => wgpu::VertexFormat::Float32x4,
        "uint32" => wgpu::VertexFormat::Uint32,
        "uint32x2" => wgpu::VertexFormat::Uint32x2,
        "uint32x3" => wgpu::VertexFormat::Uint32x3,
        "uint32x4" => wgpu::VertexFormat::Uint32x4,
        "sint32" => wgpu::VertexFormat::Sint32,
        "sint32x2" => wgpu::VertexFormat::Sint32x2,
        "sint32x3" => wgpu::VertexFormat::Sint32x3,
        "sint32x4" => wgpu::VertexFormat::Sint32x4,
        _ => return None,
    })
}

pub fn parse_topology(s: &str) -> wgpu::PrimitiveTopology {
    match s {
        "point-list" => wgpu::PrimitiveTopology::PointList,
        "line-list" => wgpu::PrimitiveTopology::LineList,
        "line-strip" => wgpu::PrimitiveTopology::LineStrip,
        "triangle-strip" => wgpu::PrimitiveTopology::TriangleStrip,
        _ => wgpu::PrimitiveTopology::TriangleList,
    }
}

pub fn parse_compare_function(s: &str) -> wgpu::CompareFunction {
    match s {
        "never" => wgpu::CompareFunction::Never,
        "less" => wgpu::CompareFunction::Less,
        "equal" => wgpu::CompareFunction::Equal,
        "less-equal" => wgpu::CompareFunction::LessEqual,
        "greater" => wgpu::CompareFunction::Greater,
        "not-equal" => wgpu::CompareFunction::NotEqual,
        "greater-equal" => wgpu::CompareFunction::GreaterEqual,
        _ => wgpu::CompareFunction::Always,
    }
}

pub fn parse_blend_factor(s: &str) -> wgpu::BlendFactor {
    match s {
        "zero" => wgpu::BlendFactor::Zero,
        "one" => wgpu::BlendFactor::One,
        "src" => wgpu::BlendFactor::Src,
        "one-minus-src" => wgpu::BlendFactor::OneMinusSrc,
        "src-alpha" => wgpu::BlendFactor::SrcAlpha,
        "one-minus-src-alpha" => wgpu::BlendFactor::OneMinusSrcAlpha,
        "dst" => wgpu::BlendFactor::Dst,
        "one-minus-dst" => wgpu::BlendFactor::OneMinusDst,
        "dst-alpha" => wgpu::BlendFactor::DstAlpha,
        "one-minus-dst-alpha" => wgpu::BlendFactor::OneMinusDstAlpha,
        "src-alpha-saturated" => wgpu::BlendFactor::SrcAlphaSaturated,
        "constant" => wgpu::BlendFactor::Constant,
        "one-minus-constant" => wgpu::BlendFactor::OneMinusConstant,
        _ => wgpu::BlendFactor::One,
    }
}

pub fn parse_blend_op(s: &str) -> wgpu::BlendOperation {
    match s {
        "subtract" => wgpu::BlendOperation::Subtract,
        "reverse-subtract" => wgpu::BlendOperation::ReverseSubtract,
        "min" => wgpu::BlendOperation::Min,
        "max" => wgpu::BlendOperation::Max,
        _ => wgpu::BlendOperation::Add,
    }
}

pub fn parse_blend_component(vm: &mut JsVm, val: JsValue) -> wgpu::BlendComponent {
    let Some(h) = val.as_object() else {
        return wgpu::BlendComponent::REPLACE;
    };
    wgpu::BlendComponent {
        src_factor: js_prop_str(vm, h, "srcFactor")
            .as_deref()
            .map_or(wgpu::BlendFactor::One, parse_blend_factor),
        dst_factor: js_prop_str(vm, h, "dstFactor")
            .as_deref()
            .map_or(wgpu::BlendFactor::Zero, parse_blend_factor),
        operation: js_prop_str(vm, h, "operation")
            .as_deref()
            .map_or(wgpu::BlendOperation::Add, parse_blend_op),
    }
}

pub fn parse_color(vm: &mut JsVm, val: JsValue) -> wgpu::Color {
    if let Some(h) = val.as_object() {
        let elems = js_array_elems(vm, val);
        if elems.len() >= 4 {
            return wgpu::Color {
                r: vm.to_number(elems[0]).unwrap_or(0.0),
                g: vm.to_number(elems[1]).unwrap_or(0.0),
                b: vm.to_number(elems[2]).unwrap_or(0.0),
                a: vm.to_number(elems[3]).unwrap_or(1.0),
            };
        }
        return wgpu::Color {
            r: js_prop_f64(vm, h, "r").unwrap_or(0.0),
            g: js_prop_f64(vm, h, "g").unwrap_or(0.0),
            b: js_prop_f64(vm, h, "b").unwrap_or(0.0),
            a: js_prop_f64(vm, h, "a").unwrap_or(1.0),
        };
    }
    wgpu::Color::BLACK
}

pub fn parse_extent_3d(vm: &mut JsVm, val: JsValue) -> wgpu::Extent3d {
    if let Some(h) = val.as_object() {
        let elems = js_array_elems(vm, val);
        if !elems.is_empty() {
            let w = elems
                .first()
                .and_then(|v| vm.to_number(*v).ok())
                .map_or(1, |n| n.max(1.0) as u32);
            let ht = elems
                .get(1)
                .and_then(|v| vm.to_number(*v).ok())
                .map_or(1, |n| n.max(1.0) as u32);
            let d = elems
                .get(2)
                .and_then(|v| vm.to_number(*v).ok())
                .map_or(1, |n| n.max(1.0) as u32);
            return wgpu::Extent3d {
                width: w,
                height: ht,
                depth_or_array_layers: d,
            };
        }
        return wgpu::Extent3d {
            width: js_prop_u32(vm, h, "width").unwrap_or(1).max(1),
            height: js_prop_u32(vm, h, "height").unwrap_or(1).max(1),
            depth_or_array_layers: js_prop_u32(vm, h, "depthOrArrayLayers").unwrap_or(1).max(1),
        };
    }
    wgpu::Extent3d {
        width: 1,
        height: 1,
        depth_or_array_layers: 1,
    }
}

pub fn parse_origin_3d(vm: &mut JsVm, val: JsValue) -> wgpu::Origin3d {
    if let Some(h) = val.as_object() {
        let elems = js_array_elems(vm, val);
        if !elems.is_empty() {
            return wgpu::Origin3d {
                x: elems
                    .first()
                    .and_then(|v| vm.to_number(*v).ok())
                    .map_or(0, |n| n.max(0.0) as u32),
                y: elems
                    .get(1)
                    .and_then(|v| vm.to_number(*v).ok())
                    .map_or(0, |n| n.max(0.0) as u32),
                z: elems
                    .get(2)
                    .and_then(|v| vm.to_number(*v).ok())
                    .map_or(0, |n| n.max(0.0) as u32),
            };
        }
        return wgpu::Origin3d {
            x: js_prop_u32(vm, h, "x").unwrap_or(0),
            y: js_prop_u32(vm, h, "y").unwrap_or(0),
            z: js_prop_u32(vm, h, "z").unwrap_or(0),
        };
    }
    wgpu::Origin3d::ZERO
}

pub fn parse_address_mode(s: &str) -> wgpu::AddressMode {
    match s {
        "repeat" => wgpu::AddressMode::Repeat,
        "mirror-repeat" => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::ClampToEdge,
    }
}

pub fn parse_filter_mode(s: &str) -> wgpu::FilterMode {
    match s {
        "linear" => wgpu::FilterMode::Linear,
        _ => wgpu::FilterMode::Nearest,
    }
}

pub fn parse_mipmap_filter_mode(s: &str) -> wgpu::MipmapFilterMode {
    match s {
        "linear" => wgpu::MipmapFilterMode::Linear,
        _ => wgpu::MipmapFilterMode::Nearest,
    }
}

pub fn parse_shader_stages(bits: u32) -> wgpu::ShaderStages {
    let mut s = wgpu::ShaderStages::empty();
    if bits & 0x1 != 0 {
        s |= wgpu::ShaderStages::VERTEX;
    }
    if bits & 0x2 != 0 {
        s |= wgpu::ShaderStages::FRAGMENT;
    }
    if bits & 0x4 != 0 {
        s |= wgpu::ShaderStages::COMPUTE;
    }
    s
}

pub fn parse_texture_view_dimension(s: &str) -> wgpu::TextureViewDimension {
    match s {
        "1d" => wgpu::TextureViewDimension::D1,
        "2d-array" => wgpu::TextureViewDimension::D2Array,
        "cube" => wgpu::TextureViewDimension::Cube,
        "cube-array" => wgpu::TextureViewDimension::CubeArray,
        "3d" => wgpu::TextureViewDimension::D3,
        _ => wgpu::TextureViewDimension::D2,
    }
}

pub fn parse_texture_aspect(s: &str) -> wgpu::TextureAspect {
    match s {
        "stencil-only" => wgpu::TextureAspect::StencilOnly,
        "depth-only" => wgpu::TextureAspect::DepthOnly,
        _ => wgpu::TextureAspect::All,
    }
}

pub fn parse_buffer_usages(bits: u32) -> wgpu::BufferUsages {
    let mut u = wgpu::BufferUsages::empty();
    if bits & 0x0001 != 0 {
        u |= wgpu::BufferUsages::MAP_READ;
    }
    if bits & 0x0002 != 0 {
        u |= wgpu::BufferUsages::MAP_WRITE;
    }
    if bits & 0x0004 != 0 {
        u |= wgpu::BufferUsages::COPY_SRC;
    }
    if bits & 0x0008 != 0 {
        u |= wgpu::BufferUsages::COPY_DST;
    }
    if bits & 0x0010 != 0 {
        u |= wgpu::BufferUsages::INDEX;
    }
    if bits & 0x0020 != 0 {
        u |= wgpu::BufferUsages::VERTEX;
    }
    if bits & 0x0040 != 0 {
        u |= wgpu::BufferUsages::UNIFORM;
    }
    if bits & 0x0080 != 0 {
        u |= wgpu::BufferUsages::STORAGE;
    }
    if bits & 0x0100 != 0 {
        u |= wgpu::BufferUsages::INDIRECT;
    }
    if bits & 0x0200 != 0 {
        u |= wgpu::BufferUsages::QUERY_RESOLVE;
    }
    u
}

pub fn parse_texture_usages(bits: u32) -> wgpu::TextureUsages {
    let mut u = wgpu::TextureUsages::empty();
    if bits & 0x01 != 0 {
        u |= wgpu::TextureUsages::COPY_SRC;
    }
    if bits & 0x02 != 0 {
        u |= wgpu::TextureUsages::COPY_DST;
    }
    if bits & 0x04 != 0 {
        u |= wgpu::TextureUsages::TEXTURE_BINDING;
    }
    if bits & 0x08 != 0 {
        u |= wgpu::TextureUsages::STORAGE_BINDING;
    }
    if bits & 0x10 != 0 {
        u |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    u
}

pub fn parse_texel_copy_buffer_layout(
    vm: &mut JsVm,
    obj: HeapHandle,
) -> wgpu::TexelCopyBufferLayout {
    wgpu::TexelCopyBufferLayout {
        offset: js_prop_u64(vm, obj, "offset").unwrap_or(0),
        bytes_per_row: js_prop_u32(vm, obj, "bytesPerRow"),
        rows_per_image: js_prop_u32(vm, obj, "rowsPerImage"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_webgpu_formats_usages_and_stages() {
        assert_eq!(
            parse_texture_format("rgba8unorm"),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(
            parse_texture_format("depth24plus-stencil8"),
            Some(wgpu::TextureFormat::Depth24PlusStencil8)
        );
        assert_eq!(parse_texture_format("invalid"), None);
        assert_eq!(
            parse_vertex_format("float32x3"),
            Some(wgpu::VertexFormat::Float32x3)
        );
        assert_eq!(
            parse_buffer_usages(0x0080 | 0x0004),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC
        );
        assert_eq!(
            parse_texture_usages(0x10 | 0x01),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        );
        assert_eq!(
            parse_shader_stages(0x1 | 0x2),
            wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT
        );
    }
}
