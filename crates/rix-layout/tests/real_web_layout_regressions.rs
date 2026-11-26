use std::sync::Arc;
use std::time::Instant;

use rix_core::{Point, Rect, Size};
use rix_css::{CascadeOrigin, ColorRgba, MediaContext, StyleContext, Stylesheet};
use rix_dom::NodeId;
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::{extract_svg_from_document, Fragment, LayoutEngine};
use rix_render::{DisplayItem, ResourceRegistry, Rgba8};
use rix_text::FontDatabase;

fn find_abs_rect(frag: &Fragment, origin: Point, target: NodeId) -> Option<Rect> {
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
fn dom_insert_before_self_and_ancestor_cycle_are_safe_no_ops() {
    let mut doc =
        parse_html("<!doctype html><html><body><div id='a'><p id='b'>Hi</p></div></body></html>");
    let a = doc.get_element_by_id("a").unwrap();
    let b = doc.get_element_by_id("b").unwrap();

    // Must never panic on self-insertion or ancestor cycles!
    doc.insert_before(b, b);
    doc.insert_before(b, a);
    doc.append_child(b, a);
    doc.append_child(a, a);
    assert_eq!(doc.parent(b), Some(a));
}

#[test]
fn inline_containing_block_and_centered_table_layout_properly() {
    let html = r##"<!doctype html><html><body>
        <center>
            <table id="hn" width="80%" bgcolor="#f6f6ef">
                <tr><td id="cell">Hacker News Story Title</td></tr>
            </table>
        </center>
        <a id="wrap" href="/card"><div id="card" style="width: 300px; height: 48px;">Card Inside Link</div></a>
    </body></html>"##;

    let mut doc = parse_html(html);
    let media = MediaContext {
        viewport_width: 1000.0,
        viewport_height: 600.0,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(1000.0, 600.0), &font_db);
    let root = tree.root.as_ref().expect("root fragment");

    let hn_node = doc.get_element_by_id("hn").unwrap();
    let card_node = doc.get_element_by_id("card").unwrap();

    let hn_rect = find_abs_rect(root, Point::ZERO, hn_node).expect("table rendered");
    assert!(
        hn_rect.size.width > 700.0 && hn_rect.size.height > 10.0,
        "centered table must have non-zero geometry, got {hn_rect:?}"
    );
    assert!(
        hn_rect.min_x() > 50.0,
        "center > table must be horizontally centered via auto margins, got x={}",
        hn_rect.min_x()
    );

    let card_rect = find_abs_rect(root, Point::ZERO, card_node).expect("block inside <a> rendered");
    assert!((card_rect.size.width - 300.0).abs() < 2.0);
    assert!((card_rect.size.height - 48.0).abs() < 2.0);
}

#[test]
fn grid_template_areas_places_sidebar_and_main_side_by_side() {
    let css = r#"
        .shell {
            display: grid;
            column-gap: 24px;
            grid-template: auto 1fr / 200px 1fr;
            grid-template-areas:
                'notice notice'
                'sidebar main';
        }
        .sidebar { grid-area: sidebar; }
        .main { grid-area: main; }
    "#;
    let html = r#"<!doctype html><html><body style="margin:0">
        <div class="shell">
            <div class="sidebar" id="side"><p>Contents</p></div>
            <main class="main" id="content"><h1>Rust Article</h1><p>Body text</p></main>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext {
        viewport_width: 1000.0,
        viewport_height: 600.0,
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
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(1000.0, 600.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let side = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("side").unwrap()).unwrap();
    let main = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("content").unwrap()).unwrap();
    assert!(
        main.min_x() >= side.max_x() + 20.0,
        "grid-area: main ({main:?}) must sit to the right of sidebar ({side:?})"
    );
    assert!(
        (main.min_y() - side.min_y()).abs() < 4.0,
        "sidebar and main must align on the same grid row"
    );
}

#[test]
fn deep_14_level_nested_flex_grid_stays_linear_time() {
    // Regression test ensuring per-pass memoization prevents exponential O(3^D) blowup!
    let mut html = String::from(
        "<!doctype html><html><head><style>.f{display:flex;gap:4px}.g{display:grid;grid-template-columns:1fr 1fr;gap:4px}</style></head><body>",
    );
    for d in 0..14 {
        if d % 2 == 0 {
            html.push_str("<div class='f'><span>Item</span>");
        } else {
            html.push_str("<div class='g'><span>Col</span>");
        }
    }
    html.push_str("<div>Leaf content</div>");
    for _ in 0..14 {
        html.push_str("</div>");
    }
    html.push_str("</body></html>");

    let mut doc = parse_html(&html);
    let media = MediaContext {
        viewport_width: 1280.0,
        viewport_height: 800.0,
        ..MediaContext::default()
    };
    let mut styles = StyleContext::new(media);
    let css = ".f{display:flex;gap:4px}.g{display:grid;grid-template-columns:1fr 1fr;gap:4px}";
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let t0 = Instant::now();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(1280.0, 800.0), &font_db);
    let elapsed = t0.elapsed();
    assert!(tree.root.is_some());
    assert!(
        elapsed.as_millis() < 1500,
        "14-level nested flex+grid layout must stay linear, took {} ms",
        elapsed.as_millis()
    );
}

