use std::fmt::Write as _;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData, NodeId};
use rix_html::parse_html;

fn build_10k_dom_and_3k_rules() -> (Document, Arc<Stylesheet>, NodeId) {
    let mut html = String::with_capacity(600_000);
    html.push_str("<!doctype html><html><body><main id=\"app\" class=\"shell\">");
    // 100 sections * 99 items + wrapper elements ~= 10,004 element nodes.
    for s in 0..100 {
        let _ = write!(html, "<section class=\"group g-{}\">", s % 20);
        for i in 0..99 {
            if s == 50 && i == 50 {
                html.push_str("<div id=\"target\" class=\"card item-0\">Hi</div>");
            } else {
                let _ = write!(html, "<div class=\"card item-{}\">Hi</div>", i % 16);
            }
        }
        html.push_str("</section>");
    }
    html.push_str("</main></body></html>");

    let mut css = String::with_capacity(200_000);
    css.push_str(
        "#app.shell { display: block; width: 1200px; color: #1a1a1a; }\n\
         .group { display: flex; flex-wrap: wrap; gap: 8px; }\n\
         .group .card { display: block; padding: 8px; border: 1px solid #dddddd; }\n\
         .card:hover { background-color: #f0f4ff; }\n\
         .card.active { background-color: #ddeeff; }\n",
    );
    // Add 2,995 additional realistic rules across classes, IDs, and descendant chains.
    for r in 0..2995 {
        match r % 5 {
            0 => {
                let _ = writeln!(
                    css,
                    ".unused-cls-{r} {{ color: #{:06x}; padding: 4px; }}",
                    (r * 7919) & 0x00ff_ffff
                );
            }
            1 => {
                let _ = writeln!(css, ".nonexistent-parent-{r} .card {{ margin-top: 4px; }}");
            }
            2 => {
                let _ = writeln!(css, "#missing-id-{r} {{ width: {}px; }}", 100 + (r % 200));
            }
            3 => {
                let _ = writeln!(css, ".g-{} .item-{} {{ opacity: 0.95; }}", r % 20, r % 16);
            }
            _ => {
                let _ = writeln!(css, "section.other-{r} > div.card {{ font-weight: 600; }}");
            }
        }
    }

    let doc = parse_html(&html);
    let sheet = Arc::new(Stylesheet::parse(
        &css,
        CascadeOrigin::Author,
        &MediaContext::default(),
    ));

    let mut target = doc.root();
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
        if let Some(NodeData::Element(el)) = doc.get(id) {
            if el.cached_id.as_deref() == Some("target") {
                target = id;
                break;
            }
        }
        for c in doc.children(id) {
            stack.push(c);
        }
    }
    (doc, sheet, target)
}

fn bench_cascade(c: &mut Criterion) {
    let (base_doc, sheet, target) = build_10k_dom_and_3k_rules();
    let media = MediaContext::default();

    assert!(
        sheet.rules.len() >= 3000,
        "expected >= 3000 rules, got {}",
        sheet.rules.len()
    );

    c.bench_function("full_cascade_10k_nodes_3k_rules", |b| {
        let mut ctx = StyleContext::new(media);
        ctx.add_stylesheet(Arc::clone(&sheet));
        ctx.ensure_index();
        let mut doc = base_doc.clone();
        b.iter(|| {
            let rep = ctx.cascade_full(&mut doc);
            std::hint::black_box(rep);
        });
    });

    c.bench_function("incremental_hover_restyle_10k_nodes", |b| {
        let mut ctx = StyleContext::new(media);
        ctx.add_stylesheet(Arc::clone(&sheet));
        let mut doc = base_doc.clone();
        ctx.cascade_full(&mut doc);

        let mut hovered = false;
        b.iter(|| {
            hovered = !hovered;
            ctx.set_hovered(&mut doc, if hovered { Some(target) } else { None });
            let rep = ctx.restyle_dirty(&mut doc);
            std::hint::black_box(rep);
        });
    });

    c.bench_function("incremental_class_toggle_10k_nodes", |b| {
        let mut ctx = StyleContext::new(media);
        ctx.add_stylesheet(Arc::clone(&sheet));
        let mut doc = base_doc.clone();
        ctx.cascade_full(&mut doc);

        b.iter_batched(
            || (),
            |()| {
                if let Some(NodeData::Element(el)) = doc.get_mut(target) {
                    let next = if el.has_class_hash(rix_dom::hash_ident("active")) {
                        "card item-0"
                    } else {
                        "card item-0 active"
                    };
                    el.set_attribute("class", next);
                }
                ctx.invalidate_class_change(&mut doc, target, "active");
                let rep = ctx.restyle_dirty(&mut doc);
                std::hint::black_box(rep);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_cascade);
criterion_main!(benches);
