//! Zero-copy `wgpu` BT.709 / BT.601 YUV420p & NV12 -> RGBA video shader layer.
//!
//! Uploads planar `Y`, `U`, `V` (`R8Unorm`) or `Y` (`R8Unorm`) + interleaved `UV` (`Rg8Unorm`)
//! textures directly onto the compositor's [`SharedGpuDevice`], converts YUV->RGBA in a single
//! fullscreen fragment shader pass into a cached `Rgba8Unorm` GPU texture, and binds it to
//! `DisplayItem::ExternalSurface` via [`VelloRenderer::register_external_gpu_texture`] and
//! [`Compositor::present_external_surface_image`] — with **zero CPU pixel conversion loops**,
//! **zero DOM mutations**, and **zero `DisplayList` rebuilds**.

use std::collections::HashMap;

use rix_ipc::SurfaceId;
use vello::wgpu;

use crate::compositor::Compositor;
use crate::gpu_context::SharedGpuDevice;
use crate::renderer::{RenderError, VelloRenderer};

/// 4-byte binary header magic matching `rix-media::YUV_SHMEM_MAGIC`.
const YUV_SHMEM_MAGIC: [u8; 4] = *b"RYUV";
const YUV_SHMEM_VERSION: u32 = 1;
const YUV_SHMEM_HEADER_BYTES: usize = 64;

/// Planar YUV pixel layout understood by [`YuvVideoRenderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvPixelFormat {
    /// 3-plane 8-bit 4:2:0 (`Y` in `R8Unorm`, `U` in `R8Unorm`, `V` in `R8Unorm`).
    Yuv420p = 0,
    /// 2-plane 8-bit 4:2:0 (`Y` in `R8Unorm`, interleaved `UV` in `Rg8Unorm`).
    Nv12 = 1,
}

/// Color conversion matrix standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvColorMatrix {
    /// ITU-R BT.709 (HD / 1080p / 4K web video default).
    Bt709 = 0,
    /// ITU-R BT.601 (SD / VP8 / legacy web video).
    Bt601 = 1,
}

/// YUV signal quantization range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvColorRange {
    /// Studio / video range (`Y: 16..=235`, `UV: 16..=240`).
    Limited = 0,
    /// Full / PC range (`0..=255`).
    Full = 1,
}

/// Borrowed descriptor of a planar YUV420p or NV12 video frame ready for GPU upload.
#[derive(Debug, Clone, Copy)]
pub struct YuvFrameUpload<'a> {
    pub width: u32,
    pub height: u32,
    pub pixel_format: YuvPixelFormat,
    pub color_matrix: YuvColorMatrix,
    pub color_range: YuvColorRange,
    pub y_plane: &'a [u8],
    /// U plane for `Yuv420p` or interleaved UV plane for `Nv12`.
    pub u_plane: &'a [u8],
    /// V plane for `Yuv420p` (ignored for `Nv12`).
    pub v_plane: &'a [u8],
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
    pub pts_us: i64,
}

