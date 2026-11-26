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
