//! `WebGL2RenderingContext` (and WebGL1 `"webgl"` compatibility) translating
//! GLSL ES 1.00 / ES 3.00 shaders via `naga` (`rix_render::compile_webgl_program`)
//! onto cached `rix_render::wgpu` render pipelines and compositing directly onto
//! `DisplayItem::ExternalSurface` with zero `DisplayList` rebuilds.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #26.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::NodeId;
use rix_ipc::SurfaceId;
use rix_js::{HeapHandle, JsException, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::surface_id_for_node;
use rix_render::webgl_constants::*;
use rix_render::wgpu;
use rix_render::{
    compile_webgl_program, create_headless_gpu_device, readback_wgpu_texture_rgba,
    CompiledWebGlProgram, Compositor, SharedGpuDevice, VelloRenderer,
};

use crate::canvas2d::{canvas_attr_dim, SharedCanvas2D};
use crate::dom_bridge::{wrap_node, SharedHost};
use crate::web_apis::{
    arg, bind_ctor_proto, define_global, define_host_getter, define_host_method,
};
use crate::webgpu_types::{
    alloc_proto, alloc_with_proto, extract_gpu_buffer_source, js_array_elems,
};

const MAX_TEXTURE_UNITS: usize = 16;

// ---------------------------------------------------------------------------
// Internal WebGL2 GPU state structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct WebGl2Prototypes {
    ctx_proto: Option<HeapHandle>,
    shader_proto: Option<HeapHandle>,
    program_proto: Option<HeapHandle>,
    buffer_proto: Option<HeapHandle>,
    texture_proto: Option<HeapHandle>,
    vao_proto: Option<HeapHandle>,
    uniform_loc_proto: Option<HeapHandle>,
}

