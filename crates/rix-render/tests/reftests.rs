use rix_core::{Point, Rect, Size, Vec2};
use rix_ipc::{
    FontResourceId, ImageId, PathResourceId, PipelineId, ScrollId, SpatialNodeId, SurfaceId,
};
use rix_render::display_list::{
    AnimationId, BlendMode, ColorStop, DisplayItem, DisplayList, FilterOp, GlyphInstance,
    ImageSampling, Rgba8, SpatialNodeKind, StickyMargins,
};
use rix_render::{
    encode_display_list, Compositor, EncodeOptions, LayerRole, ResourceRegistry, VelloRenderer,
};
use rix_text::{FontDatabase, FontDescriptor, TextShaper};
use smallvec::smallvec;
use std::sync::Mutex;
use vello::peniko::Color;
use vello::Scene;

const BG_COLOR: Color = Color::from_rgba8(12, 16, 24, 255);

fn render_rgba(scene: &Scene, width: u32, height: u32, bg: Color) -> Vec<u8> {
    // Serialize headless GPU initialization and readback within the test binary to avoid
    // concurrent Mesa/Vulkan driver teardown races across test threads.
    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut renderer = VelloRenderer::new();
    renderer
        .render_to_rgba_blocking(scene, width, height, bg)
        .expect("headless GPU render succeeds")
}

#[inline]
fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [
        pixels[idx],
        pixels[idx + 1],
        pixels[idx + 2],
        pixels[idx + 3],
    ]
}

