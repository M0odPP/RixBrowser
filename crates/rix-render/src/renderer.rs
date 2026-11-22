use rix_ipc::SurfaceId;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use thiserror::Error;
use vello::peniko::Color;
use vello::util::{RenderContext, RenderSurface};
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

use crate::canvas2d::Canvas2DContext;

struct OffscreenGpuSurface {
    texture: vello::wgpu::Texture,
    view: vello::wgpu::TextureView,
    width: u32,
    height: u32,
}

#[derive(Error, Debug)]
pub enum RenderError {
    #[error("No GPU surface attached")]
    NoSurface,
    #[error("Surface error: {0}")]
    Surface(String),
    #[error("Vello render error: {0}")]
    Vello(String),
    #[error("No compatible GPU device found")]
    NoDevice,
    #[error("Invalid render dimensions {0}x{1}")]
    BadSize(u32, u32),
    #[error("GPU readback error: {0}")]
    Readback(String),
    #[error("Image encoding error: {0}")]
    Image(#[from] image::ImageError),
}

pub struct VelloRenderer {
    context: RenderContext,
    renderer: Option<Renderer>,
    renderer_device_id: Option<usize>,
    surface: Option<RenderSurface<'static>>,
    offscreen_surfaces: HashMap<SurfaceId, OffscreenGpuSurface>,
    gpu_surface_allocations: u64,
    width: u32,
    height: u32,
}

impl Default for VelloRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl VelloRenderer {
    pub fn new() -> Self {
        Self {
            context: RenderContext::new(),
            renderer: None,
            renderer_device_id: None,
            surface: None,
            offscreen_surfaces: HashMap::new(),
            gpu_surface_allocations: 0,
            width: 0,
            height: 0,
        }
    }

    #[must_use]
    pub const fn gpu_surface_allocations(&self) -> u64 {
        self.gpu_surface_allocations
    }

    pub fn release_gpu_surface(&mut self, surface_id: SurfaceId) -> bool {
        self.offscreen_surfaces.remove(&surface_id).is_some()
    }

    pub fn has_surface(&self) -> bool {
        self.surface.is_some() && self.renderer.is_some()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Initializes the GPU swapchain once (e.g. inside `ApplicationHandler::resumed`).
    pub async fn ensure_surface(
        &mut self,
        target: impl Into<vello::wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        let width = width.max(1);
        let height = height.max(1);
        if let Some(surface) = self.surface.as_mut() {
            if self.width != width || self.height != height {
                self.context.resize_surface(surface, width, height);
                self.width = width;
                self.height = height;
            }
            return Ok(());
        }

        // Spec 02 §1: Always present with AutoVsync (Fifo fallback inside wgpu).
        let surface = self
            .context
            .create_surface(target, width, height, vello::wgpu::PresentMode::AutoVsync)
            .await
            .map_err(|e| RenderError::Surface(format!("{e:?}")))?;

        if self.renderer_device_id != Some(surface.dev_id) {
            let handle = self
                .context
                .devices
                .get(surface.dev_id)
                .ok_or(RenderError::NoDevice)?;
            let renderer = Renderer::new(&handle.device, RendererOptions::default())
                .map_err(|e| RenderError::Vello(format!("{e:?}")))?;
            self.renderer = Some(renderer);
            self.renderer_device_id = Some(surface.dev_id);
        }
        self.width = width;
        self.height = height;
        self.surface = Some(surface);
        Ok(())
    }

    /// Synchronous resize safe to call directly from `WindowEvent::Resized` with zero `block_on`.
    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return false;
        }
        if let Some(surface) = self.surface.as_mut() {
            self.context.resize_surface(surface, width, height);
        }
        self.width = width;
        self.height = height;
        true
    }