impl<'a> YuvFrameUpload<'a> {
    /// Parses a borrowed [`YuvFrameUpload`] directly out of a `rix-ipc::TripleBufferConsumer`
    /// shared-memory slot with zero allocations and zero pixel copies.
    #[must_use]
    pub fn from_shmem_slot(slot: &'a [u8]) -> Option<Self> {
        if slot.len() < YUV_SHMEM_HEADER_BYTES || slot[0..4] != YUV_SHMEM_MAGIC {
            return None;
        }
        let u32_at = |off: usize| -> u32 {
            u32::from_le_bytes([slot[off], slot[off + 1], slot[off + 2], slot[off + 3]])
        };
        if u32_at(4) != YUV_SHMEM_VERSION {
            return None;
        }
        let width = u32_at(8);
        let height = u32_at(12);
        let pixel_format = match u32_at(16) {
            0 => YuvPixelFormat::Yuv420p,
            1 => YuvPixelFormat::Nv12,
            _ => return None,
        };
        let color_matrix = match u32_at(20) {
            0 => YuvColorMatrix::Bt709,
            1 => YuvColorMatrix::Bt601,
            _ => return None,
        };
        let color_range = match u32_at(24) {
            0 => YuvColorRange::Limited,
            1 => YuvColorRange::Full,
            _ => return None,
        };
        let y_stride = u32_at(28);
        let u_stride = u32_at(32);
        let v_stride = u32_at(36);
        let y_len = u32_at(40) as usize;
        let u_len = u32_at(44) as usize;
        let v_len = u32_at(48) as usize;
        let pts_us = i64::from_le_bytes([
            slot[56], slot[57], slot[58], slot[59], slot[60], slot[61], slot[62], slot[63],
        ]);

        let y_end = YUV_SHMEM_HEADER_BYTES.checked_add(y_len)?;
        let u_end = y_end.checked_add(u_len)?;
        let v_end = u_end.checked_add(v_len)?;
        if v_end > slot.len() {
            return None;
        }

        Some(Self {
            width,
            height,
            pixel_format,
            color_matrix,
            color_range,
            y_plane: &slot[YUV_SHMEM_HEADER_BYTES..y_end],
            u_plane: &slot[y_end..u_end],
            v_plane: &slot[u_end..v_end],
            y_stride,
            u_stride,
            v_stride,
            pts_us,
        })
    }
}

const YUV_TO_RGBA_WGSL: &str = r#"
struct YuvParams {
    pixel_format: u32,
    color_matrix: u32,
    color_range: u32,
    _pad: u32,
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

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var tex_uv: texture_2d<f32>;
@group(0) @binding(4) var video_sampler: sampler;
@group(0) @binding(5) var<uniform> params: YuvParams;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y_sample = textureSample(tex_y, video_sampler, in.uv).r;
    var u_sample: f32;
    var v_sample: f32;
    if (params.pixel_format == 0u) {
        u_sample = textureSample(tex_u, video_sampler, in.uv).r;
        v_sample = textureSample(tex_v, video_sampler, in.uv).r;
    } else {
        let uv_pair = textureSample(tex_uv, video_sampler, in.uv).rg;
        u_sample = uv_pair.x;
        v_sample = uv_pair.y;
    }

    var y: f32;
    var cb: f32;
    var cr: f32;
    if (params.color_range == 0u) {
        // Studio / limited range: Y in [16..235], Cb/Cr in [16..240]
        y = (y_sample - (16.0 / 255.0)) * (255.0 / 219.0);
        cb = (u_sample - (128.0 / 255.0)) * (255.0 / 224.0);
        cr = (v_sample - (128.0 / 255.0)) * (255.0 / 224.0);
    } else {
        // Full / PC range: [0..255]
        y = y_sample;
        cb = u_sample - (128.0 / 255.0);
        cr = v_sample - (128.0 / 255.0);
    }

