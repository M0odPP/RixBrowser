//! Headless GPU visual reftests and 60 FPS zero-rebuild animation verification
//! for GPU-backed `Canvas2DContext` over `vello::Scene` and `ExternalSurface`.

use std::path::PathBuf;
use std::sync::Mutex;

static GPU_LOCK: Mutex<()> = Mutex::new(());

use rix_core::{Rect, Size};
use rix_ipc::{PipelineId, SurfaceId};
use rix_render::{
    encode_rgba_to_png_data_url, parse_css_color, Canvas2DContext, CanvasFillRule, CanvasGradient,
    CanvasPaint, CanvasPattern, CanvasPatternRepeat, Color, Compositor, DisplayItem, DisplayList,
    LayerRole, VelloRenderer,
};
use rix_text::FontDatabase;

fn fixture_png_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/reftests/canvas2d")
        .join(name)
}

fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn canvas2d_visual_reftest_paths_arcs_beziers_transforms_gradients_text_clipping() {
    let w = 320u32;
    let h = 240u32;
    let sid = SurfaceId(101);
    let mut ctx = Canvas2DContext::new(sid, w, h);

    // 1. Linear gradient background.
    let mut lin = CanvasGradient::linear(0.0, 0.0, 320.0, 240.0);
    assert!(lin.add_color_stop(0.0, parse_css_color("#14213d").unwrap()));
    assert!(lin.add_color_stop(1.0, parse_css_color("#2a9d8f").unwrap()));
    ctx.set_fill_style(CanvasPaint::Gradient(lin));
    ctx.fill_rect(0.0, 0.0, 320.0, 240.0);

    // 2. Radial gradient orb.
    let mut rad = CanvasGradient::radial(70.0, 65.0, 5.0, 70.0, 65.0, 48.0);
    assert!(rad.add_color_stop(0.0, parse_css_color("#ffd166").unwrap()));
    assert!(rad.add_color_stop(1.0, parse_css_color("#ef476f").unwrap()));
    ctx.begin_path();
    ctx.arc(70.0, 65.0, 48.0, 0.0, std::f64::consts::TAU, false)
        .unwrap();
    ctx.set_fill_style(CanvasPaint::Gradient(rad));
    ctx.fill(CanvasFillRule::NonZero);

    // 3. Affine transforms (save -> translate -> rotate -> scale -> roundRect -> restore).
    ctx.save();
    ctx.translate(220.0, 65.0);
    ctx.rotate(0.35);
    ctx.scale(1.15, 0.9);
    ctx.begin_path();
    ctx.round_rect(-38.0, -24.0, 76.0, 48.0, [10.0, 10.0, 10.0, 10.0]);
    ctx.set_fill_css("#f4a261");
    ctx.fill(CanvasFillRule::NonZero);
    ctx.set_stroke_css("#ffffff");
    ctx.set_line_width(3.0);
    ctx.stroke();
    ctx.restore();

    // 4. Quadratic & cubic Bezier curves with dashed stroke.
    ctx.save();
    ctx.begin_path();
    ctx.move_to(20.0, 165.0);
    ctx.quadratic_curve_to(90.0, 115.0, 155.0, 165.0);
    ctx.bezier_curve_to(200.0, 210.0, 250.0, 120.0, 300.0, 165.0);
    ctx.set_stroke_css("#e63946");
    ctx.set_line_width(4.5);
    ctx.set_line_cap("round");
    ctx.set_line_dash(vec![10.0, 4.0]);
    ctx.stroke();
    ctx.restore();

    // 5. Clipping region inside a circle + restore popping clip cleanly.
    ctx.save();
    ctx.begin_path();
    ctx.arc(80.0, 190.0, 30.0, 0.0, std::f64::consts::TAU, false)
        .unwrap();
    ctx.clip(CanvasFillRule::NonZero);
    // Pattern fill inside clip.
    let pat_rgba = vec![
        255, 255, 255, 255, 40, 40, 200, 255, // row 0
        40, 40, 200, 255, 255, 255, 255, 255, // row 1
    ];
    let pat = CanvasPattern::from_rgba8(2, 2, pat_rgba, CanvasPatternRepeat::Repeat);
    ctx.set_fill_style(CanvasPaint::Pattern(pat));
    ctx.fill_rect(30.0, 140.0, 100.0, 100.0);
    ctx.restore();

    // Drawing outside the popped clip at (260, 195) must NOT be clipped!
    ctx.set_fill_css("#a8dadc");
    ctx.fill_rect(250.0, 185.0, 45.0, 35.0);

    // 6. Shaped text via rix-text (`measureText` + `fillText`).
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    ctx.set_font("bold 18px sans-serif");
    ctx.set_fill_css("#ffffff");
    let metrics = ctx.measure_text(&font_db, "Rix2D");
    assert!(metrics.width > 10.0, "shaped text width must be positive");
    ctx.fill_text(&font_db, "Rix2D", 140.0, 212.0, None);

    // Render into GPU-resident off-screen texture via headless Vello + readback.
    let _gpu = GPU_LOCK.lock().expect("gpu lock");
    let mut renderer = VelloRenderer::new();
    renderer
        .render_canvas_to_gpu_surface_blocking(&ctx)
        .expect("GPU canvas render");
    assert_eq!(renderer.gpu_surface_allocations(), 1);
    let (rw, rh, rgba) = renderer
        .readback_gpu_surface_rgba_blocking(sid)
        .expect("GPU canvas readback");
    assert_eq!((rw, rh), (w, h));
    assert_eq!(rgba.len(), (w * h * 4) as usize);

    // Spot-check key visual landmarks (radial orb center, popped-clip cyan box, unclipped bg).
    let orb_px = pixel_at(&rgba, w, 70, 65);
    assert!(
        orb_px[0] > 200 && orb_px[1] > 170,
        "expected golden radial center, got {orb_px:?}"
    );
    let cyan_box = pixel_at(&rgba, w, 270, 200);
    assert_eq!(
        cyan_box,
        [0xa8, 0xda, 0xdc, 0xff],
        "post-restore rect must not be clipped"
    );

    // Visual reftest comparison against committed reference PNG.
    let ref_path = fixture_png_path("canvas2d-showcase.png");
    if std::env::var_os("RIX_UPDATE_REFTESTS").is_some() || !ref_path.exists() {
        if let Some(parent) = ref_path.parent() {
            std::fs::create_dir_all(parent).expect("create reftest dir");
        }
        VelloRenderer::save_rgba_png(&ref_path, &rgba, w, h).expect("write reference PNG");
    }
    let expected_img = image::open(&ref_path)
        .expect("open committed Canvas2D reference PNG")
        .into_rgba8();
    assert_eq!(expected_img.dimensions(), (w, h));
    let expected = expected_img.into_raw();
    let mut diff_pixels = 0usize;
    for (a, b) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected.as_chunks::<4>().0)
    {
        let max_ch = (0..4).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
        if max_ch > 4 {
            diff_pixels += 1;
        }
    }
    assert_eq!(
        diff_pixels,
        0,
        "Canvas2D GPU render drifted by {diff_pixels} pixels from reference PNG {}",
        ref_path.display()
    );
}