    pub fn render(&mut self, scene: &Scene, base_color: Color) -> Result<(), RenderError> {
        let (width, height) = (self.width.max(1), self.height.max(1));
        let surface = self.surface.as_mut().ok_or(RenderError::NoSurface)?;
        let renderer = self.renderer.as_mut().ok_or(RenderError::NoSurface)?;
        let handle = self
            .context
            .devices
            .get(surface.dev_id)
            .ok_or(RenderError::NoDevice)?;
        let device = &handle.device;
        let queue = &handle.queue;

        let frame = match surface.surface.get_current_texture() {
            vello::wgpu::CurrentSurfaceTexture::Success(frame)
            | vello::wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            vello::wgpu::CurrentSurfaceTexture::Outdated
            | vello::wgpu::CurrentSurfaceTexture::Lost => {
                self.context.resize_surface(surface, width, height);
                return Err(RenderError::Surface("surface outdated/lost".to_string()));
            }
            vello::wgpu::CurrentSurfaceTexture::Timeout
            | vello::wgpu::CurrentSurfaceTexture::Occluded => {
                return Err(RenderError::Surface("frame timeout/occluded".to_string()));
            }
            vello::wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::Surface("surface validation error".to_string()));
            }
        };

        let frame_view = frame
            .texture
            .create_view(&vello::wgpu::TextureViewDescriptor::default());
        let params = render_params(width, height, base_color);
        renderer
            .render_to_texture(device, queue, scene, &surface.target_view, &params)
            .map_err(|e| RenderError::Vello(format!("{e:?}")))?;

        let mut encoder = device.create_command_encoder(&vello::wgpu::CommandEncoderDescriptor {
            label: Some("rix-vello-blit"),
        });
        surface
            .blitter
            .copy(device, &mut encoder, &surface.target_view, &frame_view);
        queue.submit([encoder.finish()]);
        frame.present();
        Ok(())
    }

    pub async fn render_to_rgba(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
        base_color: Color,
    ) -> Result<Vec<u8>, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::BadSize(width, height));
        }
        let device_id = match &self.surface {
            Some(surface) => surface.dev_id,
            None => self
                .context
                .device(None)
                .await
                .ok_or(RenderError::NoDevice)?,
        };
        let handle = &self.context.devices[device_id];
        let limits = handle.device.limits();
        if width > limits.max_texture_dimension_2d || height > limits.max_texture_dimension_2d {
            return Err(RenderError::BadSize(width, height));
        }
        let row_bytes = width
            .checked_mul(4)
            .ok_or(RenderError::BadSize(width, height))?;
        let padded_row_bytes = row_bytes
            .checked_add(255)
            .map(|b| b / 256 * 256)
            .ok_or(RenderError::BadSize(width, height))?;
        let buffer_size = u64::from(padded_row_bytes) * u64::from(height);
        if buffer_size > limits.max_buffer_size {
            return Err(RenderError::BadSize(width, height));
        }
        if self.renderer_device_id != Some(device_id) {
            self.renderer = Some(
                Renderer::new(&handle.device, RendererOptions::default())
                    .map_err(|e| RenderError::Vello(format!("{e:?}")))?,
            );
            self.renderer_device_id = Some(device_id);
        }

        let device = &handle.device;
        let queue = &handle.queue;
        let size = vello::wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&vello::wgpu::TextureDescriptor {
            label: Some("rix-offscreen-frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: vello::wgpu::TextureDimension::D2,
            format: vello::wgpu::TextureFormat::Rgba8Unorm,
            usage: vello::wgpu::TextureUsages::STORAGE_BINDING
                | vello::wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        self.renderer
            .as_mut()
            .ok_or(RenderError::NoDevice)?
            .render_to_texture(
                device,
                queue,
                scene,
                &view,
                &render_params(width, height, base_color),
            )
            .map_err(|e| RenderError::Vello(format!("{e:?}")))?;

        let buffer = device.create_buffer(&vello::wgpu::BufferDescriptor {
            label: Some("rix-offscreen-readback"),
            size: buffer_size,
            usage: vello::wgpu::BufferUsages::COPY_DST | vello::wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            vello::wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: vello::wgpu::Origin3d::ZERO,
                aspect: vello::wgpu::TextureAspect::All,
            },
            vello::wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: vello::wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        let submission = queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(vello::wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let timeout = Duration::from_secs(30);
        device
            .poll(vello::wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(timeout),
            })
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv_timeout(timeout)
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;

        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity(row_bytes as usize * height as usize);
        for row in mapped.chunks_exact(padded_row_bytes as usize) {
            pixels.extend_from_slice(&row[..row_bytes as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }

    pub fn render_to_rgba_blocking(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
        base_color: Color,
    ) -> Result<Vec<u8>, RenderError> {
        pollster::block_on(self.render_to_rgba(scene, width, height, base_color))
    }

    /// Renders a GPU-backed [`Canvas2DContext`] via Vello directly into a persistent
    /// off-screen GPU `wgpu::Texture` keyed by `canvas.surface_id()` (reallocated ONLY on resize!).
    pub async fn render_canvas_to_gpu_surface(
        &mut self,
        canvas: &Canvas2DContext,
    ) -> Result<(), RenderError> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let surface_id = canvas.surface_id();
        let device_id = match self.renderer_device_id {
            Some(id) => id,
            None => self
                .context
                .device(None)
                .await
                .ok_or(RenderError::NoDevice)?,
        };
        let handle = &self.context.devices[device_id];
        let limits = handle.device.limits();
        if width > limits.max_texture_dimension_2d || height > limits.max_texture_dimension_2d {
            return Err(RenderError::BadSize(width, height));
        }
        if self.renderer_device_id != Some(device_id) {
            self.renderer = Some(
                Renderer::new(&handle.device, RendererOptions::default())
                    .map_err(|e| RenderError::Vello(format!("{e:?}")))?,
            );
            self.renderer_device_id = Some(device_id);
        }

        let device = &handle.device;
        let queue = &handle.queue;
        let needs_alloc = self
            .offscreen_surfaces
            .get(&surface_id)
            .is_none_or(|s| s.width != width || s.height != height);
        if needs_alloc {
            let texture = device.create_texture(&vello::wgpu::TextureDescriptor {
                label: Some("rix-canvas2d-gpu-surface"),
                size: vello::wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: vello::wgpu::TextureDimension::D2,
                format: vello::wgpu::TextureFormat::Rgba8Unorm,
                usage: vello::wgpu::TextureUsages::STORAGE_BINDING
                    | vello::wgpu::TextureUsages::TEXTURE_BINDING
                    | vello::wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            self.offscreen_surfaces.insert(
                surface_id,
                OffscreenGpuSurface {
                    texture,
                    view,
                    width,
                    height,
                },
            );
            self.gpu_surface_allocations += 1;
        }

        let scene = canvas.snapshot_scene();
        let target = self
            .offscreen_surfaces
            .get(&surface_id)
            .ok_or(RenderError::NoSurface)?;
        self.renderer
            .as_mut()
            .ok_or(RenderError::NoDevice)?
            .render_to_texture(
                device,
                queue,
                &scene,
                &target.view,
                &render_params(width, height, Color::TRANSPARENT),
            )
            .map_err(|e| RenderError::Vello(format!("{e:?}")))?;
        Ok(())
    }

    pub fn render_canvas_to_gpu_surface_blocking(
        &mut self,
        canvas: &Canvas2DContext,
    ) -> Result<(), RenderError> {
        pollster::block_on(self.render_canvas_to_gpu_surface(canvas))
    }

    /// Reads back RGBA8 pixels on demand from a GPU-resident `ExternalSurface` texture
    /// (called ONLY when JS invokes `getImageData`/`toDataURL`/`toBlob` or visual reftests).
    pub async fn readback_gpu_surface_rgba(
        &mut self,
        surface_id: SurfaceId,
    ) -> Result<(u32, u32, Vec<u8>), RenderError> {
        let device_id = self.renderer_device_id.ok_or(RenderError::NoDevice)?;
        let handle = &self.context.devices[device_id];
        let target = self
            .offscreen_surfaces
            .get(&surface_id)
            .ok_or(RenderError::NoSurface)?;
        let (width, height) = (target.width, target.height);
        let row_bytes = width
            .checked_mul(4)
            .ok_or(RenderError::BadSize(width, height))?;
        let padded_row_bytes = row_bytes
            .checked_add(255)
            .map(|b| b / 256 * 256)
            .ok_or(RenderError::BadSize(width, height))?;
        let buffer_size = u64::from(padded_row_bytes) * u64::from(height);
        let device = &handle.device;
        let queue = &handle.queue;
        let buffer = device.create_buffer(&vello::wgpu::BufferDescriptor {
            label: Some("rix-canvas2d-readback"),
            size: buffer_size,
            usage: vello::wgpu::BufferUsages::COPY_DST | vello::wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            vello::wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: vello::wgpu::Origin3d::ZERO,
                aspect: vello::wgpu::TextureAspect::All,
            },
            vello::wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: vello::wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes),
                    rows_per_image: Some(height),
                },
            },
            vello::wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let submission = queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(vello::wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let timeout = Duration::from_secs(30);
        device
            .poll(vello::wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(timeout),
            })
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv_timeout(timeout)
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;

        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity(row_bytes as usize * height as usize);
        for row in mapped.chunks_exact(padded_row_bytes as usize) {
            pixels.extend_from_slice(&row[..row_bytes as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok((width, height, pixels))
    }

    pub fn readback_gpu_surface_rgba_blocking(
        &mut self,
        surface_id: SurfaceId,
    ) -> Result<(u32, u32, Vec<u8>), RenderError> {
        pollster::block_on(self.readback_gpu_surface_rgba(surface_id))
    }

    pub fn save_rgba_png(
        path: impl AsRef<Path>,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        image::save_buffer_with_format(
            path,
            pixels,
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )?;
        Ok(())
    }

    pub fn render_scene_to_png(
        scene: &Scene,
        width: u32,
        height: u32,
        base_color: Color,
        path: impl AsRef<Path>,
    ) -> Result<(), RenderError> {
        let mut renderer = Self::new();
        let pixels = renderer.render_to_rgba_blocking(scene, width, height, base_color)?;
        Self::save_rgba_png(path, &pixels, width, height)
    }
}

fn render_params(width: u32, height: u32, base_color: Color) -> RenderParams {
    RenderParams {
        base_color,
        width,
        height,
        // Spec 02 §1.5: Analytic area AA is default across HiDPI displays.
        antialiasing_method: AaConfig::Area,
    }
}
