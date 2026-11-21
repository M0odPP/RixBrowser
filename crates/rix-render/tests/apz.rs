use rix_core::{Point, Rect, Size, Vec2};
use rix_ipc::{CompositorToContent, PipelineId, ScrollId, SpatialNodeId};
use rix_render::{
    apply_scroll_wheel, clamp_scroll_offset, encode_display_list, hit_test_scroll_chain, BlendMode,
    Color, ColorStop, Compositor, DisplayItem, DisplayList, EncodeOptions, LayerRole,
    ResourceRegistry, Rgba8, ScrollDelta, SpatialNodeKind, SpatialTree, VelloRenderer,
    DEFAULT_VIEWPORT_OVERSCAN_PX,
};
use smallvec::smallvec;
use std::sync::Mutex;
use vello::Scene;

fn render_rgba(scene: &Scene, width: u32, height: u32, bg: Color) -> Vec<u8> {
    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut renderer = VelloRenderer::new();
    renderer
        .render_to_rgba_blocking(scene, width, height, bg)
        .expect("headless GPU render succeeds")
}

#[test]
fn nested_scroll_hit_testing_clamping_and_chaining() {
    let mut tree = SpatialTree::new();
    let outer_id = tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::ScrollFrame {
            content_rect: Rect::new(0.0, 0.0, 1280.0, 2400.0),
            viewport_rect: Rect::new(0.0, 0.0, 1280.0, 720.0),
            external_scroll_id: ScrollId(1),
            current_offset: Vec2::ZERO,
        },
    );
    let inner_id = tree.add_node(
        Some(outer_id),
        SpatialNodeKind::ScrollFrame {
            content_rect: Rect::new(40.0, 100.0, 300.0, 800.0),
            viewport_rect: Rect::new(40.0, 100.0, 300.0, 300.0),
            external_scroll_id: ScrollId(2),
            current_offset: Vec2::ZERO,
        },
    );

    // Hit inside nested container returns [inner, outer].
    assert_eq!(
        hit_test_scroll_chain(&tree, Point::new(150.0, 200.0)),
        vec![inner_id, outer_id]
    );
    // Hit outside nested container returns [outer].
    assert_eq!(
        hit_test_scroll_chain(&tree, Point::new(600.0, 200.0)),
        vec![outer_id]
    );

    // Clamp bounds check (inner max_y = 800 - 300 = 500).
    let clamped = clamp_scroll_offset(
        Rect::new(40.0, 100.0, 300.0, 800.0),
        Rect::new(40.0, 100.0, 300.0, 300.0),
        Vec2::new(-50.0, 9999.0),
    );
    assert_eq!(clamped, Vec2::new(0.0, 500.0));

    // Wheel delta of +650 at [150, 200]: inner consumes 500, remaining 150 chains to outer!
    let outcomes = apply_scroll_wheel(
        PipelineId(1),
        &mut tree,
        Point::new(150.0, 200.0),
        ScrollDelta::new(0.0, 650.0),
    );
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].node_id, inner_id);
    assert_eq!(outcomes[0].new_offset, Vec2::new(0.0, 500.0));
    assert_eq!(outcomes[1].node_id, outer_id);
    assert_eq!(outcomes[1].new_offset, Vec2::new(0.0, 150.0));

    // Attempting to scroll negative at top clamps to 0 with scrolled == false.
    let top_outcomes = apply_scroll_wheel(
        PipelineId(1),
        &mut tree,
        Point::new(150.0, 200.0),
        ScrollDelta::new(0.0, -2000.0),
    );
    assert_eq!(top_outcomes[0].new_offset, Vec2::ZERO);
    assert_eq!(top_outcomes[1].new_offset, Vec2::ZERO);
    let noop = apply_scroll_wheel(
        PipelineId(1),
        &mut tree,
        Point::new(150.0, 200.0),
        ScrollDelta::new(0.0, -50.0),
    );
    assert!(noop.iter().all(|o| !o.scrolled));
}

