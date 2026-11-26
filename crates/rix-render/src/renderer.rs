use rix_ipc::SurfaceId;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use thiserror::Error;
use vello::peniko::{Color, ImageData};
use vello::util::{RenderContext, RenderSurface};
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

use crate::canvas2d::Canvas2DContext;
use crate::compositor::{BackdropCompositePlan, Compositor};
use crate::gpu_context::SharedGpuDevice;
use crate::gpu_filters::GpuFilterPipeline;

static NEXT_RENDERER_TOKEN: AtomicU64 = AtomicU64::new(1);

struct OffscreenGpuSurface {
    texture: vello::wgpu::Texture,
    view: vello::wgpu::TextureView,
    width: u32,
    height: u32,
}

struct RegisteredExternalGpuTexture {
    image: ImageData,
    width: u32,
    height: u32,
    blit_target: Option<vello::wgpu::Texture>,
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
    renderer_token: u64,
    context: RenderContext,
    renderer: Option<Renderer>,
    renderer_device_id: Option<usize>,
    surface: Option<RenderSurface<'static>>,
    offscreen_surfaces: HashMap<SurfaceId, OffscreenGpuSurface>,
    external_gpu_textures: HashMap<SurfaceId, RegisteredExternalGpuTexture>,
    rgba_blitter: Option<vello::wgpu::util::TextureBlitter>,
    gpu_filters: Option<GpuFilterPipeline>,
    backdrop_target: Option<OffscreenGpuSurface>,
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
        // Serialize Vulkan/wgpu loader initialization across concurrent threads (Linux
        // `libvulkan.so.1` races on simultaneous first-time `vkEnumerateInstanceExtensionProperties`).
        static RENDER_CTX_INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let context = {
            let _guard = RENDER_CTX_INIT_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            RenderContext::new()
        };
        Self {
            renderer_token: NEXT_RENDERER_TOKEN.fetch_add(1, Ordering::Relaxed),
            context,
            renderer: None,
            renderer_device_id: None,
            surface: None,
            offscreen_surfaces: HashMap::new(),
            external_gpu_textures: HashMap::new(),
            rgba_blitter: None,
            gpu_filters: None,
            backdrop_target: None,
            gpu_surface_allocations: 0,
            width: 0,
            height: 0,
        }
    }

    /// Unique runtime token identifying this `VelloRenderer`'s `wgpu::Device` for
    /// zero-copy GPU-to-GPU external texture registration.
    #[must_use]
    pub const fn renderer_token(&self) -> u64 {
        self.renderer_token
    }

    /// Returns cloned `wgpu` handles (`Adapter`, `Device`, `Queue`) backed by this renderer's
    /// active GPU device, initializing the headless device and Vello renderer if needed.
    pub async fn ensure_shared_gpu_device(&mut self) -> Result<SharedGpuDevice, RenderError> {
        let device_id = match self.renderer_device_id {
            Some(id) => id,
            None => match &self.surface {
                Some(surface) => surface.dev_id,
                None => self
                    .context
                    .device(None)
                    .await
                    .ok_or(RenderError::NoDevice)?,
            },
        };
        let handle = self
            .context
            .devices
            .get(device_id)
            .ok_or(RenderError::NoDevice)?;
        if self.renderer_device_id != Some(device_id) {
            self.renderer = Some(
                Renderer::new(&handle.device, RendererOptions::default())
                    .map_err(|e| RenderError::Vello(format!("{e:?}")))?,
            );
            self.renderer_device_id = Some(device_id);
        }
        Ok(SharedGpuDevice {
            adapter: handle.adapter().clone(),
            device: handle.device.clone(),
            queue: handle.queue.clone(),
            renderer_token: self.renderer_token,
        })
    }

    /// Blocking convenience wrapper around [`Self::ensure_shared_gpu_device`].
    pub fn ensure_shared_gpu_device_blocking(&mut self) -> Result<SharedGpuDevice, RenderError> {
        pollster::block_on(self.ensure_shared_gpu_device())
    }

    /// Returns the active [`SharedGpuDevice`] if the GPU device is already initialized.
    #[must_use]
    pub fn shared_gpu_device(&self) -> Option<SharedGpuDevice> {
        let id = self.renderer_device_id?;
        let handle = self.context.devices.get(id)?;
        Some(SharedGpuDevice {
            adapter: handle.adapter().clone(),
            device: handle.device.clone(),
            queue: handle.queue.clone(),
            renderer_token: self.renderer_token,
        })
    }

    /// Registers or updates a GPU-resident `wgpu::Texture` (owned by this renderer's device)
    /// as a zero-copy Vello external image for `surface_id`, returning an [`ImageData`] handle
    /// that can be passed directly to [`crate::Compositor::present_external_surface_image`].
    ///
    /// If `texture` is `Rgba8Unorm` with `COPY_SRC`, it is registered directly without any
    /// intermediate blit or CPU readback. Other renderable formats (such as `Bgra8Unorm`) are
    /// converted GPU-to-GPU via `wgpu::util::TextureBlitter`.
    pub fn register_external_gpu_texture(
        &mut self,
        surface_id: SurfaceId,
        texture: vello::wgpu::Texture,
    ) -> Result<ImageData, RenderError> {
        let device_id = self.renderer_device_id.ok_or(RenderError::NoDevice)?;
        let width = texture.width();
        let height = texture.height();
        if width == 0 || height == 0 {
            return Err(RenderError::BadSize(width, height));
        }

        let direct_compatible = texture.format() == vello::wgpu::TextureFormat::Rgba8Unorm
            && texture
                .usage()
                .contains(vello::wgpu::TextureUsages::COPY_SRC);

        let (final_texture, new_blit_target) = if direct_compatible {
            (texture, None)
        } else {
            let handle = self
                .context
                .devices
                .get(device_id)
                .ok_or(RenderError::NoDevice)?;
            let device = &handle.device;
            let queue = &handle.queue;
            let reuse_blit = self
                .external_gpu_textures
                .get(&surface_id)
                .and_then(|entry| entry.blit_target.as_ref())
                .filter(|t| t.width() == width && t.height() == height)
                .cloned();
            let dst = reuse_blit.unwrap_or_else(|| {
                device.create_texture(&vello::wgpu::TextureDescriptor {
                    label: Some("rix-external-gpu-rgba-blit"),
                    size: vello::wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: vello::wgpu::TextureDimension::D2,
                    format: vello::wgpu::TextureFormat::Rgba8Unorm,
                    usage: vello::wgpu::TextureUsages::RENDER_ATTACHMENT
                        | vello::wgpu::TextureUsages::COPY_SRC
                        | vello::wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
            });
            let blitter = self.rgba_blitter.get_or_insert_with(|| {
                vello::wgpu::util::TextureBlitter::new(
                    device,
                    vello::wgpu::TextureFormat::Rgba8Unorm,
                )
            });
            let src_view = texture.create_view(&vello::wgpu::TextureViewDescriptor::default());
            let dst_view = dst.create_view(&vello::wgpu::TextureViewDescriptor::default());
            let mut encoder =
                device.create_command_encoder(&vello::wgpu::CommandEncoderDescriptor {
                    label: Some("rix-external-gpu-rgba-blit-enc"),
                });
            blitter.copy(device, &mut encoder, &src_view, &dst_view);
            queue.submit([encoder.finish()]);
            (dst.clone(), Some(dst))
        };

        let renderer = self.renderer.as_mut().ok_or(RenderError::NoDevice)?;
        if let Some(existing) = self.external_gpu_textures.get_mut(&surface_id) {
            if existing.width == width && existing.height == height {
                let copy_base = vello::wgpu::TexelCopyTextureInfoBase {
                    texture: final_texture,
                    mip_level: 0,
                    origin: vello::wgpu::Origin3d::ZERO,
                    aspect: vello::wgpu::TextureAspect::All,
                };
                renderer.override_image(&existing.image, Some(copy_base));
                existing.blit_target = new_blit_target;
                return Ok(existing.image.clone());
            }
        }
        if let Some(old) = self.external_gpu_textures.remove(&surface_id) {
            renderer.unregister_texture(old.image);
        }
        let image = renderer.register_texture(final_texture);
        self.external_gpu_textures.insert(
            surface_id,
            RegisteredExternalGpuTexture {
                image: image.clone(),
                width,
                height,
                blit_target: new_blit_target,
            },
        );
        self.gpu_surface_allocations += 1;
        Ok(image)
    }

    /// Unregisters a previously registered external GPU texture for `surface_id`.
    pub fn unregister_external_gpu_texture(&mut self, surface_id: SurfaceId) -> bool {
        if let Some(old) = self.external_gpu_textures.remove(&surface_id) {
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.unregister_texture(old.image);
            }
            true
        } else {
            false
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
        self.render_with_backdrop_plan(scene, None, base_color)
    }

    /// Renders `scene` onto the swapchain, executing real GPU WGSL `backdrop-filter` passes
    /// whenever `plan` is `Some` (zero CPU readback, exact painter's order).
    pub fn render_with_backdrop_plan(
        &mut self,
        scene: &Scene,
        plan: Option<&BackdropCompositePlan>,
        base_color: Color,
    ) -> Result<(), RenderError> {
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

        if let Some(bd_plan) = plan.filter(|p| !p.steps.is_empty()) {
            let needs_alloc = self
                .backdrop_target
                .as_ref()
                .is_none_or(|t| t.width != width || t.height != height);
            if needs_alloc {
                let texture = device.create_texture(&vello::wgpu::TextureDescriptor {
                    label: Some("rix-backdrop-swapchain-target"),
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
                        | vello::wgpu::TextureUsages::RENDER_ATTACHMENT
                        | vello::wgpu::TextureUsages::COPY_SRC
                        | vello::wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                self.backdrop_target = Some(OffscreenGpuSurface {
                    texture,
                    view,
                    width,
                    height,
                });
            }
            let target = self.backdrop_target.as_ref().unwrap();
            let filters = self
                .gpu_filters
                .get_or_insert_with(|| GpuFilterPipeline::new(device));
            execute_backdrop_plan(
                device,
                queue,
                renderer,
                filters,
                &target.texture,
                &target.view,
                width,
                height,
                base_color,
                bd_plan,
            )?;
            let mut encoder =
                device.create_command_encoder(&vello::wgpu::CommandEncoderDescriptor {
                    label: Some("rix-vello-backdrop-blit"),
                });
            surface
                .blitter
                .copy(device, &mut encoder, &target.view, &frame_view);
            queue.submit([encoder.finish()]);
            frame.present();
            return Ok(());
        }

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

    /// Composes `compositor` and renders the frame to RGBA8 on the GPU, including real
    /// WGSL GPU `backdrop-filter` passes whenever present (exact painter's order).
    pub async fn render_compositor_to_rgba(
        &mut self,
        compositor: &mut Compositor,
        width: u32,
        height: u32,
        base_color: Color,
    ) -> Result<Vec<u8>, RenderError> {
        let _ = compositor.compose_force();
        let Some(plan) = compositor.backdrop_plan() else {
            let scene = compositor.last_composed_scene();
            return self.render_to_rgba(scene, width, height, base_color).await;
        };
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
            label: Some("rix-offscreen-backdrop-frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: vello::wgpu::TextureDimension::D2,
            format: vello::wgpu::TextureFormat::Rgba8Unorm,
            usage: vello::wgpu::TextureUsages::STORAGE_BINDING
                | vello::wgpu::TextureUsages::TEXTURE_BINDING
                | vello::wgpu::TextureUsages::RENDER_ATTACHMENT
                | vello::wgpu::TextureUsages::COPY_SRC
                | vello::wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let renderer = self.renderer.as_mut().ok_or(RenderError::NoDevice)?;
        let filters = self
            .gpu_filters
            .get_or_insert_with(|| GpuFilterPipeline::new(device));
        execute_backdrop_plan(
            device, queue, renderer, filters, &texture, &view, width, height, base_color, plan,
        )?;

        let buffer = device.create_buffer(&vello::wgpu::BufferDescriptor {
            label: Some("rix-offscreen-backdrop-readback"),
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

    pub fn render_compositor_to_rgba_blocking(
        &mut self,
        compositor: &mut Compositor,
        width: u32,
        height: u32,
        base_color: Color,
    ) -> Result<Vec<u8>, RenderError> {
        pollster::block_on(self.render_compositor_to_rgba(compositor, width, height, base_color))
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

    /// Pre-warms the GPU adapter/device and Vello compute shader pipelines for off-screen
    /// rendering so GPU shader compilation overlaps with network I/O during cold navigation.
    pub fn warm_up_headless_blocking(&mut self) -> Result<(), RenderError> {
        pollster::block_on(async {
            let device_id = match &self.surface {
                Some(surface) => surface.dev_id,
                None => self
                    .context
                    .device(None)
                    .await
                    .ok_or(RenderError::NoDevice)?,
            };
            if self.renderer_device_id != Some(device_id) {
                let handle = &self.context.devices[device_id];
                self.renderer = Some(
                    Renderer::new(&handle.device, RendererOptions::default())
                        .map_err(|e| RenderError::Vello(format!("{e:?}")))?,
                );
                self.renderer_device_id = Some(device_id);
            }
            Ok(())
        })
    }

    pub fn save_rgba_png(
        path: impl AsRef<Path>,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use image::ImageEncoder;
        let file = std::fs::File::create(path).map_err(image::ImageError::IoError)?;
        let writer = std::io::BufWriter::new(file);
        let encoder = PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Sub);
        encoder.write_image(pixels, width, height, image::ExtendedColorType::Rgba8)?;
        Ok(())
    }

    /// Renders `scene` using this already-warmed renderer and saves the result to PNG.
    pub fn render_scene_to_png_warm(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
        base_color: Color,
        path: impl AsRef<Path>,
    ) -> Result<(), RenderError> {
        let pixels = self.render_to_rgba_blocking(scene, width, height, base_color)?;
        Self::save_rgba_png(path, &pixels, width, height)
    }

    pub fn render_scene_to_png(
        scene: &Scene,
        width: u32,
        height: u32,
        base_color: Color,
        path: impl AsRef<Path>,
    ) -> Result<(), RenderError> {
        let mut renderer = Self::new();
        renderer.render_scene_to_png_warm(scene, width, height, base_color, path)
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_backdrop_plan(
    device: &vello::wgpu::Device,
    queue: &vello::wgpu::Queue,
    renderer: &mut Renderer,
    filters: &mut GpuFilterPipeline,
    target_tex: &vello::wgpu::Texture,
    target_view: &vello::wgpu::TextureView,
    width: u32,
    height: u32,
    base_color: Color,
    plan: &BackdropCompositePlan,
) -> Result<(), RenderError> {
    renderer
        .render_to_texture(
            device,
            queue,
            &plan.base_scene,
            target_view,
            &render_params(width, height, base_color),
        )
        .map_err(|e| RenderError::Vello(format!("{e:?}")))?;

    for step in &plan.steps {
        filters.apply_backdrop_pass(device, queue, target_tex, target_view, &step.pass);
        let overlay_view = filters.overlay_view(device, width, height);
        renderer
            .render_to_texture(
                device,
                queue,
                &step.overlay_scene,
                overlay_view,
                &render_params(width, height, Color::TRANSPARENT),
            )
            .map_err(|e| RenderError::Vello(format!("{e:?}")))?;
        filters.composite_overlay_onto(device, queue, target_view);
    }
    Ok(())
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
