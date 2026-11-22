//! End-to-end headless GPU visual reftests and 120 FPS compositor animation tests for
//! CSS `transform` (2D/3D + `transform-origin`), `filter` (`blur`, `brightness`,
//! `contrast`, `grayscale`, `opacity`, `drop-shadow`), `clip-path`, `mix-blend-mode`,
//! `linear-gradient`, `radial-gradient`, multi-layer `background-image: url(...)`,
//! `position: sticky` during APZ scroll, and compositor-driven `@keyframes`/`transition`.

use std::sync::{Arc, Mutex};

use rix_core::{Point, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::LayoutEngine;
use rix_render::{
    AnimationId, Color, Compositor, CompositorAnimation, LayerRole, TimingCurve, VelloRenderer,
};
use rix_text::FontDatabase;

static GPU_LOCK: Mutex<()> = Mutex::new(());

fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn render_doc(
    html: &str,
    css: &str,
    width: u32,
    height: u32,
    setup_assets: impl FnOnce(&mut LayoutEngine),
) -> (Vec<u8>, Compositor) {
    let _gpu = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = parse_html(html);
    let vp = Size::new(width as f32, height as f32);
    let media = MediaContext {
        viewport_width: vp.width,
        viewport_height: vp.height,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    setup_assets(&mut engine);
    let font_db = FontDatabase::new();
    let mut compositor = Compositor::new(vp, 1.0);
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        vp,
        &font_db,
        PipelineId(1),
        &mut compositor.resources,
    );
    compositor.submit_display_list(PipelineId(1), LayerRole::WebSurface, 1, frame.display_list);
    let scene = compositor.compose().expect("compose scene");
    let mut gpu = VelloRenderer::new();
    let rgba = gpu
        .render_to_rgba_blocking(scene, width, height, Color::WHITE)
        .expect("render rgba");
    (rgba, compositor)
}

#[test]
fn gpu_reftest_transforms_gradients_multilayer_bg_filters_clip_path_and_blend_mode() {
    // 1x1 solid bright green PNG for multi-layer `background-image: url(...)`.
    let mut png_bytes = Vec::new();
    {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 220, 40, 255]));
        let mut cur = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cur, image::ImageFormat::Png)
            .expect("encode png");
    }

    let html = r#"<!doctype html><html><body>
        <div id="tf"></div>
        <div id="rad"></div>
        <div id="bgmulti"></div>
        <div id="gray"></div>
        <div id="bright"></div>
        <div id="blurbox"></div>
        <div id="drop"></div>
        <div id="clipcircle"></div>
        <div id="clippoly"></div>
        <div id="blend_base"></div>
        <div id="blend_top"></div>
    </body></html>"#;

    let css = r#"
        body { margin: 0; background: #ffffff; }
        /* 1. Transform + custom transform-origin: 0 0 => scales from (10,10) to [10..90, 10..50] */
        #tf {
            position: absolute; left: 10px; top: 10px; width: 40px; height: 20px;
            background: #0000ff;
            transform-origin: 0 0;
            transform: scale(2, 2);
        }
        /* 2. Radial gradient: red center -> blue edge */
        #rad {
            position: absolute; left: 110px; top: 10px; width: 80px; height: 80px;
            background-image: radial-gradient(circle at 50% 50%, #ff0000, #0000ff);
        }
        /* 3. Multi-layer background-image: green PNG over linear-gradient */
        #bgmulti {
            position: absolute; left: 210px; top: 10px; width: 60px; height: 60px;
            background-image: url("green.png"), linear-gradient(90deg, #ff0000, #0000ff);
        }
        /* 4. Grayscale(100%) on pure red #ff0000 => neutral gray (r==g==b) */
        #gray {
            position: absolute; left: 10px; top: 110px; width: 50px; height: 50px;
            background: #ff0000;
            filter: grayscale(100%);
        }
        /* 5. Brightness(0.5) on pure white/red #ff8040 => darkened by 50% */
        #bright {
            position: absolute; left: 70px; top: 110px; width: 50px; height: 50px;
            background: #ff8040;
            filter: brightness(0.5);
        }
        /* 6. Blur(6px) softens edges outside the nominal 50x50 box onto white bg */
        #blurbox {
            position: absolute; left: 140px; top: 110px; width: 40px; height: 40px;
            background: #000000;
            filter: blur(6px);
        }
        /* 7. Drop-shadow casts a red shadow to the bottom-right at (+14, +14) */
        #drop {
            position: absolute; left: 210px; top: 110px; width: 30px; height: 30px;
            background: #0000ff;
            filter: drop-shadow(14px 14px 2px #ff0000);
        }
        /* 8. clip-path: circle(25px at 50% 50%): center filled, corners remain white */
        #clipcircle {
            position: absolute; left: 10px; top: 180px; width: 60px; height: 60px;
            background: #ff00ff;
            clip-path: circle(25px at 50% 50%);
        }
        /* 9. clip-path: polygon triangle (50% 0%, 100% 100%, 0% 100%) */
        #clippoly {
            position: absolute; left: 90px; top: 180px; width: 60px; height: 60px;
            background: #0080ff;
            clip-path: polygon(50% 0%, 100% 100%, 0% 100%);
        }
        /* 10. mix-blend-mode: multiply (cyan #00ffff over yellow #ffff00 => pure green #00ff00) */
        #blend_base {
            position: absolute; left: 180px; top: 180px; width: 60px; height: 60px;
            background: #ffff00;
        }
        #blend_top {
            position: absolute; left: 180px; top: 180px; width: 60px; height: 60px;
            background: #00ffff;
            mix-blend-mode: multiply;
        }
    "#;

    let (rgba, _) = render_doc(html, css, 300, 260, |engine| {
        let _ = engine.assets.register_encoded("green.png", &png_bytes);
    });

    // 1. Transform + transform-origin: (80, 40) is inside scaled 80x40 box [10..90, 10..50].
    let tf_px = pixel_at(&rgba, 300, 80, 40);
    assert!(
        tf_px[2] > 230 && tf_px[0] < 20,
        "transform-origin scale: {tf_px:?}"
    );

    // 2. Radial gradient: center (150, 50) is reddish, corner (112, 12) is bluish.
    let rad_c = pixel_at(&rgba, 300, 150, 50);
    let rad_e = pixel_at(&rgba, 300, 112, 12);
    assert!(
        rad_c[0] > 200 && rad_c[2] < 60,
        "radial center red: {rad_c:?}"
    );
    assert!(
        rad_e[2] > 180 && rad_e[0] < 80,
        "radial corner blue: {rad_e:?}"
    );

    // 3. Multi-layer bg: top PNG green [0, 220, 40] wins over bottom linear-gradient.
    let multi_px = pixel_at(&rgba, 300, 240, 40);
    assert!(
        multi_px[1] > 190 && multi_px[0] < 30,
        "multilayer bg png: {multi_px:?}"
    );

    // 4. Grayscale(100%) on #ff0000 => r == g == b (~54)
    let gray_px = pixel_at(&rgba, 300, 35, 135);
    assert!(
        (i16::from(gray_px[0]) - i16::from(gray_px[1])).abs() <= 2
            && (i16::from(gray_px[1]) - i16::from(gray_px[2])).abs() <= 2
            && gray_px[0] > 30
            && gray_px[0] < 90,
        "grayscale filter mismatch: {gray_px:?}"
    );

    // 5. Brightness(0.5) on #ff8040 => ~(128, 64, 32)
    let br_px = pixel_at(&rgba, 300, 95, 135);
    assert!(
        (i16::from(br_px[0]) - 128).abs() <= 4
            && (i16::from(br_px[1]) - 64).abs() <= 4
            && (i16::from(br_px[2]) - 32).abs() <= 4,
        "brightness filter mismatch: {br_px:?}"
    );

    // 6. Blur(6px): pixel 3px outside the nominal rect (137, 130) is softened (< 245, > 30).
    let blur_halo = pixel_at(&rgba, 300, 137, 130);
    assert!(
        blur_halo[0] < 245 && blur_halo[0] > 20,
        "blur halo outside box expected: {blur_halo:?}"
    );

    // 7. Drop-shadow: pixel at (247, 147) (outside 210..240, inside shifted shadow 224..254) is reddish!
    let ds_px = pixel_at(&rgba, 300, 247, 147);
    assert!(
        ds_px[0] > 200 && ds_px[2] < 160,
        "drop-shadow red pixel expected: {ds_px:?}"
    );

    // 8. clip-path circle: center (40, 210) is magenta, top-left corner (12, 182) stays white!
    let cc_in = pixel_at(&rgba, 300, 40, 210);
    let cc_out = pixel_at(&rgba, 300, 12, 182);
    assert!(
        cc_in[0] > 230 && cc_in[2] > 230 && cc_in[1] < 20,
        "circle inside: {cc_in:?}"
    );
    assert!(
        cc_out[0] > 245 && cc_out[1] > 245 && cc_out[2] > 245,
        "circle corner clipped: {cc_out:?}"
    );

    // 9. clip-path polygon triangle: bottom-center (120, 230) blue, top-left corner (95, 185) white!
    let poly_in = pixel_at(&rgba, 300, 120, 230);
    let poly_out = pixel_at(&rgba, 300, 95, 185);
    assert!(
        poly_in[2] > 230 && poly_in[0] < 20,
        "polygon inside: {poly_in:?}"
    );
    assert!(
        poly_out[0] > 245 && poly_out[1] > 245 && poly_out[2] > 245,
        "polygon corner clipped: {poly_out:?}"
    );

    // 10. mix-blend-mode: multiply (#00ffff * #ffff00 => #00ff00)
    let blend_px = pixel_at(&rgba, 300, 210, 210);
    assert!(
        blend_px[0] < 20 && blend_px[1] > 235 && blend_px[2] < 20,
        "mix-blend-mode multiply expected pure green: {blend_px:?}"
    );
}