#[test]
fn headless_gpu_shapes_gradient_border_shadow_and_svg() {
    let pipeline = PipelineId(1);
    let mut resources = ResourceRegistry::default();
    let path_id = PathResourceId(42);
    resources
        .register_svg_path(path_id, "M 0 0 L 40 0 L 40 40 L 0 40 Z")
        .expect("svg path parses");

    let mut list = DisplayList::new(Rect::new(0.0, 0.0, 320.0, 200.0));
    // Solid rounded rectangle
    list.push(DisplayItem::Rect {
        bounds: Rect::new(20.0, 20.0, 80.0, 60.0),
        color: Rgba8::new(220, 40, 40, 255),
        radii: [12.0; 4],
    });
    // Outset blurred box shadow + linear gradient box
    let grad_box = Rect::new(130.0, 24.0, 120.0, 60.0);
    list.push(DisplayItem::BoxShadow {
        bounds: grad_box,
        offset: Vec2::new(0.0, 12.0),
        blur_radius: 16.0,
        spread_radius: 2.0,
        color: Rgba8::new(0, 180, 255, 220),
        radii: [8.0; 4],
        inset: false,
    });
    list.push(DisplayItem::LinearGradient {
        bounds: grad_box,
        start: Point::new(grad_box.min_x(), grad_box.min_y()),
        end: Point::new(grad_box.max_x(), grad_box.min_y()),
        stops: smallvec![
            ColorStop {
                offset: 0.0,
                color: Rgba8::new(20, 220, 40, 255),
            },
            ColorStop {
                offset: 1.0,
                color: Rgba8::new(20, 40, 230, 255),
            },
        ],
        radii: [8.0; 4],
    });
    // Asymmetric border around lower-left box
    list.push(DisplayItem::Border {
        bounds: Rect::new(20.0, 110.0, 80.0, 60.0),
        widths: [6.0, 6.0, 6.0, 6.0],
        colors: [Rgba8::new(250, 210, 40, 255); 4],
        radii: [10.0; 4],
    });
    // SVG VectorPath in lower-right
    list.push(DisplayItem::VectorPath {
        bounds: Rect::new(150.0, 120.0, 40.0, 40.0),
        path_id,
        fill: Some(Rgba8::new(210, 80, 220, 255)),
        stroke: None,
        fill_rule_even_odd: false,
    });

    let mut scene = Scene::new();
    let stats = encode_display_list(
        pipeline,
        &list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    assert_eq!(stats.items_encoded, 5);

    let pixels = render_rgba(&scene, 320, 200, BG_COLOR);

    // Background untouched in corner (2, 2)
    assert_eq!(pixel_at(&pixels, 320, 2, 2), [12, 16, 24, 255]);

    // Red rounded rect interior (60, 50)
    let red_px = pixel_at(&pixels, 320, 60, 50);
    assert!(
        red_px[0] > 205 && red_px[1] < 55 && red_px[2] < 55,
        "{red_px:?}"
    );

    // Linear gradient left vs right edge
    let grad_left = pixel_at(&pixels, 320, 140, 54);
    let grad_right = pixel_at(&pixels, 320, 240, 54);
    assert!(grad_left[1] > grad_left[2], "left is green: {grad_left:?}");
    assert!(
        grad_right[2] > grad_right[1],
        "right is blue: {grad_right:?}"
    );

    // Blue-ish box shadow glow slightly below grad_box (y = 96)
    let shadow_px = pixel_at(&pixels, 320, 190, 96);
    assert!(shadow_px[2] > 40, "shadow glow visible: {shadow_px:?}");

    // Yellow border stroke at top edge (60, 113) while center (60, 140) remains background
    let border_edge = pixel_at(&pixels, 320, 60, 113);
    let border_center = pixel_at(&pixels, 320, 60, 140);
    assert!(
        border_edge[0] > 230 && border_edge[1] > 190,
        "{border_edge:?}"
    );
    assert_eq!(border_center, [12, 16, 24, 255]);

    // Magenta SVG VectorPath interior (170, 140)
    let svg_px = pixel_at(&pixels, 320, 170, 140);
    assert!(svg_px[0] > 195 && svg_px[2] > 205, "{svg_px:?}");
}

#[test]
fn headless_gpu_nested_clip_and_finite_opacity_stacking_context() {
    let mut resources = ResourceRegistry::default();
    let mut list = DisplayList::new(Rect::new(0.0, 0.0, 160.0, 160.0));

    // Clip to [20..100, 20..100] and draw full-screen green rect: only inside clip may paint.
    list.push(DisplayItem::PushClip {
        spatial: SpatialNodeId(0),
        rect: Rect::new(20.0, 20.0, 80.0, 80.0),
        radii: [0.0; 4],
    });
    list.push(DisplayItem::PushStackingContext {
        spatial: SpatialNodeId(0),
        bounds: Rect::new(20.0, 20.0, 80.0, 80.0),
        opacity: 0.5,
        mix_blend: BlendMode::Normal,
        filters: smallvec![],
    });
    list.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 160.0, 160.0),
        color: Rgba8::new(255, 255, 255, 255),
        radii: [0.0; 4],
    });
    list.push(DisplayItem::PopStackingContext);
    list.push(DisplayItem::PopClip);

    let mut scene = Scene::new();
    encode_display_list(
        PipelineId(1),
        &list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );

    let pixels = render_rgba(&scene, 160, 160, Color::from_rgba8(0, 0, 0, 255));

    // Outside clip stays exact black
    assert_eq!(pixel_at(&pixels, 160, 10, 10), [0, 0, 0, 255]);
    assert_eq!(pixel_at(&pixels, 160, 130, 130), [0, 0, 0, 255]);

    // Inside clip blended at 0.5 opacity over black -> roughly mid-grey (~127 or ~187 depending on sRGB/linear compositing)
    let inside = pixel_at(&pixels, 160, 60, 60);
    assert!(
        inside[0] > 90 && inside[0] < 210 && inside[0] == inside[1] && inside[1] == inside[2],
        "expected blended grey inside opacity stacking context, got {inside:?}"
    );
}

