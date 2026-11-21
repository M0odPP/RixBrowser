use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::NodeData;
use rix_html::parse_html;
use rix_layout::LayoutEngine;
use rix_text::FontDatabase;

fn build_large_document(sections: usize) -> (rix_dom::Document, StyleContext, Size) {
    let mut html = String::with_capacity(sections * 180);
    html.push_str("<html><body><main id='app'>");
    for i in 0..sections {
        html.push_str(&format!(
            "<section id='sec-{i}' class='card'>\
             <div class='row'><span class='tag'>Tag {i}</span><span class='meta'>Item {i}</span></div>\
             <p class='body'>Section {i} body paragraph with multiple words for layout shaping and caching.</p>\
             </section>"
        ));
    }
    html.push_str("</main></body></html>");

    let css = r#"
        body { margin: 0; font-size: 14px; }
        #app { width: 1200px; }
        .card { display: flow-root; padding: 10px; margin-bottom: 8px; border: 1px solid #d1d5db; }
        .row { display: flex; justify-content: space-between; margin-bottom: 4px; }
        .tag { font-weight: 700; }
        .body { margin: 0; }
        .highlighted { padding: 16px; }
    "#;

    let viewport = Size::new(1280.0, 800.0);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(&html);
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);
    (doc, styles, viewport)
}

fn bench_layout_engine(c: &mut Criterion) {
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();

    let (mut doc, mut styles, viewport) = build_large_document(250);
    let mut engine = LayoutEngine::new();
    // Warm up layout cache and shaping cache.
    let _ = engine.layout_only(&doc, &styles, viewport, &font_db);
    doc.clear_all_dirty_flags();

    let target = doc.get_element_by_id("sec-0").expect("sec-0 exists");
    let mut toggled = false;

    c.bench_function("incremental_layout_cached_subtrees_250_sections", |b| {
        b.iter(|| {
            toggled = !toggled;
            let cls = if toggled { "card highlighted" } else { "card" };
            if let Some(NodeData::Element(e)) = doc.get_mut(target) {
                e.set_attribute("class", cls);
            }
            styles.invalidate_class_change(&mut doc, target, "highlighted");
            styles.restyle_dirty(&mut doc);

            let (ft, metrics) = engine.layout_only(&doc, &styles, viewport, &font_db);
            doc.clear_all_dirty_flags();
            black_box((ft.content_bounds, metrics.cache_hits));
        });
    });

    c.bench_function("full_layout_cold_250_sections", |b| {
        b.iter(|| {
            engine.clear_caches();
            let (ft, metrics) = engine.layout_only(&doc, &styles, viewport, &font_db);
            black_box((ft.content_bounds, metrics.cache_misses));
        });
    });
}

criterion_group!(benches, bench_layout_engine);
criterion_main!(benches);