#[test]
fn tall_50_screen_strip_cache_scrolls_with_zero_reencode_and_coalesces_syncs() {
    let content_rect = Rect::new(0.0, 0.0, 1280.0, 36_000.0);
    let viewport_rect = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let mut dl = DisplayList::new(content_rect);
    let root_scroll = dl.spatial_tree.add_node(
        Some(SpatialNodeId(0)),
        SpatialNodeKind::ScrollFrame {
            content_rect,
            viewport_rect,
            external_scroll_id: ScrollId(1),
            current_offset: Vec2::ZERO,
        },
    );

    for i in 0..2_500u32 {
        let y = (i as f32) * 14.2;
        let bounds = Rect::new(20.0 + ((i % 4) as f32) * 300.0, y, 280.0, 48.0);
        dl.push(DisplayItem::PushStackingContext {
            spatial: root_scroll,
            bounds,
            opacity: 1.0,
            mix_blend: BlendMode::Normal,
            filters: smallvec![],
        });
        dl.push(DisplayItem::Rect {
            bounds,
            color: Rgba8::new(40, 80, 140, 255),
            radii: [6.0; 4],
        });
        dl.push(DisplayItem::Border {
            bounds,
            widths: [1.0; 4],
            colors: [Rgba8::new(120, 150, 210, 255); 4],
            radii: [6.0; 4],
        });
        dl.push(DisplayItem::PopStackingContext);
    }

    let pipeline = PipelineId(10);
    let mut compositor = Compositor::new(Size::new(1280.0, 720.0), 1.0);
    compositor.submit_display_list(pipeline, LayerRole::WebSurface, 1, dl);
    assert!(compositor.compose().is_some());
    assert_eq!(compositor.layer(pipeline).unwrap().encode_generation(), 1);

    // 20 incremental wheel steps totaling +900 px within one VSync window.
    for _ in 0..20 {
        assert!(compositor.handle_wheel(Point::new(500.0, 300.0), 0.0, 45.0));
        assert!(compositor.compose().is_some());
    }

    // Zero DisplayList cloning / full layer re-encodes during APZ scroll!
    assert_eq!(compositor.layer(pipeline).unwrap().encode_generation(), 1);
    let stats = compositor
        .strip_cache_stats(pipeline)
        .expect("strip cache must be active for 50-screen document");
    assert!(stats.active);
    assert_eq!(stats.strip_height_px, 512);
    assert!(stats.cache_hits > 30, "expected hot strip hits: {stats:?}");
    assert!(
        stats.visible_strips_appended <= 4 && stats.visible_strips_appended >= 2,
        "only visible strips appended: {stats:?}"
    );

    // All 20 wheel notifications coalesce into a single ScrollOffsetSync per VSync.
    let syncs = compositor.drain_scroll_syncs();
    assert_eq!(
        syncs,
        vec![CompositorToContent::ScrollOffsetSync {
            pipeline,
            node_id: root_scroll,
            offset: Point::new(0.0, 900.0),
        }]
    );
    assert!(compositor.drain_scroll_syncs().is_empty());
}

#[test]
fn viewport_culling_skips_offscreen_items_and_subtrees_on_10k_display_list() {
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 1280.0, 36_000.0));
    for i in 0..10_000u32 {
        let y = (i as f32) * 3.6;
        dl.push(DisplayItem::Rect {
            bounds: Rect::new(20.0, y, 200.0, 3.0),
            color: Rgba8::WHITE,
            radii: [0.0; 4],
        });
    }
    // Add 10 offscreen clipped subtrees far below the viewport.
    for k in 0..10u32 {
        let y = 10_000.0 + (k as f32) * 100.0;
        dl.push(DisplayItem::PushClip {
            spatial: SpatialNodeId(0),
            rect: Rect::new(0.0, y, 400.0, 80.0),
            radii: [0.0; 4],
        });
        dl.push(DisplayItem::Rect {
            bounds: Rect::new(10.0, y + 10.0, 100.0, 40.0),
            color: Rgba8::WHITE,
            radii: [0.0; 4],
        });
        dl.push(DisplayItem::PopClip);
    }

    let mut resources = ResourceRegistry::default();
    let viewport = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let opts = EncodeOptions {
        cull_rect: Some(viewport.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
        viewport_clip: Some(viewport),
        base_transform: vello::kurbo::Affine::IDENTITY,
        override_scroll_zero: None,
    };
    let mut scene = Scene::new();
    let stats = encode_display_list(PipelineId(1), &dl, &mut resources, &opts, &mut scene);

    assert!(
        stats.items_culled > 9_000,
        "expected > 9,000 culled items, got {stats:?}"
    );
    assert_eq!(stats.subtrees_culled, 10);
}