#[test]
fn headless_gpu_glyph_run_rendering() {
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let Some(font_id) = font_db
        .match_font(&FontDescriptor::new("sans-serif"))
        .or(font_db.default_sans_serif)
    else {
        // Environment has no installed system fonts; nothing to shape.
        return;
    };
    let font = font_db.get_font(font_id).expect("resolved font exists");
    let shaper = TextShaper::new(&font_db);
    let runs = shaper.shape_text("RIX", font_id, 42.0);
    assert!(!runs.is_empty());

    let mut resources = ResourceRegistry::default();
    let res_font_id = resources.register_rix_font(font);

    let mut pen_x = 0.0f32;
    let mut glyphs = Vec::new();
    for run in runs {
        for g in &run.glyphs {
            glyphs.push(GlyphInstance {
                glyph_id: g.glyph_id,
                x: 24.0 + pen_x + g.x_offset,
                y: 64.0 - g.y_offset,
            });
            pen_x += g.x_advance;
        }
    }

    let mut list = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 100.0));
    list.push_glyph_run(
        Rect::new(20.0, 16.0, pen_x + 12.0, 64.0),
        res_font_id,
        42.0,
        Rgba8::new(240, 245, 255, 255),
        &glyphs,
    );

    let mut scene = Scene::new();
    let stats = encode_display_list(
        PipelineId(1),
        &list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    assert_eq!(stats.items_encoded, 1);

    let pixels = render_rgba(&scene, 200, 100, BG_COLOR);

    let mut lit_pixels = 0usize;
    for y in 20..75 {
        for x in 20..180 {
            let p = pixel_at(&pixels, 200, x, y);
            if p[0] > 120 && p[1] > 120 && p[2] > 120 {
                lit_pixels += 1;
            }
        }
    }
    assert!(
        lit_pixels > 40,
        "expected crisp rasterized glyph ink pixels, observed only {lit_pixels}"
    );
}

#[test]
fn retained_layer_cache_isolates_updates_and_idle_stays_zero_hz() {
    let mut compositor = Compositor::new(Size::new(800.0, 600.0), 1.0);
    let web_id = PipelineId(1);
    let chrome_id = PipelineId(2);
    let overlay_id = PipelineId(3);

    let make_rect_list = |color| {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        dl.push(DisplayItem::Rect {
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            color,
            radii: [0.0; 4],
        });
        dl
    };

    compositor.submit_display_list(
        web_id,
        LayerRole::WebSurface,
        1,
        make_rect_list(Rgba8::WHITE),
    );
    compositor.submit_display_list(
        chrome_id,
        LayerRole::Chrome,
        1,
        make_rect_list(Rgba8::BLACK),
    );
    compositor.submit_display_list(
        overlay_id,
        LayerRole::Overlay,
        1,
        make_rect_list(Rgba8::new(0, 200, 0, 255)),
    );

    // First compose encodes all 3 layers.
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.frames_rendered(), 1);
    assert_eq!(compositor.layer(web_id).unwrap().encode_generation(), 1);
    assert_eq!(compositor.layer(chrome_id).unwrap().encode_generation(), 1);
    assert_eq!(compositor.layer(overlay_id).unwrap().encode_generation(), 1);

    // Subsequent idle checks produce 0 Hz redraws.
    assert!(!compositor.is_dirty());
    assert!(compositor.compose().is_none());
    assert!(compositor.compose().is_none());
    assert_eq!(compositor.frames_rendered(), 1);

    // Updating Chrome ONLY re-encodes Chrome.
    compositor.submit_display_list(
        chrome_id,
        LayerRole::Chrome,
        2,
        make_rect_list(Rgba8::new(30, 40, 50, 255)),
    );
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.frames_rendered(), 2);
    assert_eq!(compositor.layer(web_id).unwrap().encode_generation(), 1);
    assert_eq!(compositor.layer(chrome_id).unwrap().encode_generation(), 2);
    assert_eq!(compositor.layer(overlay_id).unwrap().encode_generation(), 1);

    // Updating WebSurface ONLY re-encodes WebSurface.
    compositor.submit_display_list(
        web_id,
        LayerRole::WebSurface,
        2,
        make_rect_list(Rgba8::new(200, 210, 220, 255)),
    );
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.frames_rendered(), 3);
    assert_eq!(compositor.layer(web_id).unwrap().encode_generation(), 2);
    assert_eq!(compositor.layer(chrome_id).unwrap().encode_generation(), 2);
    assert_eq!(compositor.layer(overlay_id).unwrap().encode_generation(), 1);
}