    var r: f32;
    var g: f32;
    var b: f32;
    if (params.color_matrix == 0u) {
        // ITU-R BT.709
        r = y + 1.5748 * cr;
        g = y - 0.187324 * cb - 0.468124 * cr;
        b = y + 1.8556 * cb;
    } else {
        // ITU-R BT.601
        r = y + 1.402 * cr;
        g = y - 0.344136 * cb - 0.714136 * cr;
        b = y + 1.772 * cb;
    }

    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
"#;

struct CachedVideoSurface {
    width: u32,
    height: u32,
    pixel_format: YuvPixelFormat,
    tex_y: wgpu::Texture,
    tex_u: wgpu::Texture,
    tex_v: wgpu::Texture,
    tex_uv: wgpu::Texture,
    params_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    rgba_target: wgpu::Texture,
    rgba_view: wgpu::TextureView,
    registered: bool,
    frames_presented: u64,
}

/// GPU-side planar YUV420p/NV12 -> RGBA converter and zero-copy external surface presenter.
pub struct YuvVideoRenderer {
    gpu: SharedGpuDevice,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    surfaces: HashMap<SurfaceId, CachedVideoSurface>,
}

impl YuvVideoRenderer {
    /// Compiles the `wgpu` BT.709 / BT.601 YUV->RGBA shader pipeline on `gpu`.
    #[must_use]
    pub fn new(gpu: SharedGpuDevice) -> Self {
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("rix-yuv-to-rgba-shader"),
                source: wgpu::ShaderSource::Wgsl(YUV_TO_RGBA_WGSL.into()),
            });

        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };

        let bind_group_layout =
            gpu.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("rix-yuv-bgl"),
                    entries: &[
                        tex_entry(0),
                        tex_entry(1),
                        tex_entry(2),
                        tex_entry(3),
                        wgpu::BindGroupLayoutEntry {
                            binding: 4,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 5,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });

        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("rix-yuv-pipeline-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });

        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("rix-yuv-to-rgba-pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
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
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });

        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rix-yuv-linear-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Self {
            gpu,
            bind_group_layout,
            pipeline,
            sampler,
            surfaces: HashMap::new(),
        }
    }

    /// Returns the shared GPU device backing this video renderer.
    #[must_use]
    pub fn gpu(&self) -> &SharedGpuDevice {
        &self.gpu
    }

    /// Removes and frees cached GPU textures for `surface_id`.
    pub fn remove_surface(&mut self, surface_id: SurfaceId) {
        self.surfaces.remove(&surface_id);
    }

    /// Returns how many frames have been presented for `surface_id`.
    #[must_use]
    pub fn frames_presented(&self, surface_id: SurfaceId) -> u64 {
        self.surfaces
            .get(&surface_id)
            .map_or(0, |s| s.frames_presented)
    }

    /// Uploads planar YUV data straight from a `TripleBufferConsumer::try_acquire()` shared-memory
    /// slot onto the GPU, converts YUV->RGBA via fragment shader, and binds the persistent GPU
    /// texture zero-copy into `compositor` with **zero DisplayList rebuilds**.
    pub fn upload_from_shmem_slot(
        &mut self,
        surface_id: SurfaceId,
        slot_bytes: &[u8],
        vello: &mut VelloRenderer,
        compositor: &mut Compositor,
    ) -> Result<bool, RenderError> {
        let Some(frame) = YuvFrameUpload::from_shmem_slot(slot_bytes) else {
            return Ok(false);
        };
        self.upload_and_present(surface_id, &frame, vello, compositor)?;
        Ok(true)
    }

    /// Uploads planar YUV420p or NV12 planes to GPU textures, executes the BT.709/BT.601 fragment
    /// shader into the cached `Rgba8Unorm` texture, and presents it zero-copy into `compositor`.
    pub fn upload_and_present(
        &mut self,
        surface_id: SurfaceId,
        frame: &YuvFrameUpload<'_>,
        vello: &mut VelloRenderer,
        compositor: &mut Compositor,
    ) -> Result<(), RenderError> {
        if frame.width == 0 || frame.height == 0 {
            return Err(RenderError::BadSize(frame.width, frame.height));
        }

        let needs_alloc = self.surfaces.get(&surface_id).is_none_or(|s| {
            s.width != frame.width
                || s.height != frame.height
                || s.pixel_format != frame.pixel_format
        });
        if needs_alloc {
            let prev_frames = self
                .surfaces
                .get(&surface_id)
                .map_or(0, |s| s.frames_presented);
            let mut cached =
                self.allocate_surface_resources(frame.width, frame.height, frame.pixel_format);
            cached.frames_presented = prev_frames;
            self.surfaces.insert(surface_id, cached);
        }

        let surface = self.surfaces.get_mut(&surface_id).unwrap();
        let cw = frame.width.div_ceil(2);
        let ch = frame.height.div_ceil(2);

        // Upload Y plane (`R8Unorm`).
        upload_plane(
            &self.gpu.queue,
            &surface.tex_y,
            frame.y_plane,
            frame.y_stride.max(frame.width),
            frame.width,
            frame.height,
        );

        match frame.pixel_format {
            YuvPixelFormat::Yuv420p => {
                upload_plane(
                    &self.gpu.queue,
                    &surface.tex_u,
                    frame.u_plane,
                    frame.u_stride.max(cw),
                    cw,
                    ch,
                );
                upload_plane(
                    &self.gpu.queue,
                    &surface.tex_v,
                    frame.v_plane,
                    frame.v_stride.max(cw),
                    cw,
                    ch,
                );
            }
            YuvPixelFormat::Nv12 => {
                upload_plane(
                    &self.gpu.queue,
                    &surface.tex_uv,
                    frame.u_plane,
                    frame.u_stride.max(cw * 2),
                    cw,
                    ch,
                );
            }
        }

        // Update uniform params (format, color matrix, color range).
        let mut uniform_bytes = [0u8; 16];
        uniform_bytes[0..4].copy_from_slice(&(frame.pixel_format as u32).to_le_bytes());
        uniform_bytes[4..8].copy_from_slice(&(frame.color_matrix as u32).to_le_bytes());
        uniform_bytes[8..12].copy_from_slice(&(frame.color_range as u32).to_le_bytes());
        self.gpu
            .queue
            .write_buffer(&surface.params_buf, 0, &uniform_bytes);

        // Execute single fullscreen YUV->RGBA fragment shader pass into `rgba_target`.
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rix-yuv-convert-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rix-yuv-to-rgba-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface.rgba_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &surface.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.gpu.queue.submit(Some(encoder.finish()));

        // Register persistent GPU texture zero-copy into Vello & present on ExternalSurface.
        // Because `rgba_target` is `Rgba8Unorm | COPY_SRC`, Vello binds it directly GPU-to-GPU
        // with zero CPU readback and zero DisplayList rebuilds.
        let image = vello.register_external_gpu_texture(surface_id, surface.rgba_target.clone())?;
        compositor.present_external_surface_image(surface_id, image);
        surface.registered = true;
        surface.frames_presented = surface.frames_presented.saturating_add(1);
        Ok(())
    }

    fn allocate_surface_resources(
        &self,
        width: u32,
        height: u32,
        pixel_format: YuvPixelFormat,
    ) -> CachedVideoSurface {
        let cw = width.div_ceil(2).max(1);
        let ch = height.div_ceil(2).max(1);

        let create_2d = |label: &str, w: u32, h: u32, format: wgpu::TextureFormat| {
            self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };

        let tex_y = create_2d("rix-yuv-y", width, height, wgpu::TextureFormat::R8Unorm);
        let (u_w, u_h, uv_w, uv_h) = match pixel_format {
            YuvPixelFormat::Yuv420p => (cw, ch, 1, 1),
            YuvPixelFormat::Nv12 => (1, 1, cw, ch),
        };
        let tex_u = create_2d("rix-yuv-u", u_w, u_h, wgpu::TextureFormat::R8Unorm);
        let tex_v = create_2d("rix-yuv-v", u_w, u_h, wgpu::TextureFormat::R8Unorm);
        let tex_uv = create_2d("rix-yuv-uv", uv_w, uv_h, wgpu::TextureFormat::Rg8Unorm);

        let params_buf = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rix-yuv-params-ubo"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let view_y = tex_y.create_view(&wgpu::TextureViewDescriptor::default());
        let view_u = tex_u.create_view(&wgpu::TextureViewDescriptor::default());
        let view_v = tex_v.create_view(&wgpu::TextureViewDescriptor::default());
        let view_uv = tex_uv.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rix-yuv-bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view_y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view_u),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&view_v),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&view_uv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: params_buf.as_entire_binding(),
                    },
                ],
            });

        let rgba_target = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rix-yuv-rgba-out"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let rgba_view = rgba_target.create_view(&wgpu::TextureViewDescriptor::default());

        CachedVideoSurface {
            width,
            height,
            pixel_format,
            tex_y,
            tex_u,
            tex_v,
            tex_uv,
            params_buf,
            bind_group,
            rgba_target,
            rgba_view,
            registered: false,
            frames_presented: 0,
        }
    }
}

fn upload_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: &[u8],
    bytes_per_row: u32,
    width: u32,
    height: u32,
) {
    let needed = (bytes_per_row as usize).saturating_mul(height as usize);
    if bytes.len() < needed {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes[..needed],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}
