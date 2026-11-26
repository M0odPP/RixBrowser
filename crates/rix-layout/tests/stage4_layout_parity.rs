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

#[test]
fn picture_source_selects_matching_media_and_supported_mime_with_dimensions() {
    let html = r#"<!doctype html><html><body style="margin:0;">
        <picture>
            <source media="(min-width: 800px)" type="image/avif" srcset="hero.avif 1x" width="3795" height="993">
            <source media="(min-width: 800px)" type="image/webp" srcset="hero.webp 1x, hero@2x.webp 2x" width="3795" height="993">
            <img id="hero" src="mobile.png" width="1200" height="508" style="display:block; width:100%; height:auto;">
        </picture>
    </body></html>"#;

    // Desktop viewport (1280px): skips unsupported AVIF, selects WebP `<source>` (3795x993 -> 334.9px tall).
    let (doc_d, root_d) = layout_html(html, "", 1280.0, 800.0);
    let hero_d = find_abs_rect(&root_d, rix_core::Point::ZERO, find_id(&doc_d, "hero")).unwrap();
    let expected_d = 1280.0 * (993.0 / 3795.0);
    assert!(
        (hero_d.size.height - expected_d).abs() < 1.5,
        "Desktop <picture><source> must size to {expected_d:.1}px, got {hero_d:?}"
    );

    // Mobile viewport (500px): media query fails, falls back to <img> (1200x508 -> 211.7px tall).
    let (doc_m, root_m) = layout_html(html, "", 500.0, 800.0);
    let hero_m = find_abs_rect(&root_m, rix_core::Point::ZERO, find_id(&doc_m, "hero")).unwrap();
    let expected_m = 500.0 * (508.0 / 1200.0);
    assert!(
        (hero_m.size.height - expected_m).abs() < 1.5,
        "Mobile <picture> fallback must size to {expected_m:.1}px, got {hero_m:?}"
    );
}

#[test]
fn svg_gradient_href_inherits_stops_from_template_gradient() {
    let svg_markup = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <defs>
            <linearGradient id="baseGrad">
                <stop offset="0%" stop-color="#ff3366"/>
                <stop offset="100%" stop-color="#3366ff"/>
            </linearGradient>
            <linearGradient id="derivedGrad" href="#baseGrad" x1="0" y1="0" x2="1" y2="1"/>
        </defs>
        <rect x="0" y="0" width="100" height="100" fill="url(#derivedGrad)"/>
    </svg>"##;
    let doc = rix_html::parse_html(svg_markup);
    let decoded = rix_layout::extract_svg_from_document(&doc, rix_css::ColorRgba::BLACK)
        .expect("SVG with href-inherited gradient must decode cleanly");
    assert_eq!(decoded.paths.len(), 1);
    assert_eq!(
        decoded.paths[0].fill,
        Some(rix_css::ColorRgba::rgb(0xff, 0x33, 0x66))
    );
}

#[test]
fn empty_self_collapsing_first_block_does_not_shift_following_float() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="height:60px;"></div>
            <div id="content">
                <div id="empty" style="margin: 20px 0;"></div>
                <div id="flt" style="float:left; width:100%; height:120px;"></div>
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let content_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "content")).unwrap();
    let flt_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "flt")).unwrap();
    assert!(
        (content_r.origin.y - 80.0).abs() < 0.5,
        "Empty first child margin should collapse into parent top (y=80), got {content_r:?}"
    );
    assert!(
        (flt_r.origin.y - content_r.origin.y).abs() < 0.5,
        "CSS 2.1 §8.3.1: following float must start at parent content edge (y=80), got {flt_r:?}"
    );
}

#[test]
fn negative_half_leading_preserves_exact_line_height_when_smaller_than_font_size() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <h1 id="h" style="margin:0; font-size:62.4px; line-height:20.8px;">The Linux Kernel Archives</h1>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let h_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "h")).unwrap();
    assert!(
        (h_r.size.height - 20.8).abs() < 0.5,
        "CSS 2.1 §10.8.1: negative half-leading must yield exact 20.8px line box, got {h_r:?}"
    );
}

#[test]
fn em_letter_spacing_resolves_against_element_font_size() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <span id="normal" style="display:inline-block; font-size:80px; letter-spacing:0;">Speed you can trust</span>
            <span id="tight" style="display:inline-block; font-size:80px; letter-spacing:-0.05em;">Speed you can trust</span>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let normal_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "normal")).unwrap();
    let tight_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "tight")).unwrap();
    let delta = normal_r.size.width - tight_r.size.width;
    // 19 chars * (-0.05 * 80px = -4px/char) = ~76px narrower (vs ~15px if resolved against 16px default!).
    assert!(
        delta > 45.0,
        "CSS Text L3: -0.05em letter-spacing at 80px must tighten width by >45px (got delta={delta:.1}, normal={normal_r:?}, tight={tight_r:?})"
    );
}

#[test]
fn replaced_img_honors_css_aspect_ratio_without_intrinsic_dimensions() {
    let (doc, root) = layout_html(
        r#"<!doctype html><html><body style="margin:0;">
            <div style="width:1232px;">
                <img id="hero" src="placeholder.webp" style="display:block; width:100%; height:auto; aspect-ratio:1240/618;">
            </div>
        </body></html>"#,
        "",
        1280.0,
        800.0,
    );
    let hero_r = find_abs_rect(&root, rix_core::Point::ZERO, find_id(&doc, "hero")).unwrap();
    assert!(
        (hero_r.size.width - 1232.0).abs() < 0.5 && (hero_r.size.height - 614.0).abs() < 1.0,
        "CSS Sizing L4 §6.1: replaced img with aspect-ratio 1240/618 must size to 1232x614, got {hero_r:?}"
    );
}