#[test]
fn display_list_sealed_shmem_roundtrip() {
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 1280.0, 2400.0));
    let scroll_id = dl.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::ScrollFrame {
            content_rect: Rect::new(0.0, 0.0, 1280.0, 2400.0),
            viewport_rect: Rect::new(0.0, 0.0, 1280.0, 720.0),
            external_scroll_id: ScrollId(7),
            current_offset: Vec2::new(0.0, 120.0),
        },
    );
    let _sticky_id = dl.spatial_tree.add_node(
        Some(scroll_id),
        SpatialNodeKind::StickyFrame {
            bounds: Rect::new(0.0, 200.0, 0.0, 500.0),
            margins: StickyMargins {
                top: Some(16.0),
                ..StickyMargins::default()
            },
            scroll_parent: scroll_id,
        },
    );
    let _tx_id = dl.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::Transform {
            matrix: [1.0, 0.0, 0.0, 1.0, 10.0, 20.0],
            origin: Point::new(50.0, 50.0),
            animation_id: Some(AnimationId(99)),
        },
    );

    dl.push(DisplayItem::PushStackingContext {
        spatial: scroll_id,
        bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
        opacity: 0.75,
        mix_blend: BlendMode::Multiply,
        filters: smallvec![FilterOp::Opacity(0.9)],
    });
    dl.push(DisplayItem::PushClip {
        spatial: scroll_id,
        rect: Rect::new(10.0, 10.0, 380.0, 280.0),
        radii: [8.0; 4],
    });
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(20.0, 20.0, 100.0, 80.0),
        color: Rgba8::new(10, 20, 30, 255),
        radii: [4.0; 4],
    });
    dl.push(DisplayItem::Image {
        bounds: Rect::new(140.0, 20.0, 64.0, 64.0),
        image_id: ImageId(3),
        sampling: ImageSampling::High,
        radii: [6.0; 4],
    });
    dl.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(220.0, 20.0, 120.0, 90.0),
        surface_id: SurfaceId(5),
        radii: [0.0; 4],
    });
    dl.push_glyph_run(
        Rect::new(20.0, 120.0, 180.0, 24.0),
        FontResourceId(12345),
        16.0,
        Rgba8::WHITE,
        &[GlyphInstance {
            glyph_id: 65,
            x: 20.0,
            y: 136.0,
        }],
    );
    dl.push(DisplayItem::PopClip);
    dl.push(DisplayItem::PopStackingContext);

    let blob = dl.to_sealed_shmem().expect("seal display list to memfd");
    let decoded = DisplayList::from_sealed_shmem(&blob).expect("decode display list from memfd");
    assert_eq!(decoded, dl);
}

#[test]
fn resource_registry_image_lru_eviction_and_budget() {
    let mut reg = ResourceRegistry::new(64);
    let p = PipelineId(1);
    let img = vec![255u8; 32]; // 2x4 RGBA8 = 32 bytes

    reg.register_rgba8(p, ImageId(1), 2, 4, img.clone())
        .unwrap();
    reg.register_rgba8(p, ImageId(2), 2, 4, img.clone())
        .unwrap();
    assert_eq!(reg.image_used_bytes(), 64);

    // Touch ImageId(1) so ImageId(2) becomes least recently used.
    assert!(reg.get_image(p, ImageId(1)).is_some());

    // Inserting ImageId(3) must evict ImageId(2) and stay within 64-byte budget.
    reg.register_rgba8(p, ImageId(3), 2, 4, img).unwrap();
    assert_eq!(reg.image_used_bytes(), 64);
    assert!(reg.get_image(p, ImageId(1)).is_some());
    assert!(reg.get_image(p, ImageId(2)).is_none());
    assert!(reg.get_image(p, ImageId(3)).is_some());
}

