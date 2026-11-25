//! Real `wgpu` WGSL GPU filter & `backdrop-filter` compositing pipeline.
//!
//! Executes W3C Filter Effects Level 1 & Level 2 entirely GPU-to-GPU on `Rgba8Unorm`
//! textures with **zero CPU pixel readbacks** and **zero per-frame texture allocations**:
//! - Exact W3C 4×5 RGBA [`ColorFilterMatrix`] covering `brightness`, `contrast`,
//!   `grayscale`, `saturate`, `sepia`, `invert`, `hue-rotate`, and `opacity` composed
//!   in exact CSS declaration order.
//! - Two-pass separable WGSL Gaussian blur (`GAUSSIAN_BLUR_WGSL`) with adaptive stride
//!   for large radii so 60–120 FPS animations stay smooth.
//! - Rounded-rectangle SDF-masked color-matrix backdrop composite (`BACKDROP_COMPOSITE_WGSL`)
//!   plus GPU SrcOver overlay blending (`OVERLAY_COMPOSITE_WGSL`) for true painter's order.

use rix_core::Rect;
use smallvec::SmallVec;
use vello::wgpu;

use crate::display_list::{FilterOp, Rgba8};

/// Exact W3C Filter Effects Level 1 4×5 RGBA color transformation matrix:
/// `out_rgba = M[0..4] * in_rgba + M[4]` (column 4 is constant offset per channel).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorFilterMatrix {
    /// Row-major 4 rows × 5 columns (`[r_row(5), g_row(5), b_row(5), a_row(5)]`).
    pub m: [f32; 20],
}

impl Default for ColorFilterMatrix {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl ColorFilterMatrix {
    pub const IDENTITY: Self = Self {
        m: [
            1.0, 0.0, 0.0, 0.0, 0.0, // R
            0.0, 1.0, 0.0, 0.0, 0.0, // G
            0.0, 0.0, 1.0, 0.0, 0.0, // B
            0.0, 0.0, 0.0, 1.0, 0.0, // A
        ],
    };

    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.m
            .iter()
            .zip(Self::IDENTITY.m.iter())
            .all(|(a, b)| (a - b).abs() < 1e-4)
    }

    /// Composes `next` AFTER `self` (`out = next * self`).
    #[must_use]
    pub fn then(self, next: Self) -> Self {
        let a = &next.m;
        let b = &self.m;
        let mut out = [0.0f32; 20];
        for row in 0..4 {
            let r = row * 5;
            for col in 0..4 {
                out[r + col] = a[r] * b[col]
                    + a[r + 1] * b[5 + col]
                    + a[r + 2] * b[10 + col]
                    + a[r + 3] * b[15 + col];
            }
            out[r + 4] =
                a[r] * b[4] + a[r + 1] * b[9] + a[r + 2] * b[14] + a[r + 3] * b[19] + a[r + 4];
        }
        Self { m: out }
    }

    #[must_use]
    pub fn brightness(amount: f32) -> Self {
        let k = amount.max(0.0);
        Self {
            m: [
                k, 0.0, 0.0, 0.0, 0.0, //
                0.0, k, 0.0, 0.0, 0.0, //
                0.0, 0.0, k, 0.0, 0.0, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ],
        }
    }

    #[must_use]
    pub fn contrast(amount: f32) -> Self {
        let k = amount.max(0.0);
        let t = 0.5 * (1.0 - k);
        Self {
            m: [
                k, 0.0, 0.0, 0.0, t, //
                0.0, k, 0.0, 0.0, t, //
                0.0, 0.0, k, 0.0, t, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ],
        }
    }