#[derive(Debug, Clone)]
struct GlShader {
    stage: u32,
    source: String,
    compiled: bool,
    info_log: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlAttribLayoutSig {
    location: u32,
    slot: u32,
    format: wgpu::VertexFormat,
    offset: u64,
    stride: u64,
    step_mode: wgpu::VertexStepMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GlPipelineKey {
    topology: wgpu::PrimitiveTopology,
    cull_mode: Option<wgpu::Face>,
    front_face: wgpu::FrontFace,
    depth_test: bool,
    depth_write: bool,
    depth_func: wgpu::CompareFunction,
    blend_enabled: bool,
    blend_src_rgb: u32,
    blend_dst_rgb: u32,
    blend_src_alpha: u32,
    blend_dst_alpha: u32,
    blend_eq_rgb: u32,
    blend_eq_alpha: u32,
    color_mask: u32,
    layout: Vec<GlAttribLayoutSig>,
}

struct GlProgram {
    vs_id: Option<u32>,
    fs_id: Option<u32>,
    bound_attribs: HashMap<String, u32>,
    linked: bool,
    info_log: String,
    compiled: Option<CompiledWebGlProgram>,
    vs_module: Option<wgpu::ShaderModule>,
    fs_module: Option<wgpu::ShaderModule>,
    bind_group_layout: Option<wgpu::BindGroupLayout>,
    pipeline_layout: Option<wgpu::PipelineLayout>,
    ubo_data: Vec<u8>,
    ubo_buffer: Option<wgpu::Buffer>,
    sampler_units: Vec<u32>,
    pipeline_cache: HashMap<GlPipelineKey, wgpu::RenderPipeline>,
}

impl GlProgram {
    fn new() -> Self {
        Self {
            vs_id: None,
            fs_id: None,
            bound_attribs: HashMap::new(),
            linked: false,
            info_log: String::new(),
            compiled: None,
            vs_module: None,
            fs_module: None,
            bind_group_layout: None,
            pipeline_layout: None,
            ubo_data: Vec::new(),
            ubo_buffer: None,
            sampler_units: Vec::new(),
            pipeline_cache: HashMap::new(),
        }
    }
}

struct GlBuffer {
    gpu_buffer: Option<wgpu::Buffer>,
    byte_len: usize,
}

struct GlTexture {
    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    width: u32,
    height: u32,
    min_filter: wgpu::FilterMode,
    mag_filter: wgpu::FilterMode,
    wrap_s: wgpu::AddressMode,
    wrap_t: wgpu::AddressMode,
    sampler: Option<wgpu::Sampler>,
}

impl GlTexture {
    fn new() -> Self {
        Self {
            texture: None,
            view: None,
            width: 0,
            height: 0,
            min_filter: wgpu::FilterMode::Linear,
            mag_filter: wgpu::FilterMode::Linear,
            wrap_s: wgpu::AddressMode::Repeat,
            wrap_t: wgpu::AddressMode::Repeat,
            sampler: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GlVertexAttrib {
    enabled: bool,
    buffer_id: Option<u32>,
    size: u32,
    gl_type: u32,
    normalized: bool,
    stride: u64,
    offset: u64,
    divisor: u32,
}

impl Default for GlVertexAttrib {
    fn default() -> Self {
        Self {
            enabled: false,
            buffer_id: None,
            size: 4,
            gl_type: GL_FLOAT,
            normalized: false,
            stride: 0,
            offset: 0,
            divisor: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct GlVaoState {
    element_buffer: Option<u32>,
    attribs: HashMap<u32, GlVertexAttrib>,
}

#[derive(Debug, Clone)]
enum GlUniformTarget {
    Ubo { program_id: u32, offset: usize },
    Sampler { program_id: u32, sampler_idx: usize },
}

struct WebGl2Context {
    surface_id: SurfaceId,
    width: u32,
    height: u32,
    gpu: SharedGpuDevice,
    color_tex: wgpu::Texture,
    color_view: wgpu::TextureView,
    depth_tex: wgpu::Texture,
    depth_view: wgpu::TextureView,
    default_white_view: wgpu::TextureView,
    default_sampler: wgpu::Sampler,
    dirty: bool,
    is_webgl2: bool,

    last_error: u32,
    clear_color: [f64; 4],
    clear_depth: f32,
    clear_stencil: u32,
    viewport: (f32, f32, f32, f32),
    scissor: (u32, u32, u32, u32),
    scissor_enabled: bool,
    depth_test: bool,
    depth_mask: bool,
    depth_func: wgpu::CompareFunction,
    cull_face: bool,
    cull_mode: wgpu::Face,
    front_face: wgpu::FrontFace,
    blend_enabled: bool,
    blend_src_rgb: u32,
    blend_dst_rgb: u32,
    blend_src_alpha: u32,
    blend_dst_alpha: u32,
    blend_eq_rgb: u32,
    blend_eq_alpha: u32,
    color_mask: wgpu::ColorWrites,
    unpack_flip_y: bool,
    unpack_premultiply_alpha: bool,

    active_tex_unit: usize,
    bound_tex_2d: [Option<u32>; MAX_TEXTURE_UNITS],
    bound_array_buffer: Option<u32>,
    default_vao: GlVaoState,
    bound_vao: Option<u32>,
    current_program: Option<u32>,

    next_id: u32,
    shaders: HashMap<u32, GlShader>,
    programs: HashMap<u32, GlProgram>,
    buffers: HashMap<u32, GlBuffer>,
    textures: HashMap<u32, GlTexture>,
    vaos: HashMap<u32, GlVaoState>,
}

impl WebGl2Context {
    fn new(
        surface_id: SurfaceId,
        width: u32,
        height: u32,
        gpu: SharedGpuDevice,
        is_webgl2: bool,
    ) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        let (color_tex, color_view, depth_tex, depth_view) =
            alloc_swapchain_targets(&gpu.device, w, h);

        let default_white_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rix-webgl2-default-white"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &default_white_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let default_white_view =
            default_white_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let default_sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rix-webgl2-default-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            surface_id,
            width: w,
            height: h,
            gpu,
            color_tex,
            color_view,
            depth_tex,
            depth_view,
            default_white_view,
            default_sampler,
            dirty: false,
            is_webgl2,
            last_error: GL_NO_ERROR,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            clear_depth: 1.0,
            clear_stencil: 0,
            viewport: (0.0, 0.0, w as f32, h as f32),
            scissor: (0, 0, w, h),
            scissor_enabled: false,
            depth_test: false,
            depth_mask: true,
            depth_func: wgpu::CompareFunction::Less,
            cull_face: false,
            cull_mode: wgpu::Face::Back,
            front_face: wgpu::FrontFace::Ccw,
            blend_enabled: false,
            blend_src_rgb: GL_ONE,
            blend_dst_rgb: GL_ZERO,
            blend_src_alpha: GL_ONE,
            blend_dst_alpha: GL_ZERO,
            blend_eq_rgb: GL_FUNC_ADD,
            blend_eq_alpha: GL_FUNC_ADD,
            color_mask: wgpu::ColorWrites::ALL,
            unpack_flip_y: false,
            unpack_premultiply_alpha: false,
            active_tex_unit: 0,
            bound_tex_2d: [None; MAX_TEXTURE_UNITS],
            bound_array_buffer: None,
            default_vao: GlVaoState::default(),
            bound_vao: None,
            current_program: None,
            next_id: 1,
            shaders: HashMap::new(),
            programs: HashMap::new(),
            buffers: HashMap::new(),
            textures: HashMap::new(),
            vaos: HashMap::new(),
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn active_vao(&self) -> &GlVaoState {
        self.bound_vao
            .and_then(|id| self.vaos.get(&id))
            .unwrap_or(&self.default_vao)
    }

    fn active_vao_mut(&mut self) -> &mut GlVaoState {
        if let Some(id) = self.bound_vao {
            if self.vaos.contains_key(&id) {
                return self.vaos.get_mut(&id).unwrap();
            }
        }
        &mut self.default_vao
    }

    fn resize(&mut self, width: u32, height: u32) {
        let w = width.max(1);
        let h = height.max(1);
        if self.width == w && self.height == h {
            return;
        }
        self.width = w;
        self.height = h;
        let (ct, cv, dt, dv) = alloc_swapchain_targets(&self.gpu.device, w, h);
        self.color_tex = ct;
        self.color_view = cv;
        self.depth_tex = dt;
        self.depth_view = dv;
        self.viewport = (0.0, 0.0, w as f32, h as f32);
        self.scissor = (0, 0, w, h);
        self.dirty = true;
    }

    fn gl_clear(&mut self, mask: u32) {
        let clear_color = (mask & GL_COLOR_BUFFER_BIT) != 0;
        let clear_depth = (mask & GL_DEPTH_BUFFER_BIT) != 0;
        if !clear_color && !clear_depth {
            return;
        }
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rix-webgl2-clear"),
            });
        let color_load = if clear_color {
            wgpu::LoadOp::Clear(wgpu::Color {
                r: self.clear_color[0],
                g: self.clear_color[1],
                b: self.clear_color[2],
                a: self.clear_color[3],
            })
        } else {
            wgpu::LoadOp::Load
        };
        let depth_load = if clear_depth {
            wgpu::LoadOp::Clear(self.clear_depth)
        } else {
            wgpu::LoadOp::Load
        };
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rix-webgl2-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: color_load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: depth_load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.gpu.queue.submit([encoder.finish()]);
        self.dirty = true;
    }

    fn link_program(&mut self, prog_id: u32) {
        let (vs_src, fs_src, bound_attribs) = {
            let Some(prog) = self.programs.get(&prog_id) else {
                self.last_error = GL_INVALID_VALUE;
                return;
            };
            let vs = prog
                .vs_id
                .and_then(|id| self.shaders.get(&id))
                .map(|s| s.source.clone());
            let fs = prog
                .fs_id
                .and_then(|id| self.shaders.get(&id))
                .map(|s| s.source.clone());
            (vs, fs, prog.bound_attribs.clone())
        };
        let (Some(vs_src), Some(fs_src)) = (vs_src, fs_src) else {
            if let Some(prog) = self.programs.get_mut(&prog_id) {
                prog.linked = false;
                prog.info_log = "Missing vertex or fragment shader".to_string();
            }
            return;
        };

        match compile_webgl_program(&vs_src, &fs_src, &bound_attribs) {
            Ok(compiled) => {
                let vs_module =
                    self.gpu
                        .device
                        .create_shader_module(wgpu::ShaderModuleDescriptor {
                            label: Some("rix-webgl2-vs"),
                            source: wgpu::ShaderSource::Wgsl(compiled.vertex_wgsl.clone().into()),
                        });
                let fs_module =
                    self.gpu
                        .device
                        .create_shader_module(wgpu::ShaderModuleDescriptor {
                            label: Some("rix-webgl2-fs"),
                            source: wgpu::ShaderSource::Wgsl(compiled.fragment_wgsl.clone().into()),
                        });

                let mut bgl_entries = Vec::new();
                let mut ubo_buffer = None;
                let mut ubo_data = Vec::new();
                if compiled.ubo_size > 0 {
                    ubo_data = vec![0u8; compiled.ubo_size as usize];
                    ubo_buffer = Some(self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("rix-webgl2-ubo"),
                        size: u64::from(compiled.ubo_size),
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }));
                    bgl_entries.push(wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    });
                }
                for samp in &compiled.samplers {
                    bgl_entries.push(wgpu::BindGroupLayoutEntry {
                        binding: samp.tex_binding,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    });
                    bgl_entries.push(wgpu::BindGroupLayoutEntry {
                        binding: samp.samp_binding,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    });
                }
                let bgl =
                    self.gpu
                        .device
                        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                            label: Some("rix-webgl2-bgl"),
                            entries: &bgl_entries,
                        });
                let pl = self
                    .gpu
                    .device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("rix-webgl2-pl"),
                        bind_group_layouts: &[Some(&bgl)],
                        immediate_size: 0,
                    });
                let sampler_units = vec![0u32; compiled.samplers.len()];
                if let Some(prog) = self.programs.get_mut(&prog_id) {
                    prog.linked = true;
                    prog.info_log.clear();
                    prog.compiled = Some(compiled);
                    prog.vs_module = Some(vs_module);
                    prog.fs_module = Some(fs_module);
                    prog.bind_group_layout = Some(bgl);
                    prog.pipeline_layout = Some(pl);
                    prog.ubo_data = ubo_data;
                    prog.ubo_buffer = ubo_buffer;
                    prog.sampler_units = sampler_units;
                    prog.pipeline_cache.clear();
                }
            }
            Err(err) => {
                if let Some(prog) = self.programs.get_mut(&prog_id) {
                    prog.linked = false;
                    prog.info_log = err;
                }
            }
        }
    }

    fn draw_common(
        &mut self,
        mode: u32,
        indexed: Option<(u32, u32, u64)>,
        first_vertex: u32,
        count: u32,
        instance_count: u32,
    ) {
        if count == 0 || instance_count == 0 {
            return;
        }
        let Some(prog_id) = self.current_program else {
            return;
        };
        let topology = match mode {
            GL_POINTS => wgpu::PrimitiveTopology::PointList,
            GL_LINES => wgpu::PrimitiveTopology::LineList,
            GL_LINE_STRIP | GL_LINE_LOOP => wgpu::PrimitiveTopology::LineStrip,
            GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN => wgpu::PrimitiveTopology::TriangleStrip,
            _ => wgpu::PrimitiveTopology::TriangleList,
        };

        // Snapshot VAO attributes matching this program's reflected vertex attributes.
        let vao = self.active_vao().clone();
        let prog_attrs = match self
            .programs
            .get(&prog_id)
            .and_then(|p| p.compiled.as_ref())
        {
            Some(c) => c.attributes.clone(),
            None => return,
        };

        // Group enabled vertex attributes by (buffer_id, effective_stride, step_mode).
        let mut slot_buffers: Vec<(u32, u64, wgpu::VertexStepMode, Vec<wgpu::VertexAttribute>)> =
            Vec::new();
        let mut layout_sig: Vec<GlAttribLayoutSig> = Vec::new();
        for attr_ref in &prog_attrs {
            let Some(va) = vao.attribs.get(&attr_ref.location) else {
                continue;
            };
            if !va.enabled {
                continue;
            }
            let Some(buf_id) = va.buffer_id else {
                continue;
            };
            let Some((vfmt, elem_bytes)) = gl_to_vertex_format(va.size, va.gl_type, va.normalized)
            else {
                continue;
            };
            let stride = if va.stride == 0 {
                elem_bytes
            } else {
                va.stride
            };
            let step_mode = if va.divisor > 0 {
                wgpu::VertexStepMode::Instance
            } else {
                wgpu::VertexStepMode::Vertex
            };
            let slot_idx = if let Some(pos) = slot_buffers
                .iter()
                .position(|(b, s, sm, _)| *b == buf_id && *s == stride && *sm == step_mode)
            {
                pos
            } else {
                let pos = slot_buffers.len();
                slot_buffers.push((buf_id, stride, step_mode, Vec::new()));
                pos
            };
            slot_buffers[slot_idx].3.push(wgpu::VertexAttribute {
                format: vfmt,
                offset: va.offset,
                shader_location: attr_ref.location,
            });
            layout_sig.push(GlAttribLayoutSig {
                location: attr_ref.location,
                slot: slot_idx as u32,
                format: vfmt,
                offset: va.offset,
                stride,
                step_mode,
            });
        }

        // Resolve GPU buffers for vertex slots & optional index buffer.
        let mut resolved_vb: Vec<wgpu::Buffer> = Vec::with_capacity(slot_buffers.len());
        for (buf_id, _, _, _) in &slot_buffers {
            let Some(buf) = self.buffers.get(buf_id).and_then(|b| b.gpu_buffer.clone()) else {
                return;
            };
            resolved_vb.push(buf);
        }
        let resolved_ib = if let Some((idx_count, idx_type, idx_off)) = indexed {
            let Some(eb_id) = vao.element_buffer else {
                return;
            };
            let Some(eb) = self.buffers.get(&eb_id).and_then(|b| b.gpu_buffer.clone()) else {
                return;
            };
            let idx_fmt = if idx_type == GL_UNSIGNED_INT {
                wgpu::IndexFormat::Uint32
            } else {
                wgpu::IndexFormat::Uint16
            };
            Some((eb, idx_fmt, idx_off, idx_count))
        } else {
            None
        };

        let key = GlPipelineKey {
            topology,
            cull_mode: self.cull_face.then_some(self.cull_mode),
            front_face: self.front_face,
            depth_test: self.depth_test,
            depth_write: self.depth_test && self.depth_mask,
            depth_func: if self.depth_test {
                self.depth_func
            } else {
                wgpu::CompareFunction::Always
            },
            blend_enabled: self.blend_enabled,
            blend_src_rgb: self.blend_src_rgb,
            blend_dst_rgb: self.blend_dst_rgb,
            blend_src_alpha: self.blend_src_alpha,
            blend_dst_alpha: self.blend_dst_alpha,
            blend_eq_rgb: self.blend_eq_rgb,
            blend_eq_alpha: self.blend_eq_alpha,
            color_mask: self.color_mask.bits(),
            layout: layout_sig,
        };

        // Ensure cached pipeline exists & upload UBO data + collect sampler bindings.
        let (pipeline, bgl, ubo_buf_opt, sampler_specs) = {
            let Some(prog) = self.programs.get_mut(&prog_id) else {
                return;
            };
            if !prog.linked {
                return;
            }
            if let Some(ref ubo_buf) = prog.ubo_buffer {
                if !prog.ubo_data.is_empty() {
                    self.gpu.queue.write_buffer(ubo_buf, 0, &prog.ubo_data);
                }
            }
            if !prog.pipeline_cache.contains_key(&key) {
                let (Some(vs_mod), Some(fs_mod), Some(pl)) = (
                    prog.vs_module.as_ref(),
                    prog.fs_module.as_ref(),
                    prog.pipeline_layout.as_ref(),
                ) else {
                    return;
                };
                let vb_layouts: Vec<wgpu::VertexBufferLayout<'_>> = slot_buffers
                    .iter()
                    .map(|(_, stride, step_mode, attrs)| wgpu::VertexBufferLayout {
                        array_stride: *stride,
                        step_mode: *step_mode,
                        attributes: attrs.as_slice(),
                    })
                    .collect();
                let blend = key.blend_enabled.then(|| wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: gl_to_blend_factor(key.blend_src_rgb),
                        dst_factor: gl_to_blend_factor(key.blend_dst_rgb),
                        operation: gl_to_blend_op(key.blend_eq_rgb),
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: gl_to_blend_factor(key.blend_src_alpha),
                        dst_factor: gl_to_blend_factor(key.blend_dst_alpha),
                        operation: gl_to_blend_op(key.blend_eq_alpha),
                    },
                });
                let pipe =
                    self.gpu
                        .device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: Some("rix-webgl2-pipeline"),
                            layout: Some(pl),
                            vertex: wgpu::VertexState {
                                module: vs_mod,
                                entry_point: Some("main"),
                                compilation_options: wgpu::PipelineCompilationOptions::default(),
                                buffers: &vb_layouts,
                            },
                            primitive: wgpu::PrimitiveState {
                                topology: key.topology,
                                strip_index_format: None,
                                front_face: key.front_face,
                                cull_mode: key.cull_mode,
                                unclipped_depth: false,
                                polygon_mode: wgpu::PolygonMode::Fill,
                                conservative: false,
                            },
                            depth_stencil: Some(wgpu::DepthStencilState {
                                format: wgpu::TextureFormat::Depth24Plus,
                                depth_write_enabled: Some(key.depth_write),
                                depth_compare: Some(key.depth_func),
                                stencil: wgpu::StencilState::default(),
                                bias: wgpu::DepthBiasState::default(),
                            }),
                            multisample: wgpu::MultisampleState::default(),
                            fragment: Some(wgpu::FragmentState {
                                module: fs_mod,
                                entry_point: Some("main"),
                                compilation_options: wgpu::PipelineCompilationOptions::default(),
                                targets: &[Some(wgpu::ColorTargetState {
                                    format: wgpu::TextureFormat::Rgba8Unorm,
                                    blend,
                                    write_mask: self.color_mask,
                                })],
                            }),
                            multiview_mask: None,
                            cache: None,
                        });
                prog.pipeline_cache.insert(key.clone(), pipe);
            }
            let pipe = prog.pipeline_cache.get(&key).unwrap().clone();
            let bgl = prog.bind_group_layout.clone().unwrap();
            let ubo = prog.ubo_buffer.clone();
            let samps: Vec<(u32, u32, usize)> = prog
                .compiled
                .as_ref()
                .unwrap()
                .samplers
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let unit = prog.sampler_units.get(i).copied().unwrap_or(0) as usize;
                    (
                        s.tex_binding,
                        s.samp_binding,
                        unit.min(MAX_TEXTURE_UNITS - 1),
                    )
                })
                .collect();
            (pipe, bgl, ubo, samps)
        };

        // Ensure samplers exist on bound textures and construct BindGroup.
        let mut owned_tex_pairs: Vec<(u32, u32, wgpu::TextureView, wgpu::Sampler)> =
            Vec::with_capacity(sampler_specs.len());
        for (tb, sb, unit) in sampler_specs {
            let (view, sampler) = if let Some(tid) = self.bound_tex_2d[unit] {
                if let Some(tex) = self.textures.get_mut(&tid) {
                    if tex.sampler.is_none() {
                        tex.sampler =
                            Some(self.gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                                label: Some("rix-webgl2-tex-sampler"),
                                address_mode_u: tex.wrap_s,
                                address_mode_v: tex.wrap_t,
                                mag_filter: tex.mag_filter,
                                min_filter: tex.min_filter,
                                ..Default::default()
                            }));
                    }
                    (
                        tex.view
                            .clone()
                            .unwrap_or_else(|| self.default_white_view.clone()),
                        tex.sampler
                            .clone()
                            .unwrap_or_else(|| self.default_sampler.clone()),
                    )
                } else {
                    (
                        self.default_white_view.clone(),
                        self.default_sampler.clone(),
                    )
                }
            } else {
                (
                    self.default_white_view.clone(),
                    self.default_sampler.clone(),
                )
            };
            owned_tex_pairs.push((tb, sb, view, sampler));
        }

        let mut bg_entries: Vec<wgpu::BindGroupEntry<'_>> = Vec::new();
        if let Some(ref ubo_buf) = ubo_buf_opt {
            bg_entries.push(wgpu::BindGroupEntry {
                binding: 0,
                resource: ubo_buf.as_entire_binding(),
            });
        }
        for (tb, sb, view, sampler) in &owned_tex_pairs {
            bg_entries.push(wgpu::BindGroupEntry {
                binding: *tb,
                resource: wgpu::BindingResource::TextureView(view),
            });
            bg_entries.push(wgpu::BindGroupEntry {
                binding: *sb,
                resource: wgpu::BindingResource::Sampler(sampler),
            });
        }
        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rix-webgl2-bg"),
                layout: &bgl,
                entries: &bg_entries,
            });

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rix-webgl2-draw"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rix-webgl2-draw-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Convert OpenGL bottom-left viewport Y to WebGPU top-left Y.
            let (vx, vy, vw, vh) = self.viewport;
            let top_y = (self.height as f32 - (vy + vh)).clamp(0.0, self.height as f32);
            let clamp_w = vw.clamp(1.0, self.width as f32);
            let clamp_h = vh.clamp(1.0, self.height as f32);
            pass.set_viewport(vx.max(0.0), top_y, clamp_w, clamp_h, 0.0, 1.0);
            if self.scissor_enabled {
                let (sx, sy, sw, sh) = self.scissor;
                let s_top = self.height.saturating_sub(sy.saturating_add(sh));
                let s_w = sw.min(self.width.saturating_sub(sx));
                let s_h = sh.min(self.height.saturating_sub(s_top));
                pass.set_scissor_rect(sx.min(self.width), s_top, s_w, s_h);
            }
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            for (slot, vb) in resolved_vb.iter().enumerate() {
                pass.set_vertex_buffer(slot as u32, vb.slice(..));
            }
            if let Some((ib, idx_fmt, idx_off, idx_count)) = resolved_ib {
                pass.set_index_buffer(ib.slice(idx_off..), idx_fmt);
                pass.draw_indexed(0..idx_count, 0, 0..instance_count);
            } else {
                pass.draw(first_vertex..(first_vertex + count), 0..instance_count);
            }
        }
        self.gpu.queue.submit([encoder.finish()]);
        self.dirty = true;
    }
}