#[test]
fn disjoint_nested_clip_subtree_is_culled() {
    let mut resources = ResourceRegistry::default();
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 400.0, 400.0));
    dl.push(DisplayItem::PushClip {
        spatial: SpatialNodeId(0),
        rect: Rect::new(0.0, 0.0, 100.0, 100.0),
        radii: [0.0; 4],
    });
    // Child clip completely outside parent clip [0..100, 0..100].
    dl.push(DisplayItem::PushClip {
        spatial: SpatialNodeId(0),
        rect: Rect::new(200.0, 200.0, 80.0, 80.0),
        radii: [0.0; 4],
    });
    dl.push(DisplayItem::Rect {
        bounds: Rect::new(210.0, 210.0, 40.0, 40.0),
        color: Rgba8::WHITE,
        radii: [0.0; 4],
    });
    dl.push(DisplayItem::PopClip);
    dl.push(DisplayItem::PopClip);

    let mut scene = Scene::new();
    let stats = encode_display_list(
        PipelineId(1),
        &dl,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    assert!(stats.subtrees_culled >= 1);
    assert_eq!(stats.items_encoded, 2); // Only outer PushClip + PopClip.
}

#[test]
fn oopif_subframe_compositing_and_inline_crash_fallback() {
    let parent_pipe = PipelineId(10);
    let child_pipe = PipelineId(20);
    let iframe_surface = SurfaceId(777);

    // Parent page: solid blue backdrop with an embedded 120x80 OOPIF at (40, 30).
    let mut parent_dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 140.0));
    parent_dl.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 200.0, 140.0),
        color: Rgba8::new(20, 60, 200, 255),
        radii: [0.0; 4],
    });
    parent_dl.push(DisplayItem::ExternalSurface {
        bounds: Rect::new(40.0, 30.0, 120.0, 80.0),
        surface_id: iframe_surface,
        radii: [0.0; 4],
    });

    // Child OOPIF: bright emerald green document that deliberately overflows beyond 120x80
    // to prove the compositor clips the child strictly to the parent's ExternalSurface bounds.
    let mut child_dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
    child_dl.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
        color: Rgba8::new(16, 200, 90, 255),
        radii: [0.0; 4],
    });

    let mut compositor = Compositor::new(Size::new(200.0, 140.0), 1.0);
    compositor.submit_display_list(parent_pipe, LayerRole::WebSurface, 1, parent_dl);
    compositor.submit_display_list(child_pipe, LayerRole::Subframe, 1, child_dl);
    compositor.bind_external_surface_pipeline(iframe_surface, child_pipe, 120, 80);

    let scene1 = compositor.compose().expect("initial OOPIF frame");
    let rgba1 = render_rgba(scene1, 200, 140, BG_COLOR);
    assert_eq!(
        compositor.layer(parent_pipe).unwrap().encode_generation(),
        1
    );
    assert_eq!(compositor.layer(child_pipe).unwrap().encode_generation(), 1);

    // Outside iframe remains parent blue; inside iframe is child green.
    let outside = pixel_at(&rgba1, 200, 10, 10);
    let inside = pixel_at(&rgba1, 200, 100, 70);
    let past_right = pixel_at(&rgba1, 200, 180, 70);
    assert!(
        outside[2] > 180 && outside[1] < 90,
        "parent bg: {outside:?}"
    );
    assert!(
        inside[1] > 180 && inside[0] < 40,
        "child iframe: {inside:?}"
    );
    assert!(
        past_right[2] > 180 && past_right[1] < 90,
        "child must be clipped strictly to iframe bounds: {past_right:?}"
    );

    // Repainting ONLY the child OOPIF must NOT re-encode the parent layer.
    let mut child_dl2 = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 80.0));
    child_dl2.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 120.0, 80.0),
        color: Rgba8::new(240, 210, 30, 255),
        radii: [0.0; 4],
    });
    compositor.submit_display_list(child_pipe, LayerRole::Subframe, 2, child_dl2);
    let scene2 = compositor.compose().expect("child repaint frame");
    let rgba2 = render_rgba(scene2, 200, 140, BG_COLOR);
    assert_eq!(
        compositor.layer(parent_pipe).unwrap().encode_generation(),
        1,
        "parent must NOT re-encode when only child OOPIF repaints"
    );
    let inside2 = pixel_at(&rgba2, 200, 100, 70);
    assert!(
        inside2[0] > 210 && inside2[1] > 180 && inside2[2] < 60,
        "updated child yellow pixel: {inside2:?}"
    );

    // Crash the child OOPIF pipeline: parent stays untouched, iframe shows inline crashed UI.
    let crashed = compositor.mark_pipeline_crashed(child_pipe, "Child process crashed");
    assert_eq!(crashed, vec![iframe_surface]);
    assert!(compositor.external_surface_is_crashed(iframe_surface));
    assert!(compositor.layer(child_pipe).is_none());

    let scene3 = compositor.compose().expect("crashed fallback frame");
    let rgba3 = render_rgba(scene3, 200, 140, BG_COLOR);
    let outside3 = pixel_at(&rgba3, 200, 10, 10);
    let crash_bg = pixel_at(&rgba3, 200, 50, 40);
    let crash_badge = pixel_at(&rgba3, 200, 100, 70);
    assert!(
        outside3[2] > 180 && outside3[1] < 90,
        "parent must remain intact after child crash: {outside3:?}"
    );
    assert!(
        crash_bg[0] < 50 && crash_bg[1] < 55 && crash_bg[2] < 70,
        "inline crashed frame slate backdrop expected: {crash_bg:?}"
    );
    assert!(
        crash_badge[0] > 180,
        "inline crashed frame alert badge expected at center: {crash_badge:?}"
    );
}