#[test]
fn transparent_dark_mode_button_does_not_paint_hardcoded_white_box() {
    let css = "body { background: #0d1117; } button { background-color: transparent; border: 0; color: #f0f6fc; }";
    let html = "<!doctype html><html><body><button id='nav'>Products</button></body></html>";

    let mut doc = parse_html(html);
    let media = MediaContext {
        dark_mode: true,
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
    let font_db = FontDatabase::new();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 600.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let has_hardcoded_light_button_rect = frame.display_list.items.iter().any(|item| {
        matches!(
            item,
            DisplayItem::Rect { color, .. }
                if *color == Rgba8::new(236, 238, 242, 255) || *color == Rgba8::new(255, 255, 255, 255)
        )
    });
    assert!(
        !has_hardcoded_light_button_rect,
        "transparent author-styled <button> must not paint a hardcoded white/gray fallback box"
    );
}

#[test]
fn flex_items_with_100pct_width_wrap_children_inside_resolved_flex_width() {
    // CSS Flexbox L1 §9.2 / §9.8: flex items with `width: 100%; flex: 1` must receive their
    // resolved definite flex width so descendant paragraphs wrap within each column instead of
    // expanding to 100% of the flex container width.
    let css = r#"
        body { margin: 0; }
        .row { display: flex; width: 900px; }
        .col { width: 100%; flex: 1 1 0%; padding: 0 10px; box-sizing: border-box; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div class="row">
            <div class="col" id="c1"><p id="p1">First column paragraph with enough words to wrap across multiple lines cleanly inside three hundred pixels.</p></div>
            <div class="col" id="c2"><p id="p2">Second column paragraph with enough words to wrap across multiple lines cleanly inside three hundred pixels.</p></div>
            <div class="col" id="c3"><p id="p3">Third column paragraph with enough words to wrap across multiple lines cleanly inside three hundred pixels.</p></div>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(1000.0, 600.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let c1 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("c1").unwrap()).unwrap();
    let c2 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("c2").unwrap()).unwrap();
    let p1 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("p1").unwrap()).unwrap();

    assert!(
        (c1.size.width - 300.0).abs() < 5.0,
        "flex column 1 should be ~300px wide, got {c1:?}"
    );
    assert!(
        c2.min_x() >= c1.max_x() - 1.0,
        "flex column 2 ({c2:?}) must sit to the right of column 1 ({c1:?})"
    );
    assert!(
        p1.size.width <= 300.0,
        "paragraph inside flex item must wrap within 300px, got {p1:?}"
    );
}

