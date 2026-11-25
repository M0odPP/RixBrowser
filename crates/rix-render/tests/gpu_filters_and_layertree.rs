use rix_core::{Point, Rect, Size};
use rix_ipc::PipelineId;
use rix_render::{
    smallvec, AnimationId, BlendMode, Color, ColorFilterMatrix, Compositor, CompositorAnimation,
    DisplayItem, DisplayList, FilterOp, LayerRole, Rgba8, SpatialNodeKind, TimingCurve,
    VelloRenderer,
};

fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn color_filter_matrix_matches_w3c_filter_effects_l1() {
    let red = Rgba8::new(255, 0, 0, 255);
    let gray = ColorFilterMatrix::grayscale(1.0).apply_rgba(red);
    // Rec.709 red luminance = 0.2126 * 255 ≈ 54
    assert!((i16::from(gray.r) - 54).abs() <= 1, "got {gray:?}");
    assert_eq!(gray.r, gray.g);
    assert_eq!(gray.g, gray.b);

    let inv = ColorFilterMatrix::invert(1.0).apply_rgba(Rgba8::new(40, 120, 200, 255));
    assert_eq!(inv, Rgba8::new(215, 135, 55, 255));

    let sep = ColorFilterMatrix::sepia(1.0).apply_rgba(Rgba8::new(255, 255, 255, 255));
    assert_eq!(sep, Rgba8::new(255, 255, 239, 255));

    let hue180 = ColorFilterMatrix::hue_rotate_deg(180.0).apply_rgba(red);
    assert!(
        hue180.r < 20 && hue180.g > 100 && hue180.b > 100,
        "180deg hue-rotate of red should produce cyan-ish tone, got {hue180:?}"
    );

    // Composition order matters: brightness(0.5) followed by invert(1.0) != invert then brightness.
    let ops_a = [FilterOp::Brightness(0.5), FilterOp::Invert(1.0)];
    let ops_b = [FilterOp::Invert(1.0), FilterOp::Brightness(0.5)];
    let ca = ColorFilterMatrix::from_filters(&ops_a, false).apply_rgba(Rgba8::WHITE);
    let cb = ColorFilterMatrix::from_filters(&ops_b, false).apply_rgba(Rgba8::WHITE);
    assert!((i16::from(ca.r) - 128).abs() <= 2, "got {ca:?}");
    assert_eq!(
        cb.r, 0,
        "invert(white)=black, brightness(0.5)=black, got {cb:?}"
    );
}

#[test]
fn headless_gpu_backdrop_filter_blurs_background_and_keeps_foreground_crisp() {
    let (w, h) = (160u32, 120u32);
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, w as f32, h as f32));
    // Crisp red left half, crisp blue right half (vertical seam at x = 80).
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 80.0, h as f32),
        color: Rgba8::new(255, 0, 0, 255),
        radii: [0.0; 4],
    });
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(80.0, 0.0, 80.0, h as f32),
        color: Rgba8::new(0, 0, 255, 255),
        radii: [0.0; 4],
    });
    // Rounded glass panel spanning across x = 80 with GPU `backdrop-filter: blur(8px) brightness(1.1)`.
    dl.push(DisplayItem::BackdropFilter {
        bounds: Rect::new(40.0, 20.0, 80.0, 80.0),
        radii: [12.0; 4],
        filters: smallvec![FilterOp::Blur(8.0), FilterOp::Brightness(1.1)],
    });
    // Translucent glass tint painted ON TOP of the blurred backdrop.
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(40.0, 20.0, 80.0, 80.0),
        color: Rgba8::new(255, 255, 255, 40),
        radii: [12.0; 4],
    });
    // Crisp opaque green badge painted ON TOP of the glass panel (must NOT be blurred).
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(72.0, 52.0, 16.0, 16.0),
        color: Rgba8::new(0, 220, 0, 255),
        radii: [0.0; 4],
    });

    let mut compositor = Compositor::new(Size::new(w as f32, h as f32), 1.0);
    compositor.submit_display_list(PipelineId(1), LayerRole::WebSurface, 1, dl);

    let mut renderer = VelloRenderer::new();
    let pixels = renderer
        .render_compositor_to_rgba_blocking(&mut compositor, w, h, Color::BLACK)
        .expect("GPU backdrop-filter render should succeed");
    assert!(
        compositor.backdrop_plan().is_some(),
        "backdrop plan must be active"
    );

    // 1. Outside the glass card (y = 5): seam at x = 78 / x = 82 stays razor sharp red/blue.
    let outside_left = pixel_at(&pixels, w, 78, 5);
    let outside_right = pixel_at(&pixels, w, 82, 5);
    assert!(
        outside_left[0] > 240 && outside_left[2] < 10,
        "left={outside_left:?}"
    );
    assert!(
        outside_right[2] > 240 && outside_right[0] < 10,
        "right={outside_right:?}"
    );

    // 2. Inside the glass card right near the seam (x = 80, y = 30): Gaussian blur blends red + blue!
    let blurred_seam = pixel_at(&pixels, w, 80, 30);
    assert!(
        blurred_seam[0] > 80 && blurred_seam[2] > 80,
        "GPU Gaussian blur must blend red & blue across x=80 inside glass card, got {blurred_seam:?}"
    );

    // 3. Foreground green badge at (80, 60) painted AFTER backdrop-filter stays crisp green!
    let badge = pixel_at(&pixels, w, 80, 60);
    assert!(
        badge[1] > 200 && badge[0] < 20 && badge[2] < 20,
        "foreground content on top of backdrop-filter must stay crisp, got {badge:?}"
    );
}

