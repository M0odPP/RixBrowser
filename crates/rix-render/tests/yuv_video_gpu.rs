//! Headless GPU verification of zero-copy planar YUV420p & NV12 transport via
//! `rix-ipc::TripleBufferShmem` -> `wgpu` BT.709 / BT.601 YUV->RGBA fragment shader ->
//! `VelloRenderer::register_external_gpu_texture` -> `Compositor::present_external_surface_image`
//! with **zero CPU YUV->RGBA conversion loops** and **zero `DisplayList` rebuilds**.

use std::sync::Mutex;

use rix_core::{Rect, Size};
use rix_ipc::{PipelineId, SurfaceId};
use rix_media::{create_video_frame_channel, ColorMatrix, ColorRange, PixelFormat, VideoFrame};
use rix_render::{
    Color, Compositor, DisplayItem, DisplayList, LayerRole, VelloRenderer, YuvVideoRenderer,
};

static GPU_LOCK: Mutex<()> = Mutex::new(());

/// Encodes a solid RGB color into limited-range BT.709 or BT.601 `(Y, Cb, Cr)` bytes.
fn rgb_to_limited_yuv(r: u8, g: u8, b: u8, matrix: ColorMatrix) -> (u8, u8, u8) {
    let rf = f64::from(r) / 255.0;
    let gf = f64::from(g) / 255.0;
    let bf = f64::from(b) / 255.0;
    let (kr, kb) = match matrix {
        ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Bt601 => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let y = kr * rf + kg * gf + kb * bf;
    let cb = 0.5 * (bf - y) / (1.0 - kb);
    let cr = 0.5 * (rf - y) / (1.0 - kr);
    let y8 = (16.0 + 219.0 * y).round().clamp(0.0, 255.0) as u8;
    let u8_val = (128.0 + 224.0 * cb).round().clamp(0.0, 255.0) as u8;
    let v8_val = (128.0 + 224.0 * cr).round().clamp(0.0, 255.0) as u8;
    (y8, u8_val, v8_val)
}

fn make_solid_yuv_frame(
    width: u32,
    height: u32,
    rgb: [u8; 3],
    format: PixelFormat,
    matrix: ColorMatrix,
    pts_us: i64,
) -> VideoFrame {
    let (y_val, u_val, v_val) = rgb_to_limited_yuv(rgb[0], rgb[1], rgb[2], matrix);
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let y_plane = vec![y_val; w * h];
    match format {
        PixelFormat::Yuv420p => VideoFrame {
            width,
            height,
            pixel_format: PixelFormat::Yuv420p,
            color_matrix: matrix,
            color_range: ColorRange::Limited,
            y_plane,
            u_plane: vec![u_val; cw * ch],
            v_plane: vec![v_val; cw * ch],
            y_stride: width,
            u_stride: cw as u32,
            v_stride: cw as u32,
            pts_us,
            duration_us: 16_667,
            is_keyframe: true,
        },
        PixelFormat::Nv12 => {
            let mut uv = Vec::with_capacity(cw * ch * 2);
            for _ in 0..(cw * ch) {
                uv.push(u_val);
                uv.push(v_val);
            }
            VideoFrame {
                width,
                height,
                pixel_format: PixelFormat::Nv12,
                color_matrix: matrix,
                color_range: ColorRange::Limited,
                y_plane,
                u_plane: uv,
                v_plane: Vec::new(),
                y_stride: width,
                u_stride: (cw * 2) as u32,
                v_stride: 0,
                pts_us,
                duration_us: 16_667,
                is_keyframe: true,
            }
        }
    }
}

#[test]
fn bt709_and_bt601_yuv420_and_nv12_shader_color_accuracy() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut renderer = VelloRenderer::new();
    let shared = renderer
        .ensure_shared_gpu_device_blocking()
        .expect("headless shared wgpu device");
    let mut yuv_renderer = YuvVideoRenderer::new(shared);

    let sid = SurfaceId(9001);
    let mut dl = DisplayList::default();
    dl.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
        surface_id: sid,
        radii: [0.0; 4],
    });
    let mut compositor = Compositor::new(Size::new(64.0, 64.0), 1.0);
    compositor.submit_display_list(PipelineId(1), LayerRole::WebSurface, 1, dl);

    let (mut prod, mut cons) = create_video_frame_channel(64, 64).expect("triple-buffer shmem");

    let cases = [
        (
            "BT.709 YUV420p red",
            230u8,
            40u8,
            50u8,
            PixelFormat::Yuv420p,
            ColorMatrix::Bt709,
        ),
        (
            "BT.601 YUV420p green",
            40u8,
            220u8,
            60u8,
            PixelFormat::Yuv420p,
            ColorMatrix::Bt601,
        ),
        (
            "BT.709 NV12 blue",
            35u8,
            90u8,
            235u8,
            PixelFormat::Nv12,
            ColorMatrix::Bt709,
        ),
        (
            "BT.601 NV12 yellow",
            235u8,
            210u8,
            40u8,
            PixelFormat::Nv12,
            ColorMatrix::Bt601,
        ),
    ];

    for (idx, (label, r, g, b, fmt, matrix)) in cases.into_iter().enumerate() {
        let frame = make_solid_yuv_frame(64, 64, [r, g, b], fmt, matrix, (idx as i64) * 16_667);
        prod.publish(&frame).expect("publish frame to shmem");
        let slot = cons.try_acquire_slot().expect("acquire frame slot");
        let presented = yuv_renderer
            .upload_from_shmem_slot(sid, slot, &mut renderer, &mut compositor)
            .expect("upload & convert YUV on GPU");
        assert!(presented);

        let scene = compositor.compose_force().clone();
        let rgba = renderer
            .render_to_rgba_blocking(&scene, 64, 64, Color::BLACK)
            .expect("composite GPU scene");
        let center = (32 * 64 + 32) * 4;
        let (pr, pg, pb, pa) = (
            rgba[center],
            rgba[center + 1],
            rgba[center + 2],
            rgba[center + 3],
        );
        assert_eq!(pa, 255, "{label}: expected opaque alpha");
        assert!(
            pr.abs_diff(r) <= 4 && pg.abs_diff(g) <= 4 && pb.abs_diff(b) <= 4,
            "{label}: expected ~({r},{g},{b}), got ({pr},{pg},{pb})"
        );
    }

    assert_eq!(yuv_renderer.frames_presented(sid), 4);
}

