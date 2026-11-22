use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_layout::{layout_tree, AssetRegistry, BoxTree, Fragment, FragmentKind, LayoutCache};
use rix_text::{shared_shaping_cache, FontDatabase};

fn test_fonts() -> FontDatabase {
    let mut db = FontDatabase::new();
    db.scan_system_fonts();
    db
}

fn layout_html(html: &str, css: &str, viewport: Size) -> (Document, StyleContext, Fragment) {
    let mut doc = parse_html(html);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    if !css.is_empty() {
        styles.add_stylesheet(Arc::new(Stylesheet::parse(
            css,
            CascadeOrigin::Author,
            &media,
        )));
    }
    styles.cascade_full(&mut doc);
    let assets = AssetRegistry::new();
    let box_tree = BoxTree::build(&doc, &styles, &assets);
    let font_db = test_fonts();
    let mut cache = LayoutCache::new();
    let ftree = layout_tree(
        &box_tree,
        viewport,
        &font_db,
        None,
        shared_shaping_cache(),
        &mut cache,
    );
    (doc, styles, ftree.root.expect("root fragment"))
}

fn find_abs_rect_by_id(doc: &Document, root: &Fragment, id: &str) -> Option<Rect> {
    let target = doc.get_element_by_id(id)?;
    find_abs_rect_for_node(root, target, Point::ZERO)
}

fn find_abs_rect_for_node(
    frag: &Fragment,
    target: rix_dom::NodeId,
    parent_origin: Point,
) -> Option<Rect> {
    let abs = frag.absolute_border_rect(parent_origin);
    if frag.dom_node == Some(target) {
        return Some(abs);
    }
    for ch in frag.children.iter() {
        if let Some(found) = find_abs_rect_for_node(ch, target, abs.origin) {
            return Some(found);
        }
    }
    None
}

fn collect_inline_boxes_for_node(
    frag: &Fragment,
    target: rix_dom::NodeId,
    parent_origin: Point,
    out: &mut Vec<(Rect, bool, bool)>,
) {
    let abs = frag.absolute_border_rect(parent_origin);
    if frag.dom_node == Some(target) {
        if let FragmentKind::InlineBox { is_first, is_last } = frag.kind {
            out.push((abs, is_first, is_last));
        }
    }
    for ch in frag.children.iter() {
        collect_inline_boxes_for_node(ch, target, abs.origin, out);
    }
}

fn collect_text_runs(frag: &Fragment, out: &mut Vec<String>) {
    if let FragmentKind::TextRun(run) = &frag.kind {
        out.push(run.text.clone());
    }
    for ch in frag.children.iter() {
        collect_text_runs(ch, out);
    }
}

#[test]
fn bfc_collapsing_margins_and_isolation() {
    let html = r#"
        <html><body>
            <div id="a"></div>
            <div id="b"></div>
            <div id="bfc"><div id="inner"></div></div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; }
        #a { height: 20px; margin-bottom: 30px; }
        #b { height: 20px; margin-top: 20px; margin-bottom: 10px; }
        #bfc { display: flow-root; margin-top: 25px; }
        #inner { height: 15px; margin-top: 40px; }
    "#;
    let (doc, _, root) = layout_html(html, css, Size::new(800.0, 600.0));
    let ra = find_abs_rect_by_id(&doc, &root, "a").unwrap();
    let rb = find_abs_rect_by_id(&doc, &root, "b").unwrap();
    let rbfc = find_abs_rect_by_id(&doc, &root, "bfc").unwrap();
    let rinner = find_abs_rect_by_id(&doc, &root, "inner").unwrap();

    assert!((rb.min_y() - ra.max_y() - 30.0).abs() < 0.5);
    assert!((rbfc.min_y() - rb.max_y() - 25.0).abs() < 0.5);
    assert!((rinner.min_y() - rbfc.min_y() - 40.0).abs() < 0.5);
}

