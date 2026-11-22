//! Shared `wgpu` headless device helpers, buffer readback, and texture readback
//! utilities used by WebGPU (`navigator.gpu`), WebGL2, and GPU media layers.
//!
//! Reference: Spec 02 §4, Spec 06 §3, Issue #26.

use std::sync::mpsc;
use std::time::Duration;

use vello::wgpu;

use crate::renderer::RenderError;

const GPU_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Cloned `wgpu` handles (Arc-backed inside `wgpu`) shared between the compositor
/// (`VelloRenderer`) and GPU Web APIs (`WebGPU`, `WebGL2`, video shaders) so textures
/// can be composited GPU-to-GPU without CPU pixel readback.
#[derive(Clone, Debug)]
pub struct SharedGpuDevice {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Non-zero token when this device is owned by a specific [`crate::VelloRenderer`],
    /// enabling zero-copy GPU-to-GPU `ExternalSurface` compositing via `VelloRenderer::register_external_gpu_texture`.
    pub renderer_token: u64,
}

impl SharedGpuDevice {
    #[must_use]
    pub fn adapter_info(&self) -> wgpu::AdapterInfo {
        self.adapter.get_info()
    }

    #[must_use]
    pub fn limits(&self) -> wgpu::Limits {
        self.device.limits()
    }
}

/// Creates a `wgpu::Instance` configured from environment defaults without requiring a
/// window display handle.
#[must_use]
pub fn create_wgpu_instance() -> wgpu::Instance {
    let backends = wgpu::Backends::from_env().unwrap_or_default();
    let flags = wgpu::InstanceFlags::from_build_config().with_env();
    let memory_budget_thresholds = wgpu::MemoryBudgetThresholds::default();
    let backend_options = wgpu::BackendOptions::from_env_or_default();
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        display: None,
        backends,
        flags,
        memory_budget_thresholds,
        backend_options,
    })
}

/// Requests a headless `wgpu::Adapter` synchronously.
pub fn request_headless_adapter(
    power_preference: wgpu::PowerPreference,
    force_fallback_adapter: bool,
) -> Result<wgpu::Adapter, RenderError> {
    let instance = create_wgpu_instance();
    let opts = wgpu::RequestAdapterOptions {
        power_preference,
        compatible_surface: None,
        force_fallback_adapter,
    };
    pollster::block_on(instance.request_adapter(&opts)).map_err(|_| RenderError::NoDevice)
}

/// Requests a logical `wgpu::Device` and `wgpu::Queue` from `adapter`.
pub fn request_device_from_adapter(
    adapter: &wgpu::Adapter,
    label: Option<&str>,
    required_features: wgpu::Features,
    required_limits: wgpu::Limits,
) -> Result<(wgpu::Device, wgpu::Queue), RenderError> {
    let desc = wgpu::DeviceDescriptor {
        label,
        required_features,
        required_limits,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    };
    pollster::block_on(adapter.request_device(&desc))
        .map_err(|e| RenderError::Surface(format!("request_device failed: {e}")))
}

/// Creates a standalone headless `SharedGpuDevice` with adapter defaults.
pub fn create_headless_gpu_device(
    power_preference: wgpu::PowerPreference,
    force_fallback_adapter: bool,
) -> Result<SharedGpuDevice, RenderError> {
    let adapter = request_headless_adapter(power_preference, force_fallback_adapter)?;
    let (device, queue) = request_device_from_adapter(
        &adapter,
        Some("rix-headless-webgpu-device"),
        wgpu::Features::empty(),
        adapter.limits(),
    )?;
    Ok(SharedGpuDevice {
        adapter,
        device,
        queue,
        renderer_token: 0,
    })
}

/// Reads back an entire mip-0 2D `wgpu::Texture` into tightly packed RGBA8 bytes.
/// Supports `Rgba8Unorm`, `Rgba8UnormSrgb`, `Bgra8Unorm`, and `Bgra8UnormSrgb`.
pub fn readback_wgpu_texture_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Result<Vec<u8>, RenderError> {
    let size = texture.size();
    let width = size.width;
    let height = size.height;
    if width == 0 || height == 0 {
        return Err(RenderError::BadSize(width, height));
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or(RenderError::BadSize(width, height))?;
    let padded_row_bytes = row_bytes
        .checked_add(255)
        .map(|b| (b / 256) * 256)
        .ok_or(RenderError::BadSize(width, height))?;
    let buffer_size = u64::from(padded_row_bytes) * u64::from(height);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rix-gpu-texture-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rix-gpu-texture-readback-enc"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let submission = queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(GPU_POLL_TIMEOUT),
        })
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    rx.recv_timeout(GPU_POLL_TIMEOUT)
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;

    let swap_rb = matches!(
        texture.format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity(row_bytes as usize * height as usize);
    for row in mapped.chunks_exact(padded_row_bytes as usize) {
        let valid = &row[..row_bytes as usize];
        if swap_rb {
            for px in valid.as_chunks::<4>().0 {
                pixels.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        } else {
            pixels.extend_from_slice(valid);
        }
    }
    drop(mapped);
    staging.unmap();
    Ok(pixels)
}

/// Synchronously maps `[offset .. offset + size]` of a `MAP_READ` `wgpu::Buffer` and copies
/// the GPU bytes into a `Vec<u8>`.
pub fn readback_wgpu_buffer_range(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>, RenderError> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| RenderError::Readback("buffer map range overflow".to_string()))?;
    if end > buffer.size() {
        return Err(RenderError::Readback(
            "buffer map range out of bounds".to_string(),
        ));
    }
    let slice = buffer.slice(offset..end);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(GPU_POLL_TIMEOUT),
        })
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    rx.recv_timeout(GPU_POLL_TIMEOUT)
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let bytes = slice.get_mapped_range().to_vec();
    buffer.unmap();
    Ok(bytes)
}

/// Synchronously maps `[offset .. offset + data.len()]` of a `MAP_WRITE` `wgpu::Buffer`,
/// copies `data` into the mapped range, and unmaps.
pub fn write_wgpu_buffer_mapped(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    offset: u64,
    data: &[u8],
) -> Result<(), RenderError> {
    let end = offset
        .checked_add(data.len() as u64)
        .ok_or_else(|| RenderError::Readback("buffer write range overflow".to_string()))?;
    if end > buffer.size() {
        return Err(RenderError::Readback(
            "buffer write range out of bounds".to_string(),
        ));
    }
    let slice = buffer.slice(offset..end);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Write, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(GPU_POLL_TIMEOUT),
        })
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    rx.recv_timeout(GPU_POLL_TIMEOUT)
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    {
        let mut view = slice.get_mapped_range_mut();
        view.copy_from_slice(data);
    }
    buffer.unmap();
    Ok(())
}

/// Waits until all previously submitted GPU work on `device` completes.
pub fn wait_for_gpu_idle(device: &wgpu::Device) -> Result<(), RenderError> {
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(GPU_POLL_TIMEOUT),
        })
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    Ok(())
}