#[test]
fn overflow_hidden_clips_inline_text_in_non_stacking_block() {
    // CSS Overflow L3 / CSS 2.1 §11.1.1: `overflow: hidden` on a non-stacking block container
    // must push a clip rect around its inline-level descendants as well as its block backgrounds.
    let css = r#"
        body { margin: 0; }
        .clipped { width: 120px; height: 0px; padding-top: 60px; overflow: hidden; }
    "#;
    let html = r#"<!doctype html><html><body><div class="clipped">Hidden text below padding</div></body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 600.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let mut clip_depth = 0usize;
    let mut saw_clipped_text = false;
    for item in &frame.display_list.items {
        match item {
            DisplayItem::PushClip { rect, .. }
                if (rect.size.width - 120.0).abs() < 2.0
                    && (rect.size.height - 60.0).abs() < 2.0 =>
            {
                clip_depth += 1;
            }
            DisplayItem::PopClip => {
                clip_depth = clip_depth.saturating_sub(1);
            }
            DisplayItem::GlyphRun { .. } if clip_depth > 0 => {
                saw_clipped_text = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_clipped_text,
        "TextRun inside non-stacking overflow:hidden block must be painted inside a matching PushClip"
    );
}

#[test]
fn css21_clip_rect_emits_zero_area_clip_on_absolute_element() {
    // CSS 2.1 §11.1.2: `clip: rect(0 0 0 0)` on an absolutely positioned `.sr-only` element
    // emits a 0x0 clip rectangle around its stacking context.
    let css = r#"
        .sr-only {
            position: absolute;
            width: 1px;
            height: 1px;
            overflow: hidden;
            clip: rect(0, 0, 0, 0);
        }
    "#;
    let html = r##"<!doctype html><html><body><a class="sr-only" href="#main">Skip to content</a></body></html>"##;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 600.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let has_zero_clip = frame.display_list.items.iter().any(|item| {
        matches!(
            item,
            DisplayItem::PushClip { rect, .. }
                if rect.size.width <= 0.01 && rect.size.height <= 0.01
        )
    });
    assert!(
        has_zero_clip,
        "clip: rect(0, 0, 0, 0) on position:absolute element must emit a zero-sized PushClip"
    );
}

#[test]
fn mask_image_svg_paints_tinted_vector_instead_of_opaque_rect() {
    // CSS Masking L1: an icon span with `mask-image: url(...)` and `background-color` should
    // render its mask SVG vector tinted with background-color rather than a solid rectangle.
    let svg_markup = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" width="20" height="20"><circle cx="10" cy="10" r="8" fill="#000"/></svg>"##;
    let svg_doc = parse_html(svg_markup);
    let parsed_svg = extract_svg_from_document(&svg_doc, ColorRgba::BLACK).expect("parse mask svg");

    let css = r#"
        body { margin: 0; }
        .icon {
            display: inline-block;
            width: 20px;
            height: 20px;
            background-color: #112233;
            mask-image: url("/icon.svg");
        }
    "#;
    let html = r#"<!doctype html><html><body><span class="icon"></span></body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    engine.assets.register_svg("/icon.svg", parsed_svg);
    let font_db = FontDatabase::new();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 600.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let tint = Rgba8::new(0x11, 0x22, 0x33, 255);
    let has_tinted_vector =
        frame.display_list.items.iter().any(
            |item| matches!(item, DisplayItem::VectorPath { fill: Some(c), .. } if *c == tint),
        );
    let has_unmasked_solid_rect = frame
        .display_list
        .items
        .iter()
        .any(|item| matches!(item, DisplayItem::Rect { color, .. } if *color == tint));
    assert!(
        has_tinted_vector,
        "mask-image: url() must paint the mask SVG tinted with background-color"
    );
    assert!(
        !has_unmasked_solid_rect,
        "mask-image: url() must not paint an unmasked solid background rectangle"
    );
}