    /// W3C Filter Effects Level 1 §11 `grayscale(amount)` sRGB matrix.
    #[must_use]
    pub fn grayscale(amount: f32) -> Self {
        let inv = 1.0 - amount.clamp(0.0, 1.0);
        Self {
            m: [
                0.2126 + 0.7874 * inv,
                0.7152 - 0.7152 * inv,
                0.0722 - 0.0722 * inv,
                0.0,
                0.0,
                0.2126 - 0.2126 * inv,
                0.7152 + 0.2848 * inv,
                0.0722 - 0.0722 * inv,
                0.0,
                0.0,
                0.2126 - 0.2126 * inv,
                0.7152 - 0.7152 * inv,
                0.0722 + 0.9278 * inv,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
        }
    }

    /// W3C Filter Effects Level 1 §11 `saturate(amount)` sRGB matrix.
    #[must_use]
    pub fn saturate(amount: f32) -> Self {
        let s = amount.max(0.0);
        Self {
            m: [
                0.213 + 0.787 * s,
                0.715 - 0.715 * s,
                0.072 - 0.072 * s,
                0.0,
                0.0,
                0.213 - 0.213 * s,
                0.715 + 0.285 * s,
                0.072 - 0.072 * s,
                0.0,
                0.0,
                0.213 - 0.213 * s,
                0.715 - 0.715 * s,
                0.072 + 0.928 * s,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
        }
    }

    /// W3C Filter Effects Level 1 §11 `sepia(amount)` sRGB matrix.
    #[must_use]
    pub fn sepia(amount: f32) -> Self {
        let inv = 1.0 - amount.clamp(0.0, 1.0);
        Self {
            m: [
                0.393 + 0.607 * inv,
                0.769 - 0.769 * inv,
                0.189 - 0.189 * inv,
                0.0,
                0.0,
                0.349 - 0.349 * inv,
                0.686 + 0.314 * inv,
                0.168 - 0.168 * inv,
                0.0,
                0.0,
                0.272 - 0.272 * inv,
                0.534 - 0.534 * inv,
                0.131 + 0.869 * inv,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
        }
    }

    /// W3C Filter Effects Level 1 §11 `invert(amount)` linear transfer matrix.
    #[must_use]
    pub fn invert(amount: f32) -> Self {
        let a = amount.clamp(0.0, 1.0);
        let s = 1.0 - 2.0 * a;
        Self {
            m: [
                s, 0.0, 0.0, 0.0, a, //
                0.0, s, 0.0, 0.0, a, //
                0.0, 0.0, s, 0.0, a, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ],
        }
    }

    /// W3C Filter Effects Level 1 §11 `hue-rotate(deg)` exact sRGB matrix.
    #[must_use]
    pub fn hue_rotate_deg(deg: f32) -> Self {
        let rad = deg.to_radians();
        let c = rad.cos();
        let s = rad.sin();
        Self {
            m: [
                0.213 + 0.787 * c - 0.213 * s,
                0.715 - 0.715 * c - 0.715 * s,
                0.072 - 0.072 * c + 0.928 * s,
                0.0,
                0.0,
                0.213 - 0.213 * c + 0.143 * s,
                0.715 + 0.285 * c + 0.140 * s,
                0.072 - 0.072 * c - 0.283 * s,
                0.0,
                0.0,
                0.213 - 0.213 * c - 0.787 * s,
                0.715 - 0.715 * c + 0.715 * s,
                0.072 + 0.928 * c + 0.072 * s,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
        }
    }

    #[must_use]
    pub fn opacity(amount: f32) -> Self {
        let a = amount.clamp(0.0, 1.0);
        Self {
            m: [
                1.0, 0.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 0.0, a, 0.0,
            ],
        }
    }

    /// Builds a composite matrix from `ops` in exact CSS declaration order.
    /// When `include_opacity` is `false`, `FilterOp::Opacity` is skipped (used when
    /// opacity is already handled by the stacking-context layer alpha).
    #[must_use]
    pub fn from_filters(ops: &[FilterOp], include_opacity: bool) -> Self {
        let mut mat = Self::IDENTITY;
        for op in ops {
            let next = match *op {
                FilterOp::Brightness(v) => Self::brightness(v),
                FilterOp::Contrast(v) => Self::contrast(v),
                FilterOp::Grayscale(v) => Self::grayscale(v),
                FilterOp::Saturate(v) => Self::saturate(v),
                FilterOp::Sepia(v) => Self::sepia(v),
                FilterOp::Invert(v) => Self::invert(v),
                FilterOp::HueRotateDeg(d) => Self::hue_rotate_deg(d),
                FilterOp::Opacity(a) if include_opacity => Self::opacity(a),
                FilterOp::Blur(_)
                | FilterOp::Opacity(_)
                | FilterOp::DropShadow { .. }
                | FilterOp::AnimatedOpacity(_) => continue,
            };
            mat = mat.then(next);
        }
        mat
    }

    /// Evaluates this 4×5 matrix on an sRGB [`Rgba8`] color.
    #[must_use]
    pub fn apply_rgba(&self, c: Rgba8) -> Rgba8 {
        if self.is_identity() {
            return c;
        }
        let r = f32::from(c.r) / 255.0;
        let g = f32::from(c.g) / 255.0;
        let b = f32::from(c.b) / 255.0;
        let a = f32::from(c.a) / 255.0;
        let m = &self.m;
        let out_r = m[0] * r + m[1] * g + m[2] * b + m[3] * a + m[4];
        let out_g = m[5] * r + m[6] * g + m[7] * b + m[8] * a + m[9];
        let out_b = m[10] * r + m[11] * g + m[12] * b + m[13] * a + m[14];
        let out_a = m[15] * r + m[16] * g + m[17] * b + m[18] * a + m[19];
        Rgba8::from_f32(out_r, out_g, out_b, out_a)
    }

    /// Packs the 4×5 matrix into 5 `vec4<f32>` uniform rows (`r`, `g`, `b`, `a`, `offset`).
    #[must_use]
    pub fn to_wgsl_uniform_rows(&self) -> [[f32; 4]; 5] {
        let m = &self.m;
        [
            [m[0], m[1], m[2], m[3]],
            [m[5], m[6], m[7], m[8]],
            [m[10], m[11], m[12], m[13]],
            [m[15], m[16], m[17], m[18]],
            [m[4], m[9], m[14], m[19]],
        ]
    }
}

/// Physical-pixel descriptor of a GPU `backdrop-filter` pass resolved through `SpatialTree`.
#[derive(Debug, Clone, PartialEq)]
pub struct BackdropFilterPass {
    /// Physical-pixel bounding rectangle of the element's border box.
    pub bounds_px: Rect,
    /// Physical-pixel corner radii `[tl, tr, br, bl]`.
    pub radii_px: [f32; 4],
    /// Filter operations scaled to physical pixels.
    pub filters: SmallVec<[FilterOp; 2]>,
}

impl BackdropFilterPass {
    #[must_use]
    pub fn total_blur_sigma(&self) -> f32 {
        let mut sum = 0.0f32;
        for op in &self.filters {
            if let FilterOp::Blur(r) = *op {
                sum += r.max(0.0);
            }
        }
        sum
    }

    #[must_use]
    pub fn color_matrix(&self) -> ColorFilterMatrix {
        ColorFilterMatrix::from_filters(&self.filters, true)
    }
}

const GAUSSIAN_BLUR_WGSL: &str = r#"
struct BlurParams {
    texel_dir: vec2<f32>,
    sigma: f32,
    radius_taps: u32,
    region_uv: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid & 1u) << 2u) - 1.0;
    let y = 1.0 - f32((vid & 2u) << 1u);
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> params: BlurParams;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (params.sigma <= 0.05 || params.radius_taps == 0u) {
        return textureSample(src_tex, src_sampler, in.uv);
    }
    let inv_two_sigma2 = 0.5 / (params.sigma * params.sigma);
    var accum = textureSample(src_tex, src_sampler, in.uv);
    var weight_sum = 1.0;
    let min_uv = params.region_uv.xy;
    let max_uv = params.region_uv.zw;
    let taps = min(params.radius_taps, 24u);
    let stride = max(1.0, (params.sigma * 2.5) / f32(taps));
    for (var i: u32 = 1u; i <= taps; i = i + 1u) {
        let dist = f32(i) * stride;
        let w = exp(-(dist * dist) * inv_two_sigma2);
        let delta = params.texel_dir * dist;
        let uv_pos = clamp(in.uv + delta, min_uv, max_uv);
        let uv_neg = clamp(in.uv - delta, min_uv, max_uv);
        accum = accum + (textureSample(src_tex, src_sampler, uv_pos) + textureSample(src_tex, src_sampler, uv_neg)) * w;
        weight_sum = weight_sum + 2.0 * w;
    }
    return accum / weight_sum;
}
"#;

const BACKDROP_COMPOSITE_WGSL: &str = r#"
struct BackdropParams {
    row_r: vec4<f32>,
    row_g: vec4<f32>,
    row_b: vec4<f32>,
    row_a: vec4<f32>,
    offset: vec4<f32>,
    bounds_px: vec4<f32>,
    radii_px: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid & 1u) << 2u) - 1.0;
    let y = 1.0 - f32((vid & 2u) << 1u);
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> params: BackdropParams;

fn rounded_box_sdf(p: vec2<f32>, half_size: vec2<f32>, radii: vec4<f32>) -> f32 {
    // radii = (tl, tr, br, bl)
    let r_right = select(radii.y, radii.z, p.y > 0.0);
    let r_left = select(radii.x, radii.w, p.y > 0.0);
    let r = select(r_left, r_right, p.x > 0.0);
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let px = in.pos.xy;
    let center = params.bounds_px.xy + params.bounds_px.zw * 0.5;
    let half_size = max(params.bounds_px.zw * 0.5, vec2<f32>(0.5, 0.5));
    let sdf = rounded_box_sdf(px - center, half_size, params.radii_px);
    let cov = clamp(0.5 - sdf, 0.0, 1.0);
    if (cov <= 0.001) {
        discard;
    }
    let c = textureSample(src_tex, src_sampler, in.uv);
    let r = dot(params.row_r, c) + params.offset.x;
    let g = dot(params.row_g, c) + params.offset.y;
    let b = dot(params.row_b, c) + params.offset.z;
    let a = dot(params.row_a, c) + params.offset.w;
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), clamp(a, 0.0, 1.0) * cov);
}
"#;

