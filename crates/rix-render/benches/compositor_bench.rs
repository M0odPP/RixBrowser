#![deny(unsafe_code)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rix_core::{Point, Rect, Size, Vec2};
use rix_ipc::{PipelineId, ScrollId, SpatialNodeId};
use rix_render::{
    encode_display_list, BlendMode, Compositor, DisplayItem, DisplayList, EncodeOptions, LayerRole,
    ResourceRegistry, Rgba8, SpatialNodeKind, DEFAULT_VIEWPORT_OVERSCAN_PX,
};
use smallvec::smallvec;
use vello::Scene;

fn build_10k_item_50_screen_display_list() -> (DisplayList, SpatialNodeId) {
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

    dl.push(DisplayItem::PushStackingContext {
        spatial: root_scroll,
        bounds: content_rect,
        opacity: 1.0,
        mix_blend: BlendMode::Normal,
        filters: smallvec![],
    });
    // 2,500 cards x 4 display items each = 10,000 items across 50 screens (36,000 px).
    let step_y = 35_800.0 / 2_500.0;
    for i in 0..2_500u32 {
        let y = (i as f32) * step_y;
        let card = Rect::new(24.0 + ((i % 4) as f32) * 300.0, y, 280.0, 48.0);
        dl.push(DisplayItem::PushClip {
            spatial: root_scroll,
            rect: card,
            radii: [6.0; 4],
        });
        dl.push(DisplayItem::Rect {
            bounds: card,
            color: Rgba8::new((30 + (i % 40)) as u8, 42, 64, 255),
            radii: [6.0; 4],
        });
        dl.push(DisplayItem::Border {
            bounds: card,
            widths: [1.0; 4],
            colors: [Rgba8::new(90, 110, 145, 255); 4],
            radii: [6.0; 4],
        });
        dl.push(DisplayItem::PopClip);
    }
    dl.push(DisplayItem::PopStackingContext);
    (dl, root_scroll)
}

fn bench_encode_10k_culled(c: &mut Criterion) {
    let (dl, _) = build_10k_item_50_screen_display_list();
    let mut resources = ResourceRegistry::default();
    let viewport = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let options = EncodeOptions {
        cull_rect: Some(viewport.inflate(DEFAULT_VIEWPORT_OVERSCAN_PX)),
        viewport_clip: Some(viewport),
        base_transform: vello::kurbo::Affine::IDENTITY,
        override_scroll_zero: None,
        ..EncodeOptions::default()
    };
    let mut scene = Scene::new();

    c.bench_function("encode_10k_items_with_culling", |b| {
        b.iter(|| {
            scene.reset();
            let stats = encode_display_list(
                PipelineId(1),
                black_box(&dl),
                &mut resources,
                black_box(&options),
                &mut scene,
            );
            black_box(stats);
        });
    });
}

fn bench_apz_scroll_50_screens(c: &mut Criterion) {
    let (dl, _) = build_10k_item_50_screen_display_list();
    let mut compositor = Compositor::new(Size::new(1280.0, 720.0), 1.0);
    compositor.submit_display_list(PipelineId(1), LayerRole::WebSurface, 1, dl);
    assert!(compositor.compose().is_some());

    // Pre-warm a pair of strips around y = 1200..2000 for steady-state APZ scroll measurement.
    assert!(compositor.handle_wheel(Point::new(500.0, 300.0), 0.0, 1200.0));
    assert!(compositor.compose().is_some());
    let _ = compositor.drain_scroll_syncs();

    let mut dir = 1.0f32;
    c.bench_function("apz_scroll_update_50_screens", |b| {
        b.iter(|| {
            dir = -dir;
            let scrolled = compositor.handle_wheel(Point::new(500.0, 300.0), 0.0, dir * 24.0);
            let scene_ready = compositor.compose().is_some();
            let syncs = compositor.drain_scroll_syncs();
            black_box((scrolled, scene_ready, syncs.len()));
        });
    });
}

criterion_group!(
    benches,
    bench_encode_10k_culled,
    bench_apz_scroll_50_screens
);
criterion_main!(benches);