#[test]
fn svg_background_image_paints_vector_paths() {
    let svg_markup = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 40" width="100" height="40"><rect x="0" y="0" width="100" height="40" fill="#aa1122"/></svg>"##;
    let svg_doc = parse_html(svg_markup);
    let parsed_svg = extract_svg_from_document(&svg_doc, ColorRgba::BLACK).expect("parse bg svg");

    let css = r#"
        body { margin: 0; }
        .banner {
            width: 200px;
            height: 80px;
            background-image: url("/banner.svg");
        }
    "#;
    let html = r#"<!doctype html><html><body><div class="banner"></div></body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    engine.assets.register_svg("/banner.svg", parsed_svg);
    let font_db = FontDatabase::new();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 600.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let has_bg_vector = frame.display_list.items.iter().any(|item| {
        matches!(
            item,
            DisplayItem::VectorPath {
                fill: Some(c),
                ..
            } if *c == Rgba8::new(0xaa, 0x11, 0x22, 255)
        )
    });
    assert!(
        has_bg_vector,
        "background-image: url(*.svg) must render decoded SVG vector paths"
    );
}

#[test]
fn block_level_replaced_svg_centers_with_margin_auto() {
    // CSS 2.1 §10.3.4: `<svg style="display: block; margin: 0 auto;">` must center horizontally.
    let html = r##"<!doctype html><html><body style="margin:0">
        <div style="width: 600px;">
            <svg id="hero" style="display: block; margin: 0 auto;" width="200" height="100" viewBox="0 0 200 100">
                <rect x="0" y="0" width="200" height="100" fill="#000"/>
            </svg>
        </div>
    </body></html>"##;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(800.0, 600.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let hero = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("hero").unwrap()).unwrap();
    assert!(
        (hero.min_x() - 200.0).abs() < 2.0,
        "block-level SVG with margin: 0 auto inside 600px container should start at x=200, got {hero:?}"
    );
}