const OVERLAY_COMPOSITE_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid & 1u) << 2u) - 1.0;
    let y = 1.0 - f32((vid & 2u) << 1u);
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@group(0) @binding(0) var overlay_tex: texture_2d<f32>;
@group(0) @binding(1) var overlay_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(overlay_tex, overlay_sampler, in.uv);
}
"#;

struct FilterScratchTextures {
    width: u32,
    height: u32,
    scratch_a: wgpu::Texture,
    scratch_a_view: wgpu::TextureView,
    _scratch_b: wgpu::Texture,
    scratch_b_view: wgpu::TextureView,
    _overlay: wgpu::Texture,
    overlay_view: wgpu::TextureView,
}

/// GPU-resident WGSL filter and `backdrop-filter` pipeline cached on [`crate::VelloRenderer`].
pub struct GpuFilterPipeline {
    blur_bgl: wgpu::BindGroupLayout,
    blur_pipeline: wgpu::RenderPipeline,
    backdrop_bgl: wgpu::BindGroupLayout,
    backdrop_pipeline: wgpu::RenderPipeline,
    overlay_bgl: wgpu::BindGroupLayout,
    overlay_pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    blur_uniform: wgpu::Buffer,
    backdrop_uniform: wgpu::Buffer,
    scratch: Option<FilterScratchTextures>,
}