fn alloc_swapchain_targets(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (
    wgpu::Texture,
    wgpu::TextureView,
    wgpu::Texture,
    wgpu::TextureView,
) {
    let size = wgpu::Extent3d {
        width: width.max(1),
        height: height.max(1),
        depth_or_array_layers: 1,
    };
    let color_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rix-webgl2-color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rix-webgl2-depth"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth24Plus,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth_tex.create_view(&wgpu::TextureViewDescriptor::default());
    (color_tex, color_view, depth_tex, depth_view)
}

// ---------------------------------------------------------------------------
// Shared WebGL2 system registry
// ---------------------------------------------------------------------------

/// Shared WebGL2 / WebGL1 registry owned by [`crate::ContentRuntime`].
pub struct WebGl2System {
    shared_gpu: Option<SharedGpuDevice>,
    protos: WebGl2Prototypes,
    contexts: HashMap<NodeId, WebGl2Context>,
    ctx_handle_to_node: HashMap<HeapHandle, NodeId>,
    node_to_ctx_handle: HashMap<NodeId, HeapHandle>,
    shader_handles: HashMap<HeapHandle, (NodeId, u32)>,
    program_handles: HashMap<HeapHandle, (NodeId, u32)>,
    buffer_handles: HashMap<HeapHandle, (NodeId, u32)>,
    texture_handles: HashMap<HeapHandle, (NodeId, u32)>,
    vao_handles: HashMap<HeapHandle, (NodeId, u32)>,
    uniform_handles: HashMap<HeapHandle, (NodeId, GlUniformTarget)>,
}

/// Shared handle to [`WebGl2System`].
pub type SharedWebGl2 = Rc<RefCell<WebGl2System>>;

impl Default for WebGl2System {
    fn default() -> Self {
        Self::new()
    }
}

impl WebGl2System {
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared_gpu: None,
            protos: WebGl2Prototypes::default(),
            contexts: HashMap::new(),
            ctx_handle_to_node: HashMap::new(),
            node_to_ctx_handle: HashMap::new(),
            shader_handles: HashMap::new(),
            program_handles: HashMap::new(),
            buffer_handles: HashMap::new(),
            texture_handles: HashMap::new(),
            vao_handles: HashMap::new(),
            uniform_handles: HashMap::new(),
        }
    }

    /// Attaches a compositor-owned [`SharedGpuDevice`] for zero-copy GPU-to-GPU presentation.
    pub fn attach_shared_gpu(&mut self, shared: SharedGpuDevice) {
        self.shared_gpu = Some(shared);
    }

    /// Presents all dirty WebGL2 canvas textures to `compositor` with ZERO `DisplayList` rebuilds.
    pub fn present_dirty_to_compositor(
        &mut self,
        compositor: &mut Compositor,
        mut gpu: Option<&mut VelloRenderer>,
    ) -> usize {
        let mut presented = 0usize;
        for ctx in self.contexts.values_mut() {
            if !ctx.dirty {
                continue;
            }
            let zero_copy_done = if let Some(ref mut renderer) = gpu {
                if ctx.gpu.renderer_token != 0
                    && ctx.gpu.renderer_token == renderer.renderer_token()
                {
                    if let Ok(img) = renderer
                        .register_external_gpu_texture(ctx.surface_id, ctx.color_tex.clone())
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
                if let Ok(rgba) =
                    readback_wgpu_texture_rgba(&ctx.gpu.device, &ctx.gpu.queue, &ctx.color_tex)
                {
                    compositor.present_external_surface_rgba(
                        ctx.surface_id,
                        ctx.width,
                        ctx.height,
                        rgba,
                    );
                }
            }
            ctx.dirty = false;
            presented += 1;
        }
        presented
    }

    #[must_use]
    pub fn readback_canvas_rgba(&self, node: NodeId) -> Option<(u32, u32, Vec<u8>)> {
        let ctx = self.contexts.get(&node)?;
        let rgba =
            readback_wgpu_texture_rgba(&ctx.gpu.device, &ctx.gpu.queue, &ctx.color_tex).ok()?;
        Some((ctx.width, ctx.height, rgba))
    }

    pub fn resize_canvas(&mut self, node: NodeId, width: u32, height: u32) {
        if let Some(ctx) = self.contexts.get_mut(&node) {
            ctx.resize(width, height);
        }
    }

    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.ctx_proto,
            self.protos.shader_proto,
            self.protos.program_proto,
            self.protos.buffer_proto,
            self.protos.texture_proto,
            self.protos.vao_proto,
            self.protos.uniform_loc_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for &h in self.node_to_ctx_handle.values() {
            out.push(JsValue::from_object(h));
        }
    }

    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.ctx_handle_to_node.retain(|&h, _| heap.is_alive(h));
        self.node_to_ctx_handle.retain(|_, &mut h| heap.is_alive(h));
        self.shader_handles.retain(|&h, _| heap.is_alive(h));
        self.program_handles.retain(|&h, _| heap.is_alive(h));
        self.buffer_handles.retain(|&h, _| heap.is_alive(h));
        self.texture_handles.retain(|&h, _| heap.is_alive(h));
        self.vao_handles.retain(|&h, _| heap.is_alive(h));
        self.uniform_handles.retain(|&h, _| heap.is_alive(h));
    }
}