#[test]
fn html5_ua_hides_closed_popover_dialog_and_details_body() {
    // HTML5 §15.3 UA defaults: closed `[popover]`, closed `<dialog>`, and non-summary children of
    // closed `<details>` must compute to `display: none`.
    let html = r#"<!doctype html><html><body>
        <div id="pop" popover>Hidden popover</div>
        <dialog id="dlg">Hidden dialog</dialog>
        <details id="det"><summary id="sum">Toggle</summary><p id="body">Hidden details body</p></details>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(800.0, 600.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    assert!(
        find_abs_rect(root, Point::ZERO, doc.get_element_by_id("pop").unwrap()).is_none(),
        "closed [popover] must be display: none"
    );
    assert!(
        find_abs_rect(root, Point::ZERO, doc.get_element_by_id("dlg").unwrap()).is_none(),
        "closed <dialog> must be display: none"
    );
    assert!(
        find_abs_rect(root, Point::ZERO, doc.get_element_by_id("sum").unwrap()).is_some(),
        "<summary> inside closed <details> must remain visible"
    );
    assert!(
        find_abs_rect(root, Point::ZERO, doc.get_element_by_id("body").unwrap()).is_none(),
        "non-summary content inside closed <details> must be display: none"
    );
}

#[test]
fn bfc_clear_both_does_not_indent_lines_by_cleared_float() {
    // CSS 2.1 §9.5.2: a non-BFC block with `clear: both` must clear preceding floats BEFORE
    // laying out its own inline lines so text is not indented by the cleared float's width.
    let css = r#"
        body { margin: 0; }
        .fl { float: left; width: 140px; height: 30px; }
        .fr { float: right; width: 200px; height: 30px; }
        .cleared { clear: both; }
        .item { display: inline-block; width: 100px; height: 20px; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div class="fl">Left</div>
        <div class="fr">Right</div>
        <div class="cleared" id="nav">
            <span class="item" id="i1">One</span>
            <span class="item" id="i2">Two</span>
            <span class="item" id="i3">Three</span>
            <span class="item" id="i4">Four</span>
            <span class="item" id="i5">Five</span>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(700.0, 400.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let i1 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("i1").unwrap()).unwrap();
    let i5 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("i5").unwrap()).unwrap();
    assert!(
        i1.min_x() < 5.0,
        "cleared block's first item must start at x=0 (not indented past left float), got {i1:?}"
    );
    assert!(
        i1.min_y() >= 29.0,
        "cleared block must sit below 30px floats, got y={}",
        i1.min_y()
    );
    assert!(
        (i5.min_y() - i1.min_y()).abs() < 2.0,
        "all 5 items (500px total) must stay on one row inside 700px cleared block, got i1={i1:?} i5={i5:?}"
    );
}

#[test]
fn ifc_atomic_inline_drops_below_narrowing_float_even_when_wider_than_container() {
    // CSS 2.1 §9.5: an atomic inline (`display: inline-block`) inside a narrow container whose
    // current band is narrowed by a float must shift below the float to x=0 rather than sitting
    // beside the float and overflowing into adjacent columns.
    let css = r#"
        body { margin: 0; }
        #sidebar { width: 150px; }
        .logo { float: left; width: 110px; height: 60px; }
        .navbox { display: inline-block; width: 160px; height: 40px; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div id="sidebar">
            <div class="logo">Logo</div>
            <div class="navbox" id="nav">Menu</div>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(800.0, 400.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let nav = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("nav").unwrap()).unwrap();
    assert!(
        nav.min_x() < 5.0 && nav.min_y() >= 59.0,
        "inline-block wider than narrowed band must drop below 60px float to x=0, got {nav:?}"
    );
}

#[test]
fn flex_auto_sized_item_with_100pct_child_does_not_inflate_to_viewport() {
    // CSS Sizing L3 §5.2.1: an `inline-flex` item with `flex: none` containing an `inline-flex`
    // anchor with `width: 100%` must size to the anchor's content width, not 100% of viewport.
    let css = r#"
        body { margin: 0; }
        .menu { display: flex; justify-content: flex-end; width: 800px; }
        .item { display: inline-flex; flex: none; margin: 0 5px; }
        .item > a { display: inline-flex; width: 100%; padding: 0 12px; }
    "#;
    let html = r##"<!doctype html><html><body>
        <div class="menu">
            <div class="item" id="m1"><a href="#">Alpha</a></div>
            <div class="item" id="m2"><a href="#">Beta</a></div>
            <div class="item" id="m3"><a href="#">Gamma</a></div>
        </div>
    </body></html>"##;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(800.0, 400.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let m1 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("m1").unwrap()).unwrap();
    let m3 = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("m3").unwrap()).unwrap();
    assert!(
        m1.size.width < 200.0,
        "auto-sized flex item with width:100% flex child must not inflate to 800px, got {m1:?}"
    );
    assert!(
        m1.min_x() >= 0.0 && m3.max_x() <= 801.0,
        "right-aligned flex menu items must stay inside 800px viewport, got m1={m1:?} m3={m3:?}"
    );
}

#[test]
fn flex_wrap_row_with_compressible_max_width_percent_img_stays_side_by_side() {
    // CSS Sizing L3 §5.2.1: a replaced `<img>` with `max-width: 96%` inside a `flex: 1; width: 50%`
    // column in a `flex-wrap: wrap` row has 0 min-content contribution so two columns fit side-by-side.
    let css = r#"
        body { margin: 0; }
        .row { display: flex; flex-direction: row; flex-wrap: wrap; width: 1000px; }
        .col { display: flex; flex-direction: column; flex: 1; padding: 20px; width: 50%; }
        .col img { max-width: 96%; height: auto; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div class="row">
            <div class="col" id="left"><img src="/wide.png" width="700" height="300"></div>
            <div class="col" id="right"><p>Right column</p></div>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let font_db = FontDatabase::new();
    let (tree, _) = engine.layout_only(&doc, &styles, Size::new(1000.0, 600.0), &font_db);
    let root = tree.root.as_ref().unwrap();

    let left = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("left").unwrap()).unwrap();
    let right = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("right").unwrap()).unwrap();
    assert!(
        (left.min_y() - right.min_y()).abs() < 2.0 && right.min_x() >= left.max_x() - 1.0,
        "two flex:1 columns with max-width:96% img must stay side by side, got left={left:?} right={right:?}"
    );
}

