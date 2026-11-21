use std::sync::Arc;

use rix_core::{Point, Rect, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::Document;
use rix_html::parse_html;
use rix_layout::{layout_tree, AssetRegistry, BoxTree, Fragment, LayoutCache};
use rix_text::{shared_shaping_cache, FontDatabase};

fn test_fonts() -> FontDatabase {
    let mut db = FontDatabase::new();
    db.scan_system_fonts();
    db
}

fn layout_html(html: &str, css: &str, viewport: Size) -> (Document, Fragment) {
    let mut doc = parse_html(html);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
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
    (doc, ftree.root.expect("root fragment"))
}

fn find_abs_rect(doc: &Document, root: &Fragment, id: &str) -> Rect {
    let target = doc.get_element_by_id(id).expect("element id exists");
    fn walk(frag: &Fragment, target: rix_dom::NodeId, origin: Point) -> Option<Rect> {
        let abs = frag.absolute_border_rect(origin);
        if frag.dom_node == Some(target) {
            return Some(abs);
        }
        for ch in &frag.children {
            if let Some(found) = walk(ch, target, abs.origin) {
                return Some(found);
            }
        }
        None
    }
    walk(root, target, Point::ZERO).expect("fragment exists for element")
}

#[test]
fn table_layout_colspan_rowspan_border_collapse_and_anonymous_cells() {
    let html = r#"
        <html><body>
            <table id="tbl">
                <tr>
                    <td id="c00" colspan="2">SpanTwo</td>
                    <td id="c02" rowspan="2">Tall</td>
                </tr>
                <tr>
                    <td id="c10">Left</td>
                    <td id="c11">Mid</td>
                </tr>
            </table>
            <div id="anon">
                <span id="tc1">CellA</span>
                <span id="tc2">CellB</span>
            </div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; }
        #tbl { width: 300px; border-collapse: collapse; table-layout: fixed; }
        td { border: 2px solid #333; height: 40px; }
        #tc1, #tc2 { display: table-cell; width: 100px; height: 25px; }
    "#;
    let (doc, root) = layout_html(html, css, Size::new(800.0, 600.0));
    let c00 = find_abs_rect(&doc, &root, "c00");
    let c02 = find_abs_rect(&doc, &root, "c02");
    let c10 = find_abs_rect(&doc, &root, "c10");
    let c11 = find_abs_rect(&doc, &root, "c11");

    assert!((c00.size.width - 200.0).abs() < 1.5);
    assert!((c10.size.width - 100.0).abs() < 1.5);
    assert!((c11.size.width - 100.0).abs() < 1.5);
    assert!((c11.min_x() - c10.max_x()).abs() < 1.0);
    assert!((c02.min_x() - c00.max_x()).abs() < 1.0);
    assert!(c02.size.height >= c10.size.height * 1.9);

    let tc1 = find_abs_rect(&doc, &root, "tc1");
    let tc2 = find_abs_rect(&doc, &root, "tc2");
    assert!((tc2.min_x() - tc1.max_x()).abs() < 1.0);
    assert!((tc1.min_y() - tc2.min_y()).abs() < 1.0);
}

#[test]
fn retained_taffy_flexbox_and_grid_layouts() {
    let html = r#"
        <html><body>
            <div id="flex">
                <div id="f1"></div>
                <div id="f2"></div>
            </div>
            <div id="grid">
                <div id="g1"></div>
                <div id="g2"></div>
                <div id="g3"></div>
            </div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; }
        #flex { display: flex; width: 320px; gap: 20px; }
        #f1 { flex-grow: 1; height: 40px; }
        #f2 { flex-grow: 2; height: 40px; }
        #grid { display: grid; width: 420px; grid-template-columns: 100px 1fr 2fr; column-gap: 10px; }
        #g1, #g2, #g3 { height: 30px; }
    "#;
    let (doc, root) = layout_html(html, css, Size::new(800.0, 600.0));
    let f1 = find_abs_rect(&doc, &root, "f1");
    let f2 = find_abs_rect(&doc, &root, "f2");
    assert!((f1.size.width - 100.0).abs() < 1.5);
    assert!((f2.size.width - 200.0).abs() < 1.5);
    assert!((f2.min_x() - f1.max_x() - 20.0).abs() < 1.5);

    let g1 = find_abs_rect(&doc, &root, "g1");
    let g2 = find_abs_rect(&doc, &root, "g2");
    let g3 = find_abs_rect(&doc, &root, "g3");
    assert!((g1.size.width - 100.0).abs() < 1.5);
    assert!((g2.size.width - 100.0).abs() < 1.5);
    assert!((g3.size.width - 200.0).abs() < 1.5);
}
