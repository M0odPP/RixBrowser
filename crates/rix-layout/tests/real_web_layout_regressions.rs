use std::sync::Arc;
use std::time::Instant;

use rix_core::{Point, Rect, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::NodeId;
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::{Fragment, LayoutEngine};
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
    let font_db = FontDatabase::new();
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
    let font_db = FontDatabase::new();
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