#[test]
fn button_renders_dom_children_and_honors_clipped_sr_only_span() {
    // HTML5 §15.5.4: `<button>` is a container box whose DOM children receive full CSS styling
    // (such as `.sr-only` `clip: rect(1px,1px,1px,1px)` hiding accessible text inside icon buttons).
    let css = r#"
        .icon-btn { width: 24px; height: 24px; padding: 0; }
        .icon-btn span + span {
            display: block;
            position: absolute;
            clip: rect(1px, 1px, 1px, 1px);
            width: 1px;
            height: 1px;
            overflow: hidden;
        }
    "#;
    let html = r#"<!doctype html><html><body>
        <button class="icon-btn" id="btn">
            <span class="ico"></span>
            <span id="sr">Toggle subsection</span>
        </button>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let mut font_db = FontDatabase::new();
    font_db.scan_system_fonts();
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        Size::new(800.0, 400.0),
        &font_db,
        PipelineId(1),
        &mut resources,
    );

    let root = frame.fragment_tree.root.as_ref().unwrap();
    let sr = find_abs_rect(root, Point::ZERO, doc.get_element_by_id("sr").unwrap());
    assert!(
        sr.is_some(),
        "<button> must lay out its child <span> elements instead of flattening to a leaf FormControl"
    );
    let has_zero_clip = frame.display_list.items.iter().any(|item| {
        matches!(
            item,
            DisplayItem::PushClip { rect, .. }
                if rect.size.width <= 0.01 && rect.size.height <= 0.01
        )
    });
    assert!(
        has_zero_clip,
        "child sr-only <span> inside <button> must emit zero-area clip: rect(1px,1px,1px,1px)"
    );
}

#[test]
fn svg_extractor_skips_defs_clippath_and_applies_group_transforms() {
    // SVG 2: non-rendering containers (`<defs>`, `<clipPath>`, `<mask>`) must not emit visible
    // paths, `<g transform="...">` must accumulate onto descendant paths, and `<use href="#id">`
    // must instantiate referenced definitions.
    let svg_markup = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="100" height="100">
        <defs>
            <clipPath id="cp"><rect x="0" y="0" width="100" height="100" fill="#000"/></clipPath>
            <linearGradient id="g1"><stop offset="0%" stop-color="#1188ff"/></linearGradient>
            <rect id="dot" x="0" y="0" width="10" height="10"/>
        </defs>
        <g transform="translate(20, 30)" fill="url(#g1)">
            <rect x="0" y="0" width="20" height="20"/>
        </g>
        <use href="#dot" x="50" y="10" fill="#ff0000"/>
    </svg>"##;
    let svg_doc = parse_html(svg_markup);
    let decoded = extract_svg_from_document(&svg_doc, ColorRgba::BLACK).expect("extract svg");
    assert_eq!(
        decoded.paths.len(),
        2,
        "must emit only the visible transformed rect and <use> instance (skipping <defs>/<clipPath>), got {}",
        decoded.paths.len()
    );
    assert_eq!(
        decoded.paths[0].fill,
        Some(ColorRgba::rgb(0x11, 0x88, 0xff)),
        "url(#g1) fill must resolve to gradient first stop color instead of black"
    );
    assert!(
        (decoded.paths[0].transform[4] - 20.0).abs() < 0.01
            && (decoded.paths[0].transform[5] - 30.0).abs() < 0.01,
        "group translate(20, 30) must accumulate onto child path transform, got {:?}",
        decoded.paths[0].transform
    );
    assert!(
        (decoded.paths[1].transform[4] - 50.0).abs() < 0.01
            && (decoded.paths[1].transform[5] - 10.0).abs() < 0.01,
        "<use x=50 y=10> must translate instantiated def, got {:?}",
        decoded.paths[1].transform
    );
}