#[test]
fn canvas2d_60fps_animation_produces_zero_display_list_rebuilds_and_zero_reencodes() {
    let canvas_w = 240u32;
    let canvas_h = 120u32;
    let sid = SurfaceId(777);
    let pipeline = PipelineId(1);

    // Build ONE DisplayList containing the ExternalSurface slot (never rebuilt during animation!).
    let mut list = DisplayList::new(Rect::new(0.0, 0.0, 320.0, 200.0));
    list.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 320.0, 200.0),
        color: rix_render::Rgba8::new(20, 20, 30, 255),
        radii: [0.0; 4],
    });
    list.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(40.0, 40.0, canvas_w as f32, canvas_h as f32),
        surface_id: sid,
        radii: [8.0; 4],
    });

    let mut compositor = Compositor::new(Size::new(320.0, 200.0), 1.0);
    compositor.submit_display_list(pipeline, LayerRole::WebSurface, 1, list);
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.layer(pipeline).unwrap().encode_generation(), 1);

    let mut canvas = Canvas2DContext::new(sid, canvas_w, canvas_h);
    let _gpu = GPU_LOCK.lock().expect("gpu lock");
    let mut renderer = VelloRenderer::new();
    let mut frame1_rgba = Vec::new();
    let mut frame60_rgba = Vec::new();

    // Run 60 consecutive animation frames: ZERO DisplayList rebuilds, ZERO layer re-encodes!
    for frame in 0..60u32 {
        canvas.clear_rect(0.0, 0.0, f64::from(canvas_w), f64::from(canvas_h));
        canvas.set_fill_css("#0b132b");
        canvas.fill_rect(0.0, 0.0, f64::from(canvas_w), f64::from(canvas_h));
        let cx = 20.0 + f64::from(frame) * 3.2;
        canvas.begin_path();
        canvas
            .arc(cx, 60.0, 14.0, 0.0, std::f64::consts::TAU, false)
            .unwrap();
        canvas.set_fill_css("#00f5d4");
        canvas.fill(CanvasFillRule::NonZero);

        // Render directly into persistent GPU off-screen texture & swap into compositor!
        renderer
            .render_canvas_to_gpu_surface_blocking(&canvas)
            .expect("GPU render frame");
        compositor.present_external_surface_scene(sid, canvas_w, canvas_h, canvas.snapshot_scene());
        let composed = compositor
            .compose()
            .expect("compositor marks dirty on frame swap");
        if frame == 0 {
            frame1_rgba = renderer
                .render_to_rgba_blocking(composed, 320, 200, Color::BLACK)
                .expect("read frame 1");
        } else if frame == 59 {
            frame60_rgba = renderer
                .render_to_rgba_blocking(composed, 320, 200, Color::BLACK)
                .expect("read frame 60");
        }
    }

    // Core Issue #18 invariants:
    assert_eq!(
        renderer.gpu_surface_allocations(),
        1,
        "GPU off-screen texture must be allocated once and reused across all 60 frames"
    );
    assert_eq!(
        compositor.layer(pipeline).unwrap().encode_generation(),
        1,
        "60 FPS Canvas2D animation must produce ZERO DisplayList rebuilds and ZERO layer re-encodes"
    );
    assert_eq!(compositor.frames_rendered(), 61);
    assert_ne!(
        frame1_rgba, frame60_rgba,
        "animated canvas pixels must advance across frames"
    );

    // Verify ball moved from left (40 + 20 = 60, 40 + 60 = 100) to right (40 + 208 = 248, 100).
    let left_f1 = pixel_at(&frame1_rgba, 320, 60, 100);
    let left_f60 = pixel_at(&frame60_rgba, 320, 60, 100);
    let right_f60 = pixel_at(&frame60_rgba, 320, 248, 100);
    assert!(
        left_f1[1] > 200,
        "ball should be at left in frame 1, got {left_f1:?}"
    );
    assert!(
        left_f60[1] < 40,
        "ball should have left start pos in frame 60, got {left_f60:?}"
    );
    assert!(
        right_f60[1] > 200,
        "ball should reach right in frame 60, got {right_f60:?}"
    );
}