impl GpuFilterPipeline {
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let tex_entry = wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let uniform_entry = wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };

        let blur_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rix-gpu-blur-bgl"),
            entries: &[tex_entry, sampler_entry, uniform_entry],
        });
        let backdrop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rix-gpu-backdrop-bgl"),
            entries: &[tex_entry, sampler_entry, uniform_entry],
        });
        let overlay_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rix-gpu-overlay-bgl"),
            entries: &[tex_entry, sampler_entry],
        });

        let blur_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rix-gpu-gaussian-blur-shader"),
            source: wgpu::ShaderSource::Wgsl(GAUSSIAN_BLUR_WGSL.into()),
        });
        let backdrop_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rix-gpu-backdrop-composite-shader"),
            source: wgpu::ShaderSource::Wgsl(BACKDROP_COMPOSITE_WGSL.into()),
        });
        let overlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rix-gpu-overlay-composite-shader"),
            source: wgpu::ShaderSource::Wgsl(OVERLAY_COMPOSITE_WGSL.into()),
        });

        let mk_pipeline = |label: &str,
                           bgl: &wgpu::BindGroupLayout,
                           shader: &wgpu::ShaderModule,
                           blend: Option<wgpu::BlendState>| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let blur_pipeline = mk_pipeline("rix-gpu-blur-pipeline", &blur_bgl, &blur_shader, None);
        let backdrop_pipeline = mk_pipeline(
            "rix-gpu-backdrop-pipeline",
            &backdrop_bgl,
            &backdrop_shader,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        // Vello outputs premultiplied alpha into `Rgba8Unorm` textures.
        let overlay_pipeline = mk_pipeline(
            "rix-gpu-overlay-pipeline",
            &overlay_bgl,
            &overlay_shader,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rix-gpu-filter-linear-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let blur_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rix-gpu-blur-uniform"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let backdrop_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rix-gpu-backdrop-uniform"),
            size: 112, // 7 * vec4<f32>
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            blur_bgl,
            blur_pipeline,
            backdrop_bgl,
            backdrop_pipeline,
            overlay_bgl,
            overlay_pipeline,
            sampler,
            blur_uniform,
            backdrop_uniform,
            scratch: None,
        }
    }

    fn ensure_scratch(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let needs_alloc = self
            .scratch
            .as_ref()
            .is_none_or(|s| s.width != width || s.height != height);
        if !needs_alloc {
            return;
        }
        let mk_tex = |label: &str, storage: bool| {
            let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST;
            if storage {
                usage |= wgpu::TextureUsages::STORAGE_BINDING;
            }
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            (tex, view)
        };
        let (scratch_a, scratch_a_view) = mk_tex("rix-gpu-filter-scratch-a", false);
        let (scratch_b, scratch_b_view) = mk_tex("rix-gpu-filter-scratch-b", false);
        let (overlay, overlay_view) = mk_tex("rix-gpu-filter-overlay", true);
        self.scratch = Some(FilterScratchTextures {
            width,
            height,
            scratch_a,
            scratch_a_view,
            _scratch_b: scratch_b,
            scratch_b_view,
            _overlay: overlay,
            overlay_view,
        });
    }

    /// Returns the reusable off-screen Vello overlay texture view (`STORAGE_BINDING`).
    pub fn overlay_view(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> &wgpu::TextureView {
        self.ensure_scratch(device, width, height);
        &self.scratch.as_ref().unwrap().overlay_view
    }

    /// Applies `pass` (`blur` + 10-function `ColorFilterMatrix` + rounded-rect SDF mask)
    /// directly onto `target_tex` in-place via GPU-to-GPU WGSL passes (zero CPU readback).
    pub fn apply_backdrop_pass(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_tex: &wgpu::Texture,
        target_view: &wgpu::TextureView,
        pass: &BackdropFilterPass,
    ) {
        let width = target_tex.width().max(1);
        let height = target_tex.height().max(1);
        let vp = Rect::new(0.0, 0.0, width as f32, height as f32);
        let Some(clipped) = pass.bounds_px.intersect(&vp) else {
            return;
        };
        if clipped.is_empty() {
            return;
        }

        self.ensure_scratch(device, width, height);
        let scratch = self.scratch.as_ref().unwrap();
        let sigma = pass.total_blur_sigma();
        let mat = pass.color_matrix();

        // Step 1: Copy current rendered backdrop from `target_tex` -> `scratch_a`.
        let mut copy_enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rix-backdrop-copy-enc"),
        });
        copy_enc.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &scratch.scratch_a,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([copy_enc.finish()]);

        // Step 2: Optional 2-pass separable Gaussian blur (`scratch_a` -> `scratch_b` -> `scratch_a`).
        if sigma > 0.05 {
            let taps = (sigma * 2.5).ceil().clamp(1.0, 24.0) as u32;
            let region_uv = [
                0.5 / width as f32,
                0.5 / height as f32,
                1.0 - 0.5 / width as f32,
                1.0 - 0.5 / height as f32,
            ];

            // Horizontal pass: scratch_a -> scratch_b
            let h_bytes = encode_blur_uniform([1.0 / width as f32, 0.0], sigma, taps, region_uv);
            queue.write_buffer(&self.blur_uniform, 0, &h_bytes);
            let bg_h = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rix-blur-h-bg"),
                layout: &self.blur_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&scratch.scratch_a_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.blur_uniform.as_entire_binding(),
                    },
                ],
            });
            let mut enc_h = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rix-blur-h-enc"),
            });
            {
                let mut rp = enc_h.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("rix-blur-h-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &scratch.scratch_b_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(&self.blur_pipeline);
                rp.set_bind_group(0, &bg_h, &[]);
                rp.draw(0..3, 0..1);
            }
            queue.submit([enc_h.finish()]);

            // Vertical pass: scratch_b -> scratch_a
            let v_bytes = encode_blur_uniform([0.0, 1.0 / height as f32], sigma, taps, region_uv);
            queue.write_buffer(&self.blur_uniform, 0, &v_bytes);
            let bg_v = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rix-blur-v-bg"),
                layout: &self.blur_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&scratch.scratch_b_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.blur_uniform.as_entire_binding(),
                    },
                ],
            });
            let mut enc_v = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rix-blur-v-enc"),
            });
            {
                let mut rp = enc_v.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("rix-blur-v-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &scratch.scratch_a_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(&self.blur_pipeline);
                rp.set_bind_group(0, &bg_v, &[]);
                rp.draw(0..3, 0..1);
            }
            queue.submit([enc_v.finish()]);
        }

        // Step 3: Composite filtered `scratch_a` back onto `target_view` clipped by rounded-rect SDF.
        let bd_bytes = encode_backdrop_uniform(&mat, pass.bounds_px, pass.radii_px);
        queue.write_buffer(&self.backdrop_uniform, 0, &bd_bytes);
        let bg_comp = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rix-backdrop-comp-bg"),
            layout: &self.backdrop_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&scratch.scratch_a_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.backdrop_uniform.as_entire_binding(),
                },
            ],
        });

        let sx = (clipped.min_x().floor().max(0.0) as u32).min(width.saturating_sub(1));
        let sy = (clipped.min_y().floor().max(0.0) as u32).min(height.saturating_sub(1));
        let ex = (clipped.max_x().ceil().max(0.0) as u32).min(width);
        let ey = (clipped.max_y().ceil().max(0.0) as u32).min(height);
        let sw = ex.saturating_sub(sx);
        let sh = ey.saturating_sub(sy);
        if sw == 0 || sh == 0 {
            return;
        }

        let mut comp_enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rix-backdrop-comp-enc"),
        });
        {
            let mut rp = comp_enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rix-backdrop-comp-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.backdrop_pipeline);
            rp.set_bind_group(0, &bg_comp, &[]);
            rp.set_scissor_rect(sx, sy, sw, sh);
            rp.draw(0..3, 0..1);
        }
        queue.submit([comp_enc.finish()]);
    }

    /// Alpha-composites the internal `overlay` texture on top of `target_view` (premultiplied SrcOver).
    pub fn composite_overlay_onto(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_view: &wgpu::TextureView,
    ) {
        let Some(scratch) = self.scratch.as_ref() else {
            return;
        };
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rix-overlay-comp-bg"),
            layout: &self.overlay_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&scratch.overlay_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rix-overlay-comp-enc"),
        });
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rix-overlay-comp-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &bg, &[]);
            rp.draw(0..3, 0..1);
        }
        queue.submit([enc.finish()]);
    }
}