#[test]
fn gpu_reftest_position_sticky_pins_and_releases_during_apz_scroll() {
    let html = r#"<!doctype html><html><body>
        <div id="spacer_top"></div>
        <div id="section">
            <div id="sticky_hdr"></div>
            <div id="section_body"></div>
        </div>
        <div id="spacer_bottom"></div>
    </body></html>"#;
    let css = r#"
        body { margin: 0; background: #ffffff; }
        #spacer_top { height: 80px; background: #dddddd; }
        #section { height: 240px; background: #eeeeee; }
        #sticky_hdr { position: sticky; top: 10px; height: 30px; background: #ef4444; }
        #section_body { height: 210px; background: #cccccc; }
        #spacer_bottom { height: 400px; background: #10b981; }
    "#;

    let (rgba0, mut compositor) = render_doc(html, css, 200, 200, |_| {});
    // Scroll = 0: sticky header is at its natural Y in [80..110], NOT at Y=15.
    let before_top = pixel_at(&rgba0, 200, 100, 15);
    let before_nat = pixel_at(&rgba0, 200, 100, 95);
    assert!(
        before_top[0] > 200 && before_top[1] > 200,
        "at scroll 0 top is spacer: {before_top:?}"
    );
    assert!(
        before_nat[0] > 220 && before_nat[1] < 90,
        "at scroll 0 sticky at natural Y=95: {before_nat:?}"
    );

    // Scroll down by 150px via APZ compositor (ZERO layout wakes!):
    // Section is now at [-70..170]; sticky header MUST be pinned at Y = [10..40]!
    assert!(compositor.handle_wheel(Point::new(100.0, 100.0), 0.0, 150.0));
    let rgba1 = {
        let _gpu = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let scene = compositor.compose().expect("scrolled sticky frame");
        let mut gpu = VelloRenderer::new();
        gpu.render_to_rgba_blocking(scene, 200, 200, Color::WHITE)
            .expect("render sticky mid-scroll")
    };
    let pinned = pixel_at(&rgba1, 200, 100, 25);
    assert!(
        pinned[0] > 220 && pinned[1] < 90,
        "at scroll=150 sticky header must be pinned at Y=25: {pinned:?}"
    );

    // Scroll further by 180px (total scroll=330px):
    // Section bottom was at 80+240=320px, so section has scrolled completely off top;
    // sticky header MUST have released with its containing block (green spacer_bottom visible at Y=25!).
    assert!(compositor.handle_wheel(Point::new(100.0, 100.0), 0.0, 180.0));
    let rgba2 = {
        let _gpu = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let scene = compositor.compose().expect("released sticky frame");
        let mut gpu = VelloRenderer::new();
        gpu.render_to_rgba_blocking(scene, 200, 200, Color::WHITE)
            .expect("render sticky released")
    };
    let released = pixel_at(&rgba2, 200, 100, 25);
    assert!(
        released[1] > 160 && released[0] < 40,
        "past containing block bottom, sticky header must release and reveal green spacer: {released:?}"
    );
}

#[test]
fn compositor_driven_keyframes_and_transition_run_120fps_with_zero_layout_wakes() {
    let html = r#"<!doctype html><html><body><div id="mover"></div></body></html>"#;
    let css = r#"
        @keyframes slideAndFade {
            from { transform: translateX(0px); opacity: 0.2; }
            to { transform: translateX(100px); opacity: 1; }
        }
        body { margin: 0; background: #ffffff; }
        #mover {
            position: absolute; left: 20px; top: 40px; width: 40px; height: 40px;
            background: #0000ff;
            animation: slideAndFade 1000ms linear 1 normal forwards;
        }
    "#;

    let (_, mut compositor) = render_doc(html, css, 200, 120, |_| {});
    let pipe = PipelineId(1);
    assert_eq!(
        compositor
            .layer(pipe)
            .unwrap()
            .display_list
            .animations
            .len(),
        2,
        "expected both Transform and Opacity CompositorAnimations extracted from @keyframes"
    );

    // Simulate 120 consecutive 120 Hz VSync frames (dt = 8.333 ms) purely inside Compositor:
    // ZERO calls to LayoutEngine, ZERO new DisplayLists submitted!
    let mut start_rgba = Vec::new();
    let mut mid_rgba = Vec::new();
    let mut end_rgba = Vec::new();
    let t_bench = std::time::Instant::now();
    for frame in 0..=120u32 {
        let now_ms = f64::from(frame) * (1000.0 / 120.0);
        let active = compositor.tick_animations(now_ms);
        if frame < 120 {
            assert_eq!(active, 2, "frame {frame} must have 2 active animations");
        } else {
            assert_eq!(
                active, 0,
                "finite animation must complete at t=1000ms and return compositor to 0 Hz idle"
            );
        }
        let _ = compositor.compose();
        if frame == 0 || frame == 60 || frame == 120 {
            let _gpu = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut gpu = VelloRenderer::new();
            let s = compositor.last_composed_scene();
            let px = gpu
                .render_to_rgba_blocking(s, 200, 120, Color::WHITE)
                .expect("render anim frame");
            if frame == 0 {
                start_rgba = px;
            } else if frame == 60 {
                mid_rgba = px;
            } else {
                end_rgba = px;
            }
        }
    }
    let elapsed_ms = t_bench.elapsed().as_secs_f64() * 1000.0;
    eprintln!("120 frames of compositor-driven @keyframes (CPU tick+encode + 3 GPU readbacks): {elapsed_ms:.1} ms");

    // Verify 0 Hz idle after completion!
    assert_eq!(compositor.tick_animations(1500.0), 0);
    assert!(
        compositor.compose().is_none(),
        "compositor must stay 0 Hz idle once finite animations complete"
    );

    // At t=0: box at x=[20..60] with opacity=0.2 over white => light blue (~204, 204, 255).
    let p0 = pixel_at(&start_rgba, 200, 40, 60);
    assert!(
        p0[0] > 180 && p0[0] < 225 && p0[2] > 240,
        "t=0 box should be faint blue (opacity 0.2): {p0:?}"
    );
    // At t=500ms: box moved +50px to [70..110] with opacity=0.6 over white => medium blue (~102, 102, 255).
    let p_mid = pixel_at(&mid_rgba, 200, 90, 60);
    assert!(
        p_mid[0] > 80 && p_mid[0] < 130 && p_mid[2] > 240,
        "t=500ms box should be at x=90 with opacity 0.6: {p_mid:?}"
    );
    // At t=1000ms: box moved +100px to [120..160] with opacity=1.0 => solid blue (0, 0, 255); original x=40 is white!
    let p_end = pixel_at(&end_rgba, 200, 140, 60);
    let p_old = pixel_at(&end_rgba, 200, 40, 60);
    assert!(
        p_end[0] < 15 && p_end[1] < 15 && p_end[2] > 240,
        "t=1000ms box should be solid blue at x=140: {p_end:?}"
    );
    assert!(
        p_old[0] > 245 && p_old[1] > 245 && p_old[2] > 245,
        "t=1000ms original position x=40 must be white: {p_old:?}"
    );

    // Also verify one-shot compositor CSS `transition` helper runs smoothly back to x=0!
    let spatial = compositor
        .layer(pipe)
        .unwrap()
        .display_list
        .spatial_tree
        .nodes
        .iter()
        .find(|n| matches!(n.kind, rix_render::SpatialNodeKind::Transform { .. }))
        .expect("transform spatial node")
        .id;
    let trans = CompositorAnimation::transform_transition(
        AnimationId(999),
        spatial,
        [1.0, 0.0, 0.0, 1.0, 100.0, 0.0],
        [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        200.0,
        0.0,
        TimingCurve::Linear,
        2000.0,
    );
    assert!(compositor.add_animation(pipe, trans));
    assert_eq!(compositor.tick_animations(2100.0), 1);
    let tr_rgba = {
        let _gpu = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let scene = compositor.compose().expect("transition midpoint");
        let mut gpu = VelloRenderer::new();
        gpu.render_to_rgba_blocking(scene, 200, 120, Color::WHITE)
            .expect("render transition")
    };
    let p_tr = pixel_at(&tr_rgba, 200, 90, 60);
    assert!(
        p_tr[2] > 240 && p_tr[0] < 20,
        "transition midpoint at x=90: {p_tr:?}"
    );
}