/// Installs `WebGL2RenderingContext`, `WebGLRenderingContext`, WebGL resource constructors,
/// and `canvas.getContext("webgl2" | "webgl" | "experimental-webgl")` hooks onto `vm`.
pub fn install_webgl2(vm: &mut JsVm, host: &SharedHost, canvas2d: &SharedCanvas2D) -> SharedWebGl2 {
    let sys = Rc::new(RefCell::new(WebGl2System::new()));
    install_webgl2_prototypes(vm, host, &sys);
    register_canvas_webgl2_hooks(host, canvas2d, &sys);
    sys
}

// ---------------------------------------------------------------------------
// JS binding helpers
// ---------------------------------------------------------------------------

fn require_ctx_node(
    vm: &mut JsVm,
    sys: &SharedWebGl2,
    this_val: JsValue,
) -> Result<NodeId, JsException> {
    let h = this_val
        .as_object()
        .ok_or_else(|| vm.type_error("Incompatible WebGL2RenderingContext receiver"))?;
    sys.borrow()
        .ctx_handle_to_node
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible WebGL2RenderingContext receiver"))
}

fn extract_f32_seq(vm: &mut JsVm, val: JsValue) -> Vec<f32> {
    if let Some(h) = val.as_object() {
        if let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(h).map(|o| &o.kind) {
            let ta = *ta;
            let bpe = ta.kind.bytes_per_element();
            if bpe == 4 {
                if let Some(ObjectKind::ArrayBuffer(buf)) =
                    vm.heap.get_object(ta.buffer).map(|o| &o.kind)
                {
                    let start = (ta.byte_offset as usize).min(buf.len());
                    let end = start
                        .saturating_add((ta.length as usize) * 4)
                        .min(buf.len());
                    return buf[start..end]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes(*c))
                        .collect();
                }
            }
        }
    }
    js_array_elems(vm, val)
        .into_iter()
        .filter_map(|v| vm.to_number(v).ok().map(|n| n as f32))
        .collect()
}

fn extract_i32_seq(vm: &mut JsVm, val: JsValue) -> Vec<i32> {
    if let Some(h) = val.as_object() {
        if let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(h).map(|o| &o.kind) {
            let ta = *ta;
            if ta.kind.bytes_per_element() == 4 {
                if let Some(ObjectKind::ArrayBuffer(buf)) =
                    vm.heap.get_object(ta.buffer).map(|o| &o.kind)
                {
                    let start = (ta.byte_offset as usize).min(buf.len());
                    let end = start
                        .saturating_add((ta.length as usize) * 4)
                        .min(buf.len());
                    return buf[start..end]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| i32::from_le_bytes(*c))
                        .collect();
                }
            }
        }
    }
    js_array_elems(vm, val)
        .into_iter()
        .filter_map(|v| vm.to_number(v).ok().map(|n| n.trunc() as i32))
        .collect()
}