fn encode_blur_uniform(
    texel_dir: [f32; 2],
    sigma: f32,
    radius_taps: u32,
    region_uv: [f32; 4],
) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[0..4].copy_from_slice(&texel_dir[0].to_le_bytes());
    buf[4..8].copy_from_slice(&texel_dir[1].to_le_bytes());
    buf[8..12].copy_from_slice(&sigma.to_le_bytes());
    buf[12..16].copy_from_slice(&radius_taps.to_le_bytes());
    for (i, v) in region_uv.iter().enumerate() {
        let off = 16 + i * 4;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    buf
}

fn encode_backdrop_uniform(mat: &ColorFilterMatrix, bounds: Rect, radii: [f32; 4]) -> [u8; 112] {
    let mut buf = [0u8; 112];
    let rows = mat.to_wgsl_uniform_rows();
    for (r_idx, row) in rows.iter().enumerate() {
        for (c_idx, v) in row.iter().enumerate() {
            let off = r_idx * 16 + c_idx * 4;
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    let b = [
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width.max(0.0),
        bounds.size.height.max(0.0),
    ];
    for (i, v) in b.iter().enumerate() {
        let off = 80 + i * 4;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    for (i, v) in radii.iter().enumerate() {
        let off = 96 + i * 4;
        buf[off..off + 4].copy_from_slice(&v.max(0.0).to_le_bytes());
    }
    buf
}