#[test]
fn retained_layer_tree_runs_120_fps_animations_with_zero_reencodes_and_idle_0hz() {
    let (w, h) = (320u32, 160u32);
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, w as f32, h as f32));

    // Static background content (encoded ONCE in a static segment).
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, w as f32, h as f32),
        color: Rgba8::new(20, 24, 34, 255),
        radii: [0.0; 4],
    });

    // Animated promoted sub-layer moving from x=10 to x=210 over 1000ms (120 frames at 8.33ms).
    let anim_id = AnimationId(77);
    let spatial = dl.spatial_tree.add_node(
        Some(rix_ipc::SpatialNodeId(0)),
        SpatialNodeKind::Transform {
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            origin: Point::new(0.0, 0.0),
            animation_id: Some(anim_id),
        },
    );
    let opacity_id = AnimationId(78);
    dl.push(DisplayItem::PushStackingContext {
        spatial,
        bounds: Rect::new(10.0, 40.0, 40.0, 40.0),
        opacity: 1.0,
        mix_blend: BlendMode::Normal,
        filters: smallvec![FilterOp::AnimatedOpacity(opacity_id)],
    });
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(10.0, 40.0, 40.0, 40.0),
        color: Rgba8::new(255, 80, 40, 255),
        radii: [6.0; 4],
    });
    dl.push(DisplayItem::PopStackingContext);

    dl.animations
        .push(CompositorAnimation::transform_transition(
            anim_id,
            spatial,
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 1.0, 200.0, 0.0],
            1000.0,
            0.0,
            TimingCurve::Linear,
            0.0,
        ));
    dl.animations.push(CompositorAnimation::opacity_transition(
        opacity_id,
        0.4,
        1.0,
        1000.0,
        0.0,
        TimingCurve::Linear,
        0.0,
    ));

    let pid = PipelineId(42);
    let mut compositor = Compositor::new(Size::new(w as f32, h as f32), 1.0);
    compositor.submit_display_list(pid, LayerRole::WebSurface, 1, dl);
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.layer(pid).unwrap().encode_generation(), 1);
    let stats = compositor
        .layer_tree_stats(pid)
        .expect("animated layer should be promoted into RetainedLayerTree");
    assert_eq!(stats.promoted_layers, 1);

    let mut renderer = VelloRenderer::new();
    let frame0 = renderer
        .render_to_rgba_blocking(compositor.last_composed_scene(), w, h, Color::BLACK)
        .expect("frame 0 GPU render");

    // Step 120 frames (8.333ms per frame -> 120 Hz) and verify ZERO full-layer re-encodes
    // AND bounded partial damage rects smaller than full viewport!
    let mut mid_frame_pixels = None;
    for frame_idx in 1..=120u32 {
        let now_ms = f64::from(frame_idx) * (1000.0 / 120.0);
        let active = compositor.tick_animations(now_ms);
        if frame_idx < 120 {
            assert_eq!(
                active, 2,
                "both transform + opacity tracks active at frame {frame_idx}"
            );
        }
        assert!(
            compositor.compose().is_some(),
            "frame {frame_idx} must compose"
        );
        assert_eq!(
            compositor.layer(pid).unwrap().encode_generation(),
            1,
            "LayerTree MUST NOT re-encode full layer during 120 FPS animation (frame {frame_idx})"
        );
        let dmg = compositor
            .last_frame_damage()
            .expect("animated frame must report damage");
        assert!(
            dmg.size.width < w as f32 && dmg.size.height < h as f32,
            "damage rect must be bounded tightly around moving layer, got {dmg:?}"
        );
        if frame_idx == 60 {
            mid_frame_pixels = Some(
                renderer
                    .render_to_rgba_blocking(compositor.last_composed_scene(), w, h, Color::BLACK)
                    .expect("frame 60 GPU render"),
            );
        }
    }

    let frame120 = renderer
        .render_to_rgba_blocking(compositor.last_composed_scene(), w, h, Color::BLACK)
        .expect("frame 120 GPU render");

    // Frame 0: orange box at x=30, y=60; Frame 60: orange box at x=130, y=60; Frame 120: x=230, y=60.
    let p0 = pixel_at(&frame0, w, 30, 60);
    let p60 = pixel_at(mid_frame_pixels.as_ref().unwrap(), w, 130, 60);
    let p120 = pixel_at(&frame120, w, 230, 60);
    assert!(p0[0] > 100, "frame 0 should show box at x=30, got {p0:?}");
    assert!(
        p60[0] > 150,
        "frame 60 should show box at x=130, got {p60:?}"
    );
    assert!(
        p120[0] > 230,
        "frame 120 should show opaque box at x=230, got {p120:?}"
    );

    // Past duration: finite animations settle -> 0 active, 0 dirty, compose() == None -> IDLE 0 Hz!
    assert_eq!(compositor.tick_animations(1500.0), 0);
    assert!(!compositor.is_dirty());
    assert!(compositor.compose().is_none());
    assert!(compositor.last_frame_damage().is_none());
}
