//! Specification regression tests for CSS Flexbox L1 §4/§9.4, CSS 2.1 §10.3.9,
//! CSS Sizing L4 §6.1 `aspect-ratio`, and CSS Containment L3 `cqw`/`cqh` preservation.

use std::sync::Arc;

use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData, NodeId};
use rix_layout::{layout_tree, AssetRegistry, BoxTree, ConstraintSpace, Fragment, LayoutCache};
use rix_text::{FontDatabase, TextShapingCache};

fn layout_html(html: &str, css: &str, vw: f32, vh: f32) -> (Document, Fragment) {
    let mut doc = rix_html::parse_html(html);
    let media = MediaContext {
        viewport_width: vw,
        viewport_height: vh,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);
    let assets = AssetRegistry::new();
    let tree = BoxTree::build(&doc, &styles, &assets);
    let font_db = FontDatabase::new();
    let shaping = Arc::new(TextShapingCache::new(64 * 1024));
    let mut cache = LayoutCache::new();
    let ft = layout_tree(
        &tree,
        Size::new(vw, vh),
        &font_db,
        None,
        shaping,
        &mut cache,
    );
    (doc, ft.root.expect("root fragment"))
}

fn find_id(doc: &Document, id: &str) -> NodeId {
    doc.descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if e.get_attribute("id") == Some(id)))
        .expect("element exists")
}

fn find_abs_rect(
    frag: &Fragment,
    origin: rix_core::Point,
    target: NodeId,
) -> Option<rix_core::Rect> {
    let abs = frag.absolute_border_rect(origin);
    if frag.dom_node == Some(target) {
        return Some(abs);
    }
    for ch in frag.children.iter() {
        if let Some(r) = find_abs_rect(ch, abs.origin, target) {
            return Some(r);
        }
    }
    None
}

#[test]
fn flex_item_establishes_bfc_preserving_child_negative_margin_top() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="display:flex; flex-direction:column;">
                <header style="height:68px;"></header>
                <div id="item"><main><section id="hero" style="margin-top:-68px; height:200px;"></section></main></div>
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let item_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "item")).unwrap();
    let hero_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "hero")).unwrap();
    assert!((item_r.origin.y - 68.0).abs() < 0.5, "item_r={item_r:?}");
    assert!(
        hero_r.origin.y.abs() < 0.5,
        "CSS Flexbox §4: negative child margin-top must pull hero up to y=0 inside flex-item BFC, got {hero_r:?}"
    );
}

#[test]
fn nested_flex_item_with_percent_width_honors_parent_flex_resolved_width() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="display:flex; width:1000px;">
                <div style="width:200px; flex-shrink:0;"></div>
                <div id="slot" style="display:flex; flex:1 1 0%;">
                    <button id="btn" style="display:flex; width:100%; height:40px; border:0; padding:0;"></button>
                </div>
                <div style="width:300px; flex-shrink:0;"></div>
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let slot_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "slot")).unwrap();
    let btn_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "btn")).unwrap();
    assert!((slot_r.size.width - 500.0).abs() < 1.0, "slot_r={slot_r:?}");
    assert!(
        (btn_r.size.width - 500.0).abs() < 1.0,
        "Nested flex child must honor definite_width_override (500px), got {btn_r:?}"
    );
}

#[test]
fn inline_block_with_specified_width_resolves_auto_horizontal_margins_to_zero() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="width:800px; text-align:center;">
                <a id="a1" style="display:inline-block; width:200px; height:40px; margin:0 auto;"></a><a id="a2" style="display:inline-block; width:200px; height:40px; margin:0 auto;"></a>
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let r1 = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "a1")).unwrap();
    let r2 = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "a2")).unwrap();
    assert!(
        (r1.origin.y - r2.origin.y).abs() < 0.5,
        "CSS 2.1 §10.3.9: inline-blocks with margin:0 auto must sit side-by-side (margin=0), r1={r1:?} r2={r2:?}"
    );
    assert!((r2.origin.x - r1.max_x()).abs() < 0.5);
}

#[test]
fn aspect_ratio_applies_to_abspos_and_auto_width_blocks() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="position:relative; width:600px;">
                <div id="blk" style="aspect-ratio: 2 / 1;"></div>
                <div id="abs" style="position:absolute; left:0; top:0; width:400px; aspect-ratio: 1 / 1;"></div>
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let blk_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "blk")).unwrap();
    let abs_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "abs")).unwrap();
    assert!(
        (blk_r.size.height - 300.0).abs() < 1.0,
        "width:auto block with aspect-ratio 2/1 in 600px parent should be 300px tall, got {blk_r:?}"
    );
    assert!(
        (abs_r.size.height - 400.0).abs() < 1.0,
        "position:absolute 400px box with aspect-ratio 1/1 should be 400px tall, got {abs_r:?}"
    );
}

#[test]
fn eval_context_for_preserves_query_container_dimensions() {
    let mut style = rix_css::ComputedStyle::default();
    Arc::make_mut(&mut style.text).eval_ctx.container_width_px = 420.0;
    Arc::make_mut(&mut style.text).eval_ctx.container_height_px = 240.0;
    let space = ConstraintSpace::for_viewport(Size::new(1280.0, 800.0));
    let ctx = space.eval_context_for(&style);
    assert!((ctx.container_width_px - 420.0).abs() < 0.01);
    assert!((ctx.container_height_px - 240.0).abs() < 0.01);
}