#[test]
fn video_1080p_60_frames_zero_display_list_rebuilds_via_triple_buffer_shmem() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut renderer = VelloRenderer::new();
    let shared = renderer
        .ensure_shared_gpu_device_blocking()
        .expect("headless shared wgpu device");
    let mut yuv_renderer = YuvVideoRenderer::new(shared);

    let sid = SurfaceId(9002);
    let mut dl = DisplayList::default();
    dl.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        surface_id: sid,
        radii: [0.0; 4],
    });
    let mut compositor = Compositor::new(Size::new(320.0, 180.0), 1.0);
    compositor.submit_display_list(PipelineId(2), LayerRole::WebSurface, 1, dl);

    // Pre-allocate 1080p triple-buffer shared memory channel.
    let (mut prod, mut cons) = create_video_frame_channel(1920, 1080).expect("1080p shmem channel");

    // Pre-build two distinct 1080p planar YUV420p frames (no per-frame CPU color loop!).
    let frame_a = make_solid_yuv_frame(
        1920,
        1080,
        [220, 50, 50],
        PixelFormat::Yuv420p,
        ColorMatrix::Bt709,
        0,
    );
    let frame_b = make_solid_yuv_frame(
        1920,
        1080,
        [40, 180, 220],
        PixelFormat::Yuv420p,
        ColorMatrix::Bt709,
        16_667,
    );

    let mut display_list_rebuilds: u64 = 0;
    for i in 0..60u64 {
        let src = if i % 2 == 0 { &frame_a } else { &frame_b };
        prod.publish(src).expect("publish 1080p frame");
        let slot = cons.try_acquire_slot().expect("acquire 1080p slot");
        let ok = yuv_renderer
            .upload_from_shmem_slot(sid, slot, &mut renderer, &mut compositor)
            .expect("GPU YUV->RGBA 1080p convert");
        assert!(ok);
        // DisplayList is NEVER resubmitted or rebuilt during video playback.
        let _ = compositor.compose();
        // Verify epoch stays at 1 (zero DisplayList rebuilds!).
        if compositor.layer(PipelineId(2)).unwrap().epoch != 1 {
            display_list_rebuilds += 1;
        }
    }

    assert_eq!(display_list_rebuilds, 0);
    assert_eq!(yuv_renderer.frames_presented(sid), 60);

    // Final composited frame (`i = 59`, odd -> `frame_b` cyan-blue) must render accurately.
    let scene = compositor.compose_force().clone();
    let rgba = renderer
        .render_to_rgba_blocking(&scene, 320, 180, Color::BLACK)
        .expect("render final composited 1080p video");
    let px = (90 * 320 + 160) * 4;
    assert!(
        rgba[px].abs_diff(40) <= 5
            && rgba[px + 1].abs_diff(180) <= 5
            && rgba[px + 2].abs_diff(220) <= 5,
        "expected final frame cyan-blue ~(40,180,220), got ({},{},{})",
        rgba[px],
        rgba[px + 1],
        rgba[px + 2]
    );
}