#[test]
fn canvas2d_draw_canvas_put_image_data_hit_test_and_png_data_url() {
    let mut src = Canvas2DContext::new(SurfaceId(201), 32, 32);
    src.set_fill_css("#ff0000");
    src.fill_rect(0.0, 0.0, 32.0, 32.0);

    let mut dst = Canvas2DContext::new(SurfaceId(202), 64, 64);
    dst.draw_canvas(&src, 0.0, 0.0, 32.0, 32.0, 16.0, 16.0, 32.0, 32.0);
    // Put a 2x2 green patch at (2, 2).
    let patch = vec![
        0, 255, 0, 255, 0, 255, 0, 255, // row 0
        0, 255, 0, 255, 0, 255, 0, 255, // row 1
    ];
    dst.put_image_data(2, 2, patch, 2.0, 2.0);

    dst.begin_path();
    dst.rect(10.0, 10.0, 20.0, 20.0);
    assert!(dst.is_point_in_path(15.0, 15.0, CanvasFillRule::NonZero));
    assert!(!dst.is_point_in_path(5.0, 5.0, CanvasFillRule::NonZero));

    let _gpu = GPU_LOCK.lock().expect("gpu lock");
    let mut renderer = VelloRenderer::new();
    renderer
        .render_canvas_to_gpu_surface_blocking(&dst)
        .expect("render dst");
    let (rw, rh, rgba) = renderer
        .readback_gpu_surface_rgba_blocking(SurfaceId(202))
        .expect("readback dst");
    assert_eq!(pixel_at(&rgba, rw, 32, 32), [255, 0, 0, 255]);
    assert_eq!(pixel_at(&rgba, rw, 2, 2), [0, 255, 0, 255]);

    let data_url = encode_rgba_to_png_data_url(rw, rh, &rgba).expect("encode data url");
    assert!(data_url.starts_with("data:image/png;base64,"));
    assert!(data_url.len() > 80);
}