#[test]
fn reftest_outset_box_shadow_clips_transparent_border_box_interior() {
    // CSS Backgrounds L3 §7.1.2: An outset `box-shadow: -4px 0 0 #000` on a transparent
    // element must paint ONLY the 4px strip outside the left border-box edge and keep the
    // element's interior untouched (background color).
    let pipeline = PipelineId(10);
    let mut resources = ResourceRegistry::default();
    let mut list = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 60.0));
    list.push(DisplayItem::Rect {
        bounds: Rect::new(0.0, 0.0, 120.0, 60.0),
        color: Rgba8::new(255, 255, 255, 255),
        radii: [0.0; 4],
    });
    list.push(DisplayItem::BoxShadow {
        bounds: Rect::new(20.0, 10.0, 80.0, 40.0),
        offset: Vec2::new(-4.0, 0.0),
        blur_radius: 0.0,
        spread_radius: 0.0,
        color: Rgba8::new(0, 0, 0, 255),
        radii: [0.0; 4],
        inset: false,
    });

    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        &list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    let pixels = render_rgba(&scene, 120, 60, BG_COLOR);

    // Left shadow strip at x=18, y=30 must be dark/black.
    let strip = pixel_at(&pixels, 120, 18, 30);
    assert!(
        strip[0] < 20 && strip[1] < 20 && strip[2] < 20,
        "outset shadow strip outside left edge must paint black, got {strip:?}"
    );

    // Interior of transparent element at x=50, y=30 must stay pure white!
    let interior = pixel_at(&pixels, 120, 50, 30);
    assert!(
        interior[0] > 240 && interior[1] > 240 && interior[2] > 240,
        "transparent element border-box interior must be clipped out of outset box-shadow, got {interior:?}"
    );
}