fn write_ubo_f32s(prog: &mut GlProgram, offset: usize, vals: &[f32]) {
    let byte_len = vals.len() * 4;
    if offset + byte_len > prog.ubo_data.len() {
        return;
    }
    for (i, &v) in vals.iter().enumerate() {
        let off = offset + i * 4;
        prog.ubo_data[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
}

fn write_ubo_i32s(prog: &mut GlProgram, offset: usize, vals: &[i32]) {
    let byte_len = vals.len() * 4;
    if offset + byte_len > prog.ubo_data.len() {
        return;
    }
    for (i, &v) in vals.iter().enumerate() {
        let off = offset + i * 4;
        prog.ubo_data[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
}

fn write_ubo_matrix(
    prog: &mut GlProgram,
    offset: usize,
    dim: usize,
    transpose: bool,
    vals: &[f32],
) {
    if vals.len() < dim * dim {
        return;
    }
    // std140 column stride: mat2 = 8 bytes (WGSL mat2x2) or 16; mat3 = 16; mat4 = 16.
    let col_stride = if dim == 2 { 8 } else { 16 };
    for c in 0..dim {
        for r in 0..dim {
            let v = if transpose {
                vals[r * dim + c]
            } else {
                vals[c * dim + r]
            };
            let off = offset + c * col_stride + r * 4;
            if off + 4 <= prog.ubo_data.len() {
                prog.ubo_data[off..off + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prototype installation
// ---------------------------------------------------------------------------

fn define_gl_constants_on(vm: &mut JsVm, target: HeapHandle) {
    for &(name, val) in WEBGL_CONSTANTS {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            target,
            k,
            JsValue::from_number(f64::from(val)),
            PropertyAttrs::READONLY,
            &mut vm.shapes,
        );
    }
}

fn install_webgl2_prototypes(vm: &mut JsVm, host: &SharedHost, sys: &SharedWebGl2) {
    let ctx_p = alloc_proto(vm);
    let shader_p = alloc_proto(vm);
    let prog_p = alloc_proto(vm);
    let buf_p = alloc_proto(vm);
    let tex_p = alloc_proto(vm);
    let vao_p = alloc_proto(vm);
    let uloc_p = alloc_proto(vm);

    sys.borrow_mut().protos = WebGl2Prototypes {
        ctx_proto: Some(ctx_p),
        shader_proto: Some(shader_p),
        program_proto: Some(prog_p),
        buffer_proto: Some(buf_p),
        texture_proto: Some(tex_p),
        vao_proto: Some(vao_p),
        uniform_loc_proto: Some(uloc_p),
    };

    define_gl_constants_on(vm, ctx_p);
    install_state_and_shader_methods(vm, host, sys, ctx_p);
    install_buffer_vao_and_uniform_methods(vm, sys, ctx_p);
    install_texture_and_draw_methods(vm, sys, ctx_p);

    let ctor2 = vm.alloc_host_fn("WebGL2RenderingContext", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WebGL2RenderingContext"))
    });
    define_gl_constants_on(vm, ctor2);
    bind_ctor_proto(vm, ctor2, ctx_p);
    define_global(vm, "WebGL2RenderingContext", JsValue::from_object(ctor2));

    let ctor1 = vm.alloc_host_fn("WebGLRenderingContext", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WebGLRenderingContext"))
    });
    define_gl_constants_on(vm, ctor1);
    bind_ctor_proto(vm, ctor1, ctx_p);
    define_global(vm, "WebGLRenderingContext", JsValue::from_object(ctor1));

    for (name, proto) in [
        ("WebGLShader", shader_p),
        ("WebGLProgram", prog_p),
        ("WebGLBuffer", buf_p),
        ("WebGLTexture", tex_p),
        ("WebGLVertexArrayObject", vao_p),
        ("WebGLUniformLocation", uloc_p),
    ] {
        let c = vm.alloc_host_fn(name, move |vm, _, _| {
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        });
        bind_ctor_proto(vm, c, proto);
        define_global(vm, name, JsValue::from_object(c));
    }
}

fn install_state_and_shader_methods(
    vm: &mut JsVm,
    host: &SharedHost,
    sys: &SharedWebGl2,
    p: HeapHandle,
) {
    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    define_host_getter(vm, p, "canvas", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        Ok(wrap_node(vm, &h, nid))
    });

    let s = Rc::clone(sys);
    define_host_getter(vm, p, "drawingBufferWidth", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let w = s.borrow().contexts.get(&nid).map_or(0, |c| c.width);
        Ok(JsValue::from_number(f64::from(w)))
    });

    let s = Rc::clone(sys);
    define_host_getter(vm, p, "drawingBufferHeight", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let h_px = s.borrow().contexts.get(&nid).map_or(0, |c| c.height);
        Ok(JsValue::from_number(f64::from(h_px)))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getError", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let err = s
            .borrow_mut()
            .contexts
            .get_mut(&nid)
            .map_or(GL_NO_ERROR, |c| {
                std::mem::replace(&mut c.last_error, GL_NO_ERROR)
            });
        Ok(JsValue::from_number(f64::from(err)))
    });

    define_host_method(vm, p, "isContextLost", |_, _, _| {
        Ok(JsValue::from_bool(false))
    });
    define_host_method(vm, p, "flush", |_, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, p, "finish", |_, _, _| Ok(JsValue::UNDEFINED));
    define_host_method(vm, p, "lineWidth", |_, _, _| Ok(JsValue::UNDEFINED));

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getParameter", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let pname = vm.to_u32(arg(args, 0))?;
        let st = s.borrow();
        let Some(ctx) = st.contexts.get(&nid) else {
            return Ok(JsValue::NULL);
        };
        Ok(match pname {
            GL_VENDOR => JsValue::from_string(vm.heap.alloc_string("RixBrowser (wgpu)")),
            GL_RENDERER => {
                let name = ctx.gpu.adapter_info().name;
                JsValue::from_string(vm.heap.alloc_string(format!("RixWebGL2 / {name}")))
            }
            GL_VERSION => JsValue::from_string(vm.heap.alloc_string(if ctx.is_webgl2 {
                "WebGL 2.0 (RixBrowser wgpu)"
            } else {
                "WebGL 1.0 (RixBrowser wgpu)"
            })),
            GL_SHADING_LANGUAGE_VERSION => {
                JsValue::from_string(vm.heap.alloc_string(if ctx.is_webgl2 {
                    "WebGL GLSL ES 3.00 (Naga)"
                } else {
                    "WebGL GLSL ES 1.00 (Naga)"
                }))
            }
            GL_MAX_TEXTURE_SIZE => {
                JsValue::from_number(f64::from(ctx.gpu.limits().max_texture_dimension_2d))
            }
            GL_MAX_VERTEX_ATTRIBS => {
                JsValue::from_number(f64::from(ctx.gpu.limits().max_vertex_attributes))
            }
            _ => JsValue::NULL,
        })
    });

    // Viewport, scissor, clear state, enable/disable, depth/cull/blend
    let s = Rc::clone(sys);
    define_host_method(vm, p, "viewport", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let x = vm.to_number(arg(args, 0))? as f32;
        let y = vm.to_number(arg(args, 1))? as f32;
        let w = vm.to_number(arg(args, 2))?.max(1.0) as f32;
        let h_px = vm.to_number(arg(args, 3))?.max(1.0) as f32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.viewport = (x, y, w, h_px);
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "scissor", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let x = vm.to_number(arg(args, 0))?.max(0.0) as u32;
        let y = vm.to_number(arg(args, 1))?.max(0.0) as u32;
        let w = vm.to_number(arg(args, 2))?.max(0.0) as u32;
        let h_px = vm.to_number(arg(args, 3))?.max(0.0) as u32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.scissor = (x, y, w, h_px);
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "clearColor", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let r = vm.to_number(arg(args, 0))?.clamp(0.0, 1.0);
        let g = vm.to_number(arg(args, 1))?.clamp(0.0, 1.0);
        let b = vm.to_number(arg(args, 2))?.clamp(0.0, 1.0);
        let a = vm.to_number(arg(args, 3))?.clamp(0.0, 1.0);
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.clear_color = [r, g, b, a];
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "clearDepth", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let d = vm.to_number(arg(args, 0))?.clamp(0.0, 1.0) as f32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.clear_depth = d;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "clearStencil", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let v = vm.to_u32(arg(args, 0))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.clear_stencil = v;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "clear", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mask = vm.to_u32(arg(args, 0))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.gl_clear(mask);
        }
        Ok(JsValue::UNDEFINED)
    });

    for (name, flag) in [("enable", true), ("disable", false)] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let cap = vm.to_u32(arg(args, 0))?;
            if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
                match cap {
                    GL_DEPTH_TEST => ctx.depth_test = flag,
                    GL_CULL_FACE => ctx.cull_face = flag,
                    GL_BLEND => ctx.blend_enabled = flag,
                    GL_SCISSOR_TEST => ctx.scissor_enabled = flag,
                    _ => {}
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let s = Rc::clone(sys);
    define_host_method(vm, p, "depthFunc", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let func = gl_to_compare(vm.to_u32(arg(args, 0))?);
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.depth_func = func;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "depthMask", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mask = vm.to_boolean(arg(args, 0));
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.depth_mask = mask;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "cullFace", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.cull_mode = if mode == GL_FRONT {
                wgpu::Face::Front
            } else {
                wgpu::Face::Back
            };
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "frontFace", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.front_face = if mode == GL_CW {
                wgpu::FrontFace::Cw
            } else {
                wgpu::FrontFace::Ccw
            };
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "blendFunc", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let src = vm.to_u32(arg(args, 0))?;
        let dst = vm.to_u32(arg(args, 1))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.blend_src_rgb = src;
            ctx.blend_dst_rgb = dst;
            ctx.blend_src_alpha = src;
            ctx.blend_dst_alpha = dst;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "blendFuncSeparate", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let sr = vm.to_u32(arg(args, 0))?;
        let dr = vm.to_u32(arg(args, 1))?;
        let sa = vm.to_u32(arg(args, 2))?;
        let da = vm.to_u32(arg(args, 3))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.blend_src_rgb = sr;
            ctx.blend_dst_rgb = dr;
            ctx.blend_src_alpha = sa;
            ctx.blend_dst_alpha = da;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "blendEquation", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let eq = vm.to_u32(arg(args, 0))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.blend_eq_rgb = eq;
            ctx.blend_eq_alpha = eq;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "colorMask", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mut mask = wgpu::ColorWrites::empty();
        if vm.to_boolean(arg(args, 0)) {
            mask |= wgpu::ColorWrites::RED;
        }
        if vm.to_boolean(arg(args, 1)) {
            mask |= wgpu::ColorWrites::GREEN;
        }
        if vm.to_boolean(arg(args, 2)) {
            mask |= wgpu::ColorWrites::BLUE;
        }
        if vm.to_boolean(arg(args, 3)) {
            mask |= wgpu::ColorWrites::ALPHA;
        }
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.color_mask = mask;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "pixelStorei", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let pname = vm.to_u32(arg(args, 0))?;
        let val = vm.to_boolean(arg(args, 1));
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            if pname == GL_UNPACK_FLIP_Y_WEBGL {
                ctx.unpack_flip_y = val;
            } else if pname == GL_UNPACK_PREMULTIPLY_ALPHA_WEBGL {
                ctx.unpack_premultiply_alpha = val;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // Shaders & Programs
    let s = Rc::clone(sys);
    define_host_method(vm, p, "createShader", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let stage = vm.to_u32(arg(args, 0))?;
        let proto = s.borrow().protos.shader_proto;
        let obj = alloc_with_proto(vm, proto);
        let mut st = s.borrow_mut();
        if let Some(ctx) = st.contexts.get_mut(&nid) {
            let id = ctx.alloc_id();
            ctx.shaders.insert(
                id,
                GlShader {
                    stage,
                    source: String::new(),
                    compiled: false,
                    info_log: String::new(),
                },
            );
            st.shader_handles.insert(obj, (nid, id));
        }
        Ok(JsValue::from_object(obj))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "shaderSource", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(sh_h) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let src = vm.value_to_rust_string(arg(args, 1));
        let mut st = s.borrow_mut();
        if let Some(&(snid, id)) = st.shader_handles.get(&sh_h) {
            if snid == nid {
                if let Some(sh) = st
                    .contexts
                    .get_mut(&nid)
                    .and_then(|c| c.shaders.get_mut(&id))
                {
                    sh.source = src;
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "compileShader", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(sh_h) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let mut st = s.borrow_mut();
        if let Some(&(snid, id)) = st.shader_handles.get(&sh_h) {
            if snid == nid {
                if let Some(sh) = st
                    .contexts
                    .get_mut(&nid)
                    .and_then(|c| c.shaders.get_mut(&id))
                {
                    sh.compiled = !sh.source.trim().is_empty();
                    sh.info_log = if sh.compiled {
                        String::new()
                    } else {
                        "Empty shader source".to_string()
                    };
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getShaderParameter", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(sh_h) = arg(args, 0).as_object() else {
            return Ok(JsValue::NULL);
        };
        let pname = vm.to_u32(arg(args, 1))?;
        let st = s.borrow();
        let Some(&(snid, id)) = st.shader_handles.get(&sh_h) else {
            return Ok(JsValue::NULL);
        };
        if snid != nid {
            return Ok(JsValue::NULL);
        }
        let Some(sh) = st.contexts.get(&nid).and_then(|c| c.shaders.get(&id)) else {
            return Ok(JsValue::NULL);
        };
        if pname == GL_COMPILE_STATUS {
            Ok(JsValue::from_bool(sh.compiled))
        } else {
            Ok(JsValue::from_number(f64::from(sh.stage)))
        }
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getShaderInfoLog", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let log = arg(args, 0)
            .as_object()
            .and_then(|h| {
                let st = s.borrow();
                let &(snid, id) = st.shader_handles.get(&h)?;
                if snid != nid {
                    return None;
                }
                st.contexts
                    .get(&nid)
                    .and_then(|c| c.shaders.get(&id).map(|sh| sh.info_log.clone()))
            })
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(log)))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "deleteShader", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        if let Some(h) = arg(args, 0).as_object() {
            let mut st = s.borrow_mut();
            if let Some((snid, id)) = st.shader_handles.remove(&h) {
                if snid == nid {
                    if let Some(ctx) = st.contexts.get_mut(&nid) {
                        ctx.shaders.remove(&id);
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "createProgram", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let proto = s.borrow().protos.program_proto;
        let obj = alloc_with_proto(vm, proto);
        let mut st = s.borrow_mut();
        if let Some(ctx) = st.contexts.get_mut(&nid) {
            let id = ctx.alloc_id();
            ctx.programs.insert(id, GlProgram::new());
            st.program_handles.insert(obj, (nid, id));
        }
        Ok(JsValue::from_object(obj))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "attachShader", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let (Some(ph), Some(sh)) = (arg(args, 0).as_object(), arg(args, 1).as_object()) else {
            return Ok(JsValue::UNDEFINED);
        };
        let mut st = s.borrow_mut();
        let (Some(&(pnid, pid)), Some(&(snid, sid))) =
            (st.program_handles.get(&ph), st.shader_handles.get(&sh))
        else {
            return Ok(JsValue::UNDEFINED);
        };
        if pnid == nid && snid == nid {
            if let Some(ctx) = st.contexts.get_mut(&nid) {
                if let Some(stage) = ctx.shaders.get(&sid).map(|x| x.stage) {
                    if let Some(prog) = ctx.programs.get_mut(&pid) {
                        if stage == GL_VERTEX_SHADER {
                            prog.vs_id = Some(sid);
                        } else if stage == GL_FRAGMENT_SHADER {
                            prog.fs_id = Some(sid);
                        }
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bindAttribLocation", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(ph) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let index = vm.to_u32(arg(args, 1))?;
        let name = vm.value_to_rust_string(arg(args, 2));
        let mut st = s.borrow_mut();
        if let Some(&(pnid, pid)) = st.program_handles.get(&ph) {
            if pnid == nid {
                if let Some(prog) = st
                    .contexts
                    .get_mut(&nid)
                    .and_then(|c| c.programs.get_mut(&pid))
                {
                    prog.bound_attribs.insert(name, index);
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "linkProgram", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(ph) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let mut st = s.borrow_mut();
        if let Some(&(pnid, pid)) = st.program_handles.get(&ph) {
            if pnid == nid {
                if let Some(ctx) = st.contexts.get_mut(&nid) {
                    ctx.link_program(pid);
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getProgramParameter", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(ph) = arg(args, 0).as_object() else {
            return Ok(JsValue::from_bool(false));
        };
        let pname = vm.to_u32(arg(args, 1))?;
        let st = s.borrow();
        let linked = st
            .program_handles
            .get(&ph)
            .filter(|(pnid, _)| *pnid == nid)
            .and_then(|(_, pid)| st.contexts.get(&nid).and_then(|c| c.programs.get(pid)))
            .is_some_and(|prog| prog.linked);
        if pname == GL_LINK_STATUS || pname == GL_VALIDATE_STATUS {
            Ok(JsValue::from_bool(linked))
        } else {
            Ok(JsValue::from_i32(0))
        }
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getProgramInfoLog", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let log = arg(args, 0)
            .as_object()
            .and_then(|ph| {
                let st = s.borrow();
                let &(pnid, pid) = st.program_handles.get(&ph)?;
                if pnid != nid {
                    return None;
                }
                st.contexts
                    .get(&nid)
                    .and_then(|c| c.programs.get(&pid).map(|pr| pr.info_log.clone()))
            })
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(log)))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "useProgram", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let pid_opt = arg(args, 0).as_object().and_then(|ph| {
            s.borrow()
                .program_handles
                .get(&ph)
                .filter(|(pnid, _)| *pnid == nid)
                .map(|(_, pid)| *pid)
        });
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.current_program = pid_opt;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "deleteProgram", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        if let Some(ph) = arg(args, 0).as_object() {
            let mut st = s.borrow_mut();
            if let Some((pnid, pid)) = st.program_handles.remove(&ph) {
                if pnid == nid {
                    if let Some(ctx) = st.contexts.get_mut(&nid) {
                        ctx.programs.remove(&pid);
                        if ctx.current_program == Some(pid) {
                            ctx.current_program = None;
                        }
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getAttribLocation", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(ph) = arg(args, 0).as_object() else {
            return Ok(JsValue::from_i32(-1));
        };
        let name = vm.value_to_rust_string(arg(args, 1));
        let st = s.borrow();
        let loc = st
            .program_handles
            .get(&ph)
            .filter(|(pnid, _)| *pnid == nid)
            .and_then(|(_, pid)| {
                st.contexts
                    .get(&nid)
                    .and_then(|c| c.programs.get(pid))
                    .and_then(|pr| pr.compiled.as_ref())
                    .and_then(|cmp| cmp.attributes.iter().find(|a| a.name == name))
                    .map(|a| a.location as i32)
            })
            .unwrap_or(-1);
        Ok(JsValue::from_i32(loc))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "getUniformLocation", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(ph) = arg(args, 0).as_object() else {
            return Ok(JsValue::NULL);
        };
        let name = vm.value_to_rust_string(arg(args, 1));
        let target = {
            let st = s.borrow();
            let Some(&(pnid, pid)) = st.program_handles.get(&ph) else {
                return Ok(JsValue::NULL);
            };
            if pnid != nid {
                return Ok(JsValue::NULL);
            }
            let Some(cmp) = st
                .contexts
                .get(&nid)
                .and_then(|c| c.programs.get(&pid))
                .and_then(|pr| pr.compiled.as_ref())
            else {
                return Ok(JsValue::NULL);
            };
            if let Some(u) = cmp.uniforms.iter().find(|u| u.name == name) {
                Some(GlUniformTarget::Ubo {
                    program_id: pid,
                    offset: u.offset as usize,
                })
            } else {
                cmp.samplers
                    .iter()
                    .position(|sm| sm.name == name)
                    .map(|idx| GlUniformTarget::Sampler {
                        program_id: pid,
                        sampler_idx: idx,
                    })
            }
        };
        let Some(tgt) = target else {
            return Ok(JsValue::NULL);
        };
        let proto = s.borrow().protos.uniform_loc_proto;
        let obj = alloc_with_proto(vm, proto);
        s.borrow_mut().uniform_handles.insert(obj, (nid, tgt));
        Ok(JsValue::from_object(obj))
    });
}

fn install_buffer_vao_and_uniform_methods(vm: &mut JsVm, sys: &SharedWebGl2, p: HeapHandle) {
    // Buffers
    let s = Rc::clone(sys);
    define_host_method(vm, p, "createBuffer", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let proto = s.borrow().protos.buffer_proto;
        let obj = alloc_with_proto(vm, proto);
        let mut st = s.borrow_mut();
        if let Some(ctx) = st.contexts.get_mut(&nid) {
            let id = ctx.alloc_id();
            ctx.buffers.insert(
                id,
                GlBuffer {
                    gpu_buffer: None,
                    byte_len: 0,
                },
            );
            st.buffer_handles.insert(obj, (nid, id));
        }
        Ok(JsValue::from_object(obj))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bindBuffer", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        let bid = arg(args, 1).as_object().and_then(|bh| {
            s.borrow()
                .buffer_handles
                .get(&bh)
                .filter(|(bnid, _)| *bnid == nid)
                .map(|(_, id)| *id)
        });
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            if target == GL_ARRAY_BUFFER {
                ctx.bound_array_buffer = bid;
            } else if target == GL_ELEMENT_ARRAY_BUFFER {
                ctx.active_vao_mut().element_buffer = bid;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bufferData", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        let src_val = arg(args, 1);
        let bytes = if let Some(b) = extract_gpu_buffer_source(vm, src_val, None, None) {
            b
        } else {
            let sz = vm.to_number(src_val)?.max(0.0) as usize;
            vec![0u8; sz]
        };
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            let bid = if target == GL_ELEMENT_ARRAY_BUFFER {
                ctx.active_vao().element_buffer
            } else {
                ctx.bound_array_buffer
            };
            if let Some(id) = bid {
                let padded_len = bytes.len().max(4).div_ceil(4) * 4;
                let gpu_buf = ctx.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rix-webgl2-buffer"),
                    size: padded_len as u64,
                    usage: wgpu::BufferUsages::VERTEX
                        | wgpu::BufferUsages::INDEX
                        | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                if !bytes.is_empty() {
                    let mut upload = bytes.clone();
                    upload.resize(padded_len, 0);
                    ctx.gpu.queue.write_buffer(&gpu_buf, 0, &upload);
                }
                if let Some(entry) = ctx.buffers.get_mut(&id) {
                    entry.gpu_buffer = Some(gpu_buf);
                    entry.byte_len = bytes.len();
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bufferSubData", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        let offset = vm.to_number(arg(args, 1))?.max(0.0) as u64;
        let Some(bytes) = extract_gpu_buffer_source(vm, arg(args, 2), None, None) else {
            return Ok(JsValue::UNDEFINED);
        };
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            let bid = if target == GL_ELEMENT_ARRAY_BUFFER {
                ctx.active_vao().element_buffer
            } else {
                ctx.bound_array_buffer
            };
            if let Some(id) = bid {
                if let Some(buf) = ctx.buffers.get(&id).and_then(|b| b.gpu_buffer.as_ref()) {
                    let aligned_len = bytes.len().div_ceil(4) * 4;
                    let mut upload = bytes;
                    upload.resize(aligned_len, 0);
                    ctx.gpu.queue.write_buffer(buf, offset, &upload);
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "deleteBuffer", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        if let Some(bh) = arg(args, 0).as_object() {
            let mut st = s.borrow_mut();
            if let Some((bnid, id)) = st.buffer_handles.remove(&bh) {
                if bnid == nid {
                    if let Some(ctx) = st.contexts.get_mut(&nid) {
                        ctx.buffers.remove(&id);
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // VAOs & Vertex Attributes
    let s = Rc::clone(sys);
    define_host_method(vm, p, "createVertexArray", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let proto = s.borrow().protos.vao_proto;
        let obj = alloc_with_proto(vm, proto);
        let mut st = s.borrow_mut();
        if let Some(ctx) = st.contexts.get_mut(&nid) {
            let id = ctx.alloc_id();
            ctx.vaos.insert(id, GlVaoState::default());
            st.vao_handles.insert(obj, (nid, id));
        }
        Ok(JsValue::from_object(obj))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bindVertexArray", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let vid = arg(args, 0).as_object().and_then(|vh| {
            s.borrow()
                .vao_handles
                .get(&vh)
                .filter(|(vnid, _)| *vnid == nid)
                .map(|(_, id)| *id)
        });
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.bound_vao = vid;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "deleteVertexArray", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        if let Some(vh) = arg(args, 0).as_object() {
            let mut st = s.borrow_mut();
            if let Some((vnid, id)) = st.vao_handles.remove(&vh) {
                if vnid == nid {
                    if let Some(ctx) = st.contexts.get_mut(&nid) {
                        ctx.vaos.remove(&id);
                        if ctx.bound_vao == Some(id) {
                            ctx.bound_vao = None;
                        }
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    for (name, enabled) in [
        ("enableVertexAttribArray", true),
        ("disableVertexAttribArray", false),
    ] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let idx = vm.to_u32(arg(args, 0))?;
            if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
                ctx.active_vao_mut().attribs.entry(idx).or_default().enabled = enabled;
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let s = Rc::clone(sys);
    define_host_method(vm, p, "vertexAttribPointer", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let idx = vm.to_u32(arg(args, 0))?;
        let size = vm.to_u32(arg(args, 1))?.clamp(1, 4);
        let gl_type = vm.to_u32(arg(args, 2))?;
        let normalized = vm.to_boolean(arg(args, 3));
        let stride = vm.to_number(arg(args, 4))?.max(0.0) as u64;
        let offset = vm.to_number(arg(args, 5))?.max(0.0) as u64;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            let buf_id = ctx.bound_array_buffer;
            let entry = ctx.active_vao_mut().attribs.entry(idx).or_default();
            entry.buffer_id = buf_id;
            entry.size = size;
            entry.gl_type = gl_type;
            entry.normalized = normalized;
            entry.stride = stride;
            entry.offset = offset;
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "vertexAttribDivisor", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let idx = vm.to_u32(arg(args, 0))?;
        let divisor = vm.to_u32(arg(args, 1))?;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.active_vao_mut().attribs.entry(idx).or_default().divisor = divisor;
        }
        Ok(JsValue::UNDEFINED)
    });

    // Uniform setters (scalar/vector float & int + matrices)
    for (name, count) in [
        ("uniform1f", 1usize),
        ("uniform2f", 2),
        ("uniform3f", 3),
        ("uniform4f", 4),
    ] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let Some(uh) = arg(args, 0).as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let mut vals = Vec::with_capacity(count);
            for i in 0..count {
                vals.push(vm.to_number(arg(args, 1 + i))? as f32);
            }
            let mut st = s.borrow_mut();
            if let Some((
                unid,
                GlUniformTarget::Ubo {
                    program_id, offset, ..
                },
            )) = st.uniform_handles.get(&uh).cloned()
            {
                if unid == nid {
                    if let Some(prog) = st
                        .contexts
                        .get_mut(&nid)
                        .and_then(|c| c.programs.get_mut(&program_id))
                    {
                        write_ubo_f32s(prog, offset, &vals);
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    for (name, count) in [
        ("uniform1fv", 1usize),
        ("uniform2fv", 2),
        ("uniform3fv", 3),
        ("uniform4fv", 4),
    ] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let Some(uh) = arg(args, 0).as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let vals = extract_f32_seq(vm, arg(args, 1));
            let slice = &vals[..vals.len().min(count)];
            let mut st = s.borrow_mut();
            if let Some((
                unid,
                GlUniformTarget::Ubo {
                    program_id, offset, ..
                },
            )) = st.uniform_handles.get(&uh).cloned()
            {
                if unid == nid {
                    if let Some(prog) = st
                        .contexts
                        .get_mut(&nid)
                        .and_then(|c| c.programs.get_mut(&program_id))
                    {
                        write_ubo_f32s(prog, offset, slice);
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    for (name, count) in [
        ("uniform1i", 1usize),
        ("uniform2i", 2),
        ("uniform3i", 3),
        ("uniform4i", 4),
    ] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let Some(uh) = arg(args, 0).as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let mut vals = Vec::with_capacity(count);
            for i in 0..count {
                vals.push(vm.to_number(arg(args, 1 + i))?.trunc() as i32);
            }
            let mut st = s.borrow_mut();
            if let Some((unid, target)) = st.uniform_handles.get(&uh).cloned() {
                if unid == nid {
                    match target {
                        GlUniformTarget::Sampler {
                            program_id,
                            sampler_idx,
                        } => {
                            if let Some(prog) = st
                                .contexts
                                .get_mut(&nid)
                                .and_then(|c| c.programs.get_mut(&program_id))
                            {
                                if let Some(slot) = prog.sampler_units.get_mut(sampler_idx) {
                                    *slot = vals.first().copied().unwrap_or(0).max(0) as u32;
                                }
                            }
                        }
                        GlUniformTarget::Ubo {
                            program_id, offset, ..
                        } => {
                            if let Some(prog) = st
                                .contexts
                                .get_mut(&nid)
                                .and_then(|c| c.programs.get_mut(&program_id))
                            {
                                write_ubo_i32s(prog, offset, &vals);
                            }
                        }
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let s = Rc::clone(sys);
    define_host_method(vm, p, "uniform1iv", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let Some(uh) = arg(args, 0).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let vals = extract_i32_seq(vm, arg(args, 1));
        let mut st = s.borrow_mut();
        if let Some((unid, target)) = st.uniform_handles.get(&uh).cloned() {
            if unid == nid {
                match target {
                    GlUniformTarget::Sampler {
                        program_id,
                        sampler_idx,
                    } => {
                        if let Some(prog) = st
                            .contexts
                            .get_mut(&nid)
                            .and_then(|c| c.programs.get_mut(&program_id))
                        {
                            if let Some(slot) = prog.sampler_units.get_mut(sampler_idx) {
                                *slot = vals.first().copied().unwrap_or(0).max(0) as u32;
                            }
                        }
                    }
                    GlUniformTarget::Ubo {
                        program_id, offset, ..
                    } => {
                        if let Some(prog) = st
                            .contexts
                            .get_mut(&nid)
                            .and_then(|c| c.programs.get_mut(&program_id))
                        {
                            write_ubo_i32s(prog, offset, &vals[..vals.len().min(1)]);
                        }
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    for (name, dim) in [
        ("uniformMatrix2fv", 2usize),
        ("uniformMatrix3fv", 3),
        ("uniformMatrix4fv", 4),
    ] {
        let s = Rc::clone(sys);
        define_host_method(vm, p, name, move |vm, this_val, args| {
            let nid = require_ctx_node(vm, &s, this_val)?;
            let Some(uh) = arg(args, 0).as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let transpose = vm.to_boolean(arg(args, 1));
            let vals = extract_f32_seq(vm, arg(args, 2));
            let mut st = s.borrow_mut();
            if let Some((
                unid,
                GlUniformTarget::Ubo {
                    program_id, offset, ..
                },
            )) = st.uniform_handles.get(&uh).cloned()
            {
                if unid == nid {
                    if let Some(prog) = st
                        .contexts
                        .get_mut(&nid)
                        .and_then(|c| c.programs.get_mut(&program_id))
                    {
                        write_ubo_matrix(prog, offset, dim, transpose, &vals);
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
}

fn install_texture_and_draw_methods(vm: &mut JsVm, sys: &SharedWebGl2, p: HeapHandle) {
    let s = Rc::clone(sys);
    define_host_method(vm, p, "createTexture", move |vm, this_val, _| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let proto = s.borrow().protos.texture_proto;
        let obj = alloc_with_proto(vm, proto);
        let mut st = s.borrow_mut();
        if let Some(ctx) = st.contexts.get_mut(&nid) {
            let id = ctx.alloc_id();
            ctx.textures.insert(id, GlTexture::new());
            st.texture_handles.insert(obj, (nid, id));
        }
        Ok(JsValue::from_object(obj))
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "activeTexture", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let unit = vm.to_u32(arg(args, 0))?.saturating_sub(GL_TEXTURE0) as usize;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            if unit < MAX_TEXTURE_UNITS {
                ctx.active_tex_unit = unit;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "bindTexture", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        let tid = arg(args, 1).as_object().and_then(|th| {
            s.borrow()
                .texture_handles
                .get(&th)
                .filter(|(tnid, _)| *tnid == nid)
                .map(|(_, id)| *id)
        });
        if target == GL_TEXTURE_2D {
            if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
                let unit = ctx.active_tex_unit;
                ctx.bound_tex_2d[unit] = tid;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "texParameteri", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        let pname = vm.to_u32(arg(args, 1))?;
        let param = vm.to_u32(arg(args, 2))?;
        if target == GL_TEXTURE_2D {
            if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
                if let Some(tid) = ctx.bound_tex_2d[ctx.active_tex_unit] {
                    if let Some(tex) = ctx.textures.get_mut(&tid) {
                        match pname {
                            GL_TEXTURE_MIN_FILTER => tex.min_filter = gl_to_filter(param),
                            GL_TEXTURE_MAG_FILTER => tex.mag_filter = gl_to_filter(param),
                            GL_TEXTURE_WRAP_S => tex.wrap_s = gl_to_wrap(param),
                            GL_TEXTURE_WRAP_T => tex.wrap_t = gl_to_wrap(param),
                            _ => {}
                        }
                        tex.sampler = None;
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // texImage2D(target, level, internalformat, width, height, border, format, type, pixels)
    let s = Rc::clone(sys);
    define_host_method(vm, p, "texImage2D", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        if target != GL_TEXTURE_2D || args.len() < 9 {
            return Ok(JsValue::UNDEFINED);
        }
        let width = vm.to_u32(arg(args, 3))?.max(1);
        let height = vm.to_u32(arg(args, 4))?.max(1);
        let format = vm.to_u32(arg(args, 6))?;
        let raw_opt = extract_gpu_buffer_source(vm, arg(args, 8), None, None);

        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            let Some(tid) = ctx.bound_tex_2d[ctx.active_tex_unit] else {
                return Ok(JsValue::UNDEFINED);
            };
            let rgba = match raw_opt {
                Some(raw) => expand_webgl_pixels_to_rgba8(
                    &raw,
                    width,
                    height,
                    format,
                    ctx.unpack_flip_y,
                    ctx.unpack_premultiply_alpha,
                ),
                None => vec![0u8; (width as usize) * (height as usize) * 4],
            };
            let gpu_tex = ctx.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rix-webgl2-tex2d"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            ctx.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &gpu_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            let view = gpu_tex.create_view(&wgpu::TextureViewDescriptor::default());
            if let Some(tex) = ctx.textures.get_mut(&tid) {
                tex.texture = Some(gpu_tex);
                tex.view = Some(view);
                tex.width = width;
                tex.height = height;
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "texSubImage2D", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let target = vm.to_u32(arg(args, 0))?;
        if target != GL_TEXTURE_2D || args.len() < 9 {
            return Ok(JsValue::UNDEFINED);
        }
        let xoff = vm.to_u32(arg(args, 2))?;
        let yoff = vm.to_u32(arg(args, 3))?;
        let width = vm.to_u32(arg(args, 4))?.max(1);
        let height = vm.to_u32(arg(args, 5))?.max(1);
        let format = vm.to_u32(arg(args, 6))?;
        let Some(raw) = extract_gpu_buffer_source(vm, arg(args, 8), None, None) else {
            return Ok(JsValue::UNDEFINED);
        };
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            if let Some(tid) = ctx.bound_tex_2d[ctx.active_tex_unit] {
                if let Some(tex) = ctx.textures.get(&tid).and_then(|t| t.texture.as_ref()) {
                    let rgba = expand_webgl_pixels_to_rgba8(
                        &raw,
                        width,
                        height,
                        format,
                        ctx.unpack_flip_y,
                        ctx.unpack_premultiply_alpha,
                    );
                    ctx.gpu.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d {
                                x: xoff,
                                y: yoff,
                                z: 0,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &rgba,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(width * 4),
                            rows_per_image: Some(height),
                        },
                        wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    define_host_method(vm, p, "generateMipmap", |_, _, _| Ok(JsValue::UNDEFINED));

    let s = Rc::clone(sys);
    define_host_method(vm, p, "deleteTexture", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        if let Some(th) = arg(args, 0).as_object() {
            let mut st = s.borrow_mut();
            if let Some((tnid, id)) = st.texture_handles.remove(&th) {
                if tnid == nid {
                    if let Some(ctx) = st.contexts.get_mut(&nid) {
                        ctx.textures.remove(&id);
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    // Draw calls
    let s = Rc::clone(sys);
    define_host_method(vm, p, "drawArrays", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        let first = vm.to_number(arg(args, 1))?.max(0.0) as u32;
        let count = vm.to_number(arg(args, 2))?.max(0.0) as u32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.draw_common(mode, None, first, count, 1);
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "drawArraysInstanced", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        let first = vm.to_number(arg(args, 1))?.max(0.0) as u32;
        let count = vm.to_number(arg(args, 2))?.max(0.0) as u32;
        let instances = vm.to_number(arg(args, 3))?.max(0.0) as u32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.draw_common(mode, None, first, count, instances);
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "drawElements", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        let count = vm.to_number(arg(args, 1))?.max(0.0) as u32;
        let idx_type = vm.to_u32(arg(args, 2))?;
        let offset = vm.to_number(arg(args, 3))?.max(0.0) as u64;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.draw_common(mode, Some((count, idx_type, offset)), 0, count, 1);
        }
        Ok(JsValue::UNDEFINED)
    });

    let s = Rc::clone(sys);
    define_host_method(vm, p, "drawElementsInstanced", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let mode = vm.to_u32(arg(args, 0))?;
        let count = vm.to_number(arg(args, 1))?.max(0.0) as u32;
        let idx_type = vm.to_u32(arg(args, 2))?;
        let offset = vm.to_number(arg(args, 3))?.max(0.0) as u64;
        let instances = vm.to_number(arg(args, 4))?.max(0.0) as u32;
        if let Some(ctx) = s.borrow_mut().contexts.get_mut(&nid) {
            ctx.draw_common(mode, Some((count, idx_type, offset)), 0, count, instances);
        }
        Ok(JsValue::UNDEFINED)
    });

    // readPixels(x, y, width, height, format, type, pixels) — OpenGL bottom-up row order!
    let s = Rc::clone(sys);
    define_host_method(vm, p, "readPixels", move |vm, this_val, args| {
        let nid = require_ctx_node(vm, &s, this_val)?;
        let x = vm.to_number(arg(args, 0))?.trunc() as i32;
        let y = vm.to_number(arg(args, 1))?.trunc() as i32;
        let w = vm.to_number(arg(args, 2))?.max(0.0) as u32;
        let h_px = vm.to_number(arg(args, 3))?.max(0.0) as u32;
        let Some(dst_h) = arg(args, 6).as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some((cw, ch, full)) = s.borrow().readback_canvas_rgba(nid) else {
            return Ok(JsValue::UNDEFINED);
        };
        let Some(ObjectKind::TypedArray(ta)) = vm.heap.get_object(dst_h).map(|o| &o.kind).cloned()
        else {
            return Ok(JsValue::UNDEFINED);
        };
        let dst_off = ta.byte_offset as usize;
        let dst_len = (ta.length as usize) * ta.kind.bytes_per_element();
        if let Some(ObjectKind::ArrayBuffer(buf)) =
            vm.heap.get_object_mut(ta.buffer).map(|o| &mut o.kind)
        {
            for row in 0..h_px as i32 {
                let gl_y = y + row;
                if gl_y < 0 || gl_y >= ch as i32 {
                    continue;
                }
                let src_y = (ch as i32 - 1 - gl_y) as usize;
                for col in 0..w as i32 {
                    let src_x = x + col;
                    if src_x < 0 || src_x >= cw as i32 {
                        continue;
                    }
                    let si = (src_y * (cw as usize) + (src_x as usize)) * 4;
                    let rel_di = ((row as usize) * (w as usize) + (col as usize)) * 4;
                    let di = dst_off + rel_di;
                    if rel_di + 4 <= dst_len && di + 4 <= buf.len() && si + 4 <= full.len() {
                        buf[di..di + 4].copy_from_slice(&full[si..si + 4]);
                    }
                }
            }
        }
        Ok(JsValue::UNDEFINED)
    });
}

fn register_canvas_webgl2_hooks(host: &SharedHost, canvas2d: &SharedCanvas2D, sys: &SharedWebGl2) {
    let h = Rc::clone(host);
    let s = Rc::clone(sys);
    canvas2d
        .borrow_mut()
        .register_context_hook(Rc::new(move |vm, nid, kind, _opts| {
            let is_webgl2 = kind.eq_ignore_ascii_case("webgl2");
            let is_webgl1 = kind.eq_ignore_ascii_case("webgl")
                || kind.eq_ignore_ascii_case("experimental-webgl");
            if !is_webgl2 && !is_webgl1 {
                return Ok(None);
            }
            if let Some(&existing) = s.borrow().node_to_ctx_handle.get(&nid) {
                if vm.heap.is_alive(existing) {
                    return Ok(Some(JsValue::from_object(existing)));
                }
            }
            let w = canvas_attr_dim(&h, nid, "width", 300).max(1);
            let h_px = canvas_attr_dim(&h, nid, "height", 150).max(1);
            let gpu = match s.borrow().shared_gpu.clone() {
                Some(g) => g,
                None => match create_headless_gpu_device(wgpu::PowerPreference::None, false) {
                    Ok(g) => g,
                    Err(_) => return Ok(Some(JsValue::NULL)),
                },
            };
            let proto = s.borrow().protos.ctx_proto;
            let obj = alloc_with_proto(vm, proto);
            let mut st = s.borrow_mut();
            st.contexts.entry(nid).or_insert_with(|| {
                WebGl2Context::new(surface_id_for_node(nid), w, h_px, gpu, is_webgl2)
            });
            st.ctx_handle_to_node.insert(obj, nid);
            st.node_to_ctx_handle.insert(nid, obj);
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