#[test]
fn bfc_floats_exclusion_bands_and_clear() {
    let html = r#"
        <html><body>
            <div id="wrap">
                <div id="fl"></div>
                <div id="fr"></div>
                <div id="cleared"></div>
            </div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; }
        #wrap { width: 400px; display: flow-root; }
        #fl { float: left; width: 100px; height: 80px; }
        #fr { float: right; width: 120px; height: 50px; }
        #cleared { clear: both; height: 30px; }
    "#;
    let (doc, _, root) = layout_html(html, css, Size::new(800.0, 600.0));
    let rfl = find_abs_rect_by_id(&doc, &root, "fl").unwrap();
    let rfr = find_abs_rect_by_id(&doc, &root, "fr").unwrap();
    let rcl = find_abs_rect_by_id(&doc, &root, "cleared").unwrap();
    let rwrap = find_abs_rect_by_id(&doc, &root, "wrap").unwrap();

    assert!((rfl.min_x() - 0.0).abs() < 0.5);
    assert!((rfr.min_x() - 280.0).abs() < 0.5);
    assert!((rcl.min_y() - 80.0).abs() < 0.5);
    assert!((rwrap.size.height - 110.0).abs() < 0.5);
}

#[test]
fn ifc_multiline_inline_box_splitting_ellipsis_and_positioned() {
    let html = r#"
        <html><body>
            <div id="para"><span id="badge">alpha beta gamma delta epsilon zeta</span></div>
            <div id="ell">SuperLongHeadlineThatMustTruncateWithAnEllipsisInNarrowBox</div>
            <div id="stage"><div id="rel"></div><div id="abs"></div></div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; font-size: 16px; }
        #para { width: 140px; }
        #badge { padding-left: 8px; padding-right: 12px; border: 2px solid #222; }
        #ell { width: 90px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
        #stage { position: relative; width: 400px; height: 200px; }
        #rel { position: relative; top: 12px; left: 16px; width: 60px; height: 30px; }
        #abs { position: absolute; right: 10px; bottom: 15px; width: 80px; height: 40px; }
    "#;
    let (doc, _, root) = layout_html(html, css, Size::new(600.0, 400.0));
    let badge_node = doc.get_element_by_id("badge").unwrap();
    let mut spans = Vec::new();
    collect_inline_boxes_for_node(&root, badge_node, Point::ZERO, &mut spans);
    assert!(spans.len() >= 2);
    assert!(spans.first().unwrap().1 && !spans.first().unwrap().2);
    assert!(spans.last().unwrap().2);

    let mut texts = Vec::new();
    collect_text_runs(&root, &mut texts);
    assert!(texts.iter().any(|t| t.contains('\u{2026}')));

    let stage = find_abs_rect_by_id(&doc, &root, "stage").unwrap();
    let rel = find_abs_rect_by_id(&doc, &root, "rel").unwrap();
    let abs = find_abs_rect_by_id(&doc, &root, "abs").unwrap();
    assert!((rel.min_x() - stage.min_x() - 16.0).abs() < 0.5);
    assert!((rel.min_y() - stage.min_y() - 12.0).abs() < 0.5);
    assert!((abs.min_x() - stage.min_x() - 310.0).abs() < 0.5);
    assert!((abs.min_y() - stage.min_y() - 145.0).abs() < 0.5);
}

#[test]
fn incremental_layout_cache_reuses_unchanged_subtrees() {
    let mut html = String::from("<html><body><div id='root'>");
    for i in 0..40 {
        html.push_str(&format!(
            "<section id='s{i}' class='card'><h3>Card {i}</h3><p>Paragraph content {i}</p></section>"
        ));
    }
    html.push_str("</div></body></html>");

    let css = r#"
        body { margin: 0; }
        .card { display: flow-root; padding: 8px; margin-bottom: 6px; }
        .wide { width: 500px; }
    "#;
    let viewport = Size::new(900.0, 700.0);
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

    let assets = AssetRegistry::new();
    let font_db = test_fonts();
    let shaping = shared_shaping_cache();
    let mut cache = LayoutCache::new();

    let tree1 = BoxTree::build(&doc, &styles, &assets);
    let _ft1 = layout_tree(
        &tree1,
        viewport,
        &font_db,
        None,
        Arc::clone(&shaping),
        &mut cache,
    );
    assert!(cache.cache_misses > 40);

    doc.clear_all_dirty_flags();
    let s0 = doc.get_element_by_id("s0").unwrap();
    if let Some(NodeData::Element(e)) = doc.get_mut(s0) {
        e.set_attribute("class", "card wide");
    }
    styles.invalidate_class_change(&mut doc, s0, "wide");
    styles.restyle_dirty(&mut doc);

    let tree2 = BoxTree::build(&doc, &styles, &assets);
    let ft2 = layout_tree(&tree2, viewport, &font_db, None, shaping, &mut cache);
    assert!(cache.cache_hits >= 39);
    let r0 = find_abs_rect_by_id(&doc, ft2.root.as_ref().unwrap(), "s0").unwrap();
    assert!((r0.size.width - 516.0).abs() < 1.5);
}