#[test]
fn headless_gpu_strip_cache_matches_direct_render_pixel_equivalent() {
    let content_rect = Rect::new(0.0, 0.0, 640.0, 3200.0);
    let viewport_rect = Rect::new(0.0, 0.0, 640.0, 480.0);

    let make_doc = |offset_y: f32| {
        let mut dl = DisplayList::new(content_rect);
        let root_scroll = dl.spatial_tree.add_node(
            Some(SpatialNodeId(0)),
            SpatialNodeKind::ScrollFrame {
                content_rect,
                viewport_rect,
                external_scroll_id: ScrollId(1),
                current_offset: Vec2::new(0.0, offset_y),
            },
        );
        dl.push(DisplayItem::PushStackingContext {
            spatial: root_scroll,
            bounds: content_rect,
            opacity: 1.0,
            mix_blend: BlendMode::Normal,
            filters: smallvec![],
        });
        // Alternating cards including items intentionally crossing 512 / 1024 / 1536 strip seams!
        for row in 0..24u32 {
            let y = 100.0 + (row as f32) * 120.0;
            let bounds = Rect::new(40.0, y, 560.0, 90.0);
            dl.push(DisplayItem::LinearGradient {
                bounds,
                start: Point::new(bounds.min_x(), bounds.min_y()),
                end: Point::new(bounds.max_x(), bounds.max_y()),
                stops: smallvec![
                    ColorStop {
                        offset: 0.0,
                        color: Rgba8::new(30 + (row * 7) as u8, 90, 180, 220),
                    },
                    ColorStop {
                        offset: 1.0,
                        color: Rgba8::new(190, 60 + (row * 5) as u8, 90, 220),
                    },
                ],
                radii: [12.0; 4],
            });
            dl.push(DisplayItem::Border {
                bounds,
                widths: [2.0; 4],
                colors: [Rgba8::new(230, 235, 245, 255); 4],
                radii: [12.0; 4],
            });
        }
        // Translucent card straddling strip boundary y = 1536.0 directly
        dl.push(DisplayItem::Rect {
            bounds: Rect::new(120.0, 1490.0, 400.0, 110.0),
            color: Rgba8::new(40, 220, 120, 128),
            radii: [16.0; 4],
        });
        dl.push(DisplayItem::PopStackingContext);
        dl
    };

    let bg = Color::from_rgba8(14, 18, 26, 255);

    // Path A: Compositor with StripSceneCache, scrolled via APZ to y = 1200.0 (crosses strip 2 & 3 at 1536).
    let mut compositor = Compositor::new(Size::new(640.0, 480.0), 1.0);
    compositor.submit_display_list(PipelineId(1), LayerRole::WebSurface, 1, make_doc(0.0));
    assert!(compositor.compose().is_some());
    assert!(compositor.handle_wheel(Point::new(320.0, 240.0), 0.0, 1200.0));
    let pixels_strip = {
        let strip_scene = compositor.compose().expect("recompose after wheel");
        render_rgba(strip_scene, 640, 480, bg)
    };

    // Path B: Direct single-pass encode with viewport culling/clipping at scroll_y = 1200.0.
    let direct_dl = make_doc(1200.0);
    let mut direct_resources = ResourceRegistry::default();
    let mut direct_scene = Scene::new();
    encode_display_list(
        PipelineId(1),
        &direct_dl,
        &mut direct_resources,
        &EncodeOptions {
            cull_rect: Some(viewport_rect.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
            viewport_clip: Some(viewport_rect),
            base_transform: vello::kurbo::Affine::IDENTITY,
            override_scroll_zero: None,
        },
        &mut direct_scene,
    );

    let pixels_direct = render_rgba(&direct_scene, 640, 480, bg);
    assert_eq!(pixels_strip.len(), pixels_direct.len());

    let max_diff = pixels_strip
        .iter()
        .zip(pixels_direct.iter())
        .map(|(&a, &b)| a.abs_diff(b))
        .max()
        .unwrap_or(0);
    assert!(
        max_diff <= 1,
        "strip-cached frame must match direct render within 1 channel unit, max_diff = {max_diff}"
    );
}
