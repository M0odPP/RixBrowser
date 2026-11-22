//! Headless GPU tests verifying zero-copy `VelloRenderer::register_external_gpu_texture`
//! and `Compositor::present_external_surface_image` across `Rgba8Unorm` and `Bgra8Unorm`
//! `wgpu::Texture` external surfaces with zero `DisplayList` rebuilds.

use std::sync::Mutex;

use rix_core::{Rect, Size};
use rix_ipc::{PipelineId, SurfaceId};
use rix_render::{wgpu, Color, Compositor, DisplayItem, DisplayList, LayerRole, VelloRenderer};

static GPU_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn zero_copy_external_wgpu_texture_composites_without_display_list_rebuild() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut renderer = VelloRenderer::new();
    let shared = renderer
        .ensure_shared_gpu_device_blocking()
        .expect("headless shared wgpu device");
    assert_ne!(shared.renderer_token, 0);
    assert_eq!(shared.renderer_token, renderer.renderer_token());

    let sid = SurfaceId(777);
    let mut dl = DisplayList::default();
    dl.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
        surface_id: sid,
        radii: [0.0; 4],
    });

    let mut compositor = Compositor::new(Size::new(64.0, 64.0), 1.0);
    let pipe = PipelineId(10);
    compositor.submit_display_list(pipe, LayerRole::WebSurface, 1, dl);

    // 1. Direct zero-copy Rgba8Unorm texture filled with solid orange (255, 128, 0, 255).
    let rgba_tex = shared.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-external-rgba"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let orange: Vec<u8> = [255u8, 128, 0, 255].repeat(64 * 64);
    shared.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &rgba_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &orange,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(64 * 4),
            rows_per_image: Some(64),
        },
        wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
    );
    let img1 = renderer
        .register_external_gpu_texture(sid, rgba_tex)
        .expect("register rgba8 external texture");
    compositor.present_external_surface_image(sid, img1);
    let scene1 = compositor.compose_force().clone();
    let gen_after_1 = compositor.layer(pipe).unwrap().encode_generation();
    let px1 = renderer
        .render_to_rgba_blocking(&scene1, 64, 64, Color::BLACK)
        .expect("render frame 1");
    let c1 = &px1[(32 * 64 + 32) * 4..(32 * 64 + 32) * 4 + 4];
    assert!(
        c1[0] > 240 && (115..=140).contains(&c1[1]) && c1[2] < 15 && c1[3] > 240,
        "expected orange pixel from direct Rgba8Unorm external texture, got {c1:?}"
    );

    // 2. Bgra8Unorm texture filled with solid cyan (B=255, G=255, R=0, A=255) -> GPU blit path!
    let bgra_tex = shared.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-external-bgra"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let bgra_cyan: Vec<u8> = [255u8, 255, 0, 255].repeat(64 * 64);
    shared.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &bgra_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bgra_cyan,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(64 * 4),
            rows_per_image: Some(64),
        },
        wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
    );
    let img2 = renderer
        .register_external_gpu_texture(sid, bgra_tex)
        .expect("register bgra8 external texture via GPU blit");
    compositor.present_external_surface_image(sid, img2);
    let scene2 = compositor.compose_force().clone();
    let gen_after_2 = compositor.layer(pipe).unwrap().encode_generation();
    assert_eq!(
        gen_after_1, gen_after_2,
        "presenting a new GPU external texture must NOT re-encode the page layer"
    );
    let px2 = renderer
        .render_to_rgba_blocking(&scene2, 64, 64, Color::BLACK)
        .expect("render frame 2");
    let c2 = &px2[(32 * 64 + 32) * 4..(32 * 64 + 32) * 4 + 4];
    assert!(
        c2[0] < 15 && c2[1] > 240 && c2[2] > 240 && c2[3] > 240,
        "expected cyan pixel from GPU-blitted Bgra8Unorm external texture, got {c2:?}"
    );
    assert!(renderer.unregister_external_gpu_texture(sid));
}