#[test]
fn max_width_auto_margins_nested_row_flex_and_text_run_outline_regression() {
    let html = r#"<!DOCTYPE html><html><head><style>
        * { box-sizing: border-box; }
        body { margin: 0; }
        .bar { display: flex; align-items: center; gap: 8px; padding: 0 8px; height: 44px; }
        .grp { display: flex; align-items: center; gap: 4px; flex-shrink: 0; }
        .btn { width: 32px; height: 32px; flex-shrink: 0; }
        .fill { flex-grow: 1; height: 32px; }
        .centered { max-width: 600px; margin: 0 auto; height: 40px; }
        .outlined { display: flex; outline: 2px solid #3b82f6; outline-offset: 2px; }
    </style></head><body>
        <div class="bar">
            <div id="left-grp" class="grp">
                <div id="b1" class="btn"></div>
                <div id="b2" class="btn"></div>
                <div id="b3" class="btn"></div>
            </div>
            <div id="mid" class="fill"></div>
        </div>
        <div id="centered" class="centered"></div>
        <div id="outlined" class="outlined">Reload tab</div>
    </body></html>"#;

    let viewport = Size::new(1000.0, 400.0);
    let mut doc = parse_html(html);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    let css = {
        let mut s = String::new();
        for node in doc.descendants(doc.root()) {
            if let Some(NodeData::Element(e)) = doc.get(node) {
                if e.name.local.eq_str_ignore_ascii_case("style") {
                    for ch in doc.children(node) {
                        if let Some(NodeData::Text(t)) = doc.get(ch) {
                            s.push_str(t);
                        }
                    }
                }
            }
        }
        s
    };
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        &css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let mut engine = rix_layout::LayoutEngine::new();
    let mut resources = rix_render::ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        viewport,
        &font_db,
        rix_ipc::PipelineId(1),
        &mut resources,
    );
    let root = frame.fragment_tree.root.as_ref().unwrap();

    // 1. Nested row flex `.grp` sums 3x32px + 2x4px gaps = 104px instead of collapsing to 32px.
    let grp = find_abs_rect_by_id(&doc, root, "left-grp").unwrap();
    assert!(
        (grp.size.width - 104.0).abs() < 1.0,
        "expected nested row flex width 104px, got {:.1}",
        grp.size.width
    );
    let mid = find_abs_rect_by_id(&doc, root, "mid").unwrap();
    assert!(
        mid.min_x() >= 119.0,
        "fill sibling must not overlap left-grp"
    );

    // 2. `max-width: 600px; margin: 0 auto` centers horizontally at x = 200px in 1000px viewport.
    let centered = find_abs_rect_by_id(&doc, root, "centered").unwrap();
    assert!((centered.size.width - 600.0).abs() < 1.0);
    assert!(
        (centered.min_x() - 200.0).abs() < 1.0,
        "expected centered x=200px, got {:.1}",
        centered.min_x()
    );

    // 3. `outline` on parent box paints exactly 1 Border DisplayItem, never duplicated per TextRun.
    let border_count = frame
        .display_list
        .items
        .iter()
        .filter(|i| matches!(i, rix_render::DisplayItem::Border { .. }))
        .count();
    assert_eq!(
        border_count, 1,
        "parent outline must paint once, not per TextRun word"
    );
}
