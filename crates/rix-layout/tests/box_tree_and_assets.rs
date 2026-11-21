use std::sync::Arc;

use image::{ImageBuffer, ImageFormat, Rgba};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_html::parse_html;
use rix_layout::{AssetRegistry, BoxKind, BoxTree, ReplacedKind};

#[test]
fn asset_registry_decodes_png_and_populates_replaced_image_box() {
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(24, 12, Rgba([20, 120, 220, 255]));
    let mut png_bytes = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png_bytes), ImageFormat::Png)
        .expect("encode png");

    let mut assets = AssetRegistry::new();
    let id = assets
        .register_encoded("hero.png", &png_bytes)
        .expect("decode png");
    assert_eq!(assets.len(), 1);
    let decoded = assets.get_by_id(id).expect("stored");
    assert_eq!((decoded.width, decoded.height), (24, 12));

    let mut doc = parse_html(r#"<html><body><img id="pic" src="hero.png"></body></html>"#);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.cascade_full(&mut doc);

    let tree = BoxTree::build(&doc, &styles, &assets);
    let pic_node = doc.get_element_by_id("pic").unwrap();
    let box_id = tree.dom_to_box[&pic_node];
    match &tree.get(box_id).kind {
        BoxKind::Replaced(ReplacedKind::Image {
            image_id,
            intrinsic_size,
            ..
        }) => {
            assert_eq!(*image_id, Some(id));
            assert_eq!((intrinsic_size.width, intrinsic_size.height), (24.0, 12.0));
        }
        other => panic!("expected replaced image box, got {other:?}"),
    }
}

#[test]
fn box_tree_handles_display_none_contents_anonymous_blocks_tables_and_svg() {
    let html = r##"
        <html><body>
            <div id="hidden">Hidden</div>
            <div id="ghost"><p id="p1">Inside</p></div>
            <div id="mixed">inline text<div id="blk">block</div></div>
            <div id="stray-cells">
                <span id="c1">A</span>
                <span id="c2">B</span>
            </div>
            <svg id="icon" width="16" height="16" viewBox="0 0 16 16">
                <path d="M 2 2 L 14 14" stroke="#112233" stroke-width="2" fill="none"></path>
            </svg>
        </body></html>
    "##;
    let css = r#"
        #hidden { display: none; }
        #ghost { display: contents; }
        #c1, #c2 { display: table-cell; }
    "#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let assets = AssetRegistry::new();
    let tree = BoxTree::build(&doc, &styles, &assets);

    // `display: none` pruned; `display: contents` flattened.
    let hidden = doc.get_element_by_id("hidden").unwrap();
    let ghost = doc.get_element_by_id("ghost").unwrap();
    let p1 = doc.get_element_by_id("p1").unwrap();
    assert!(!tree.dom_to_box.contains_key(&hidden));
    assert!(!tree.dom_to_box.contains_key(&ghost));
    assert!(tree.dom_to_box.contains_key(&p1));

    // Mixed block + inline children produce an anonymous block wrapper for inline text.
    let mixed = doc.get_element_by_id("mixed").unwrap();
    let mixed_box = tree.get(tree.dom_to_box[&mixed]);
    assert_eq!(mixed_box.children.len(), 2);
    assert!(matches!(
        tree.get(mixed_box.children[0]).kind,
        BoxKind::AnonymousBlock
    ));

    // Stray `display: table-cell` children coalesce into one anonymous table -> anonymous row.
    let stray = doc.get_element_by_id("stray-cells").unwrap();
    let stray_box = tree.get(tree.dom_to_box[&stray]);
    assert_eq!(stray_box.children.len(), 1);
    let anon_tbl = tree.get(stray_box.children[0]);
    assert!(matches!(anon_tbl.kind, BoxKind::AnonymousTable));
    let anon_row = tree.get(anon_tbl.children[0]);
    assert!(matches!(anon_row.kind, BoxKind::AnonymousTableRow));
    assert_eq!(anon_row.children.len(), 2);

    // Inline `<svg>` extracts vector paths + stroke attributes.
    let icon = doc.get_element_by_id("icon").unwrap();
    match &tree.get(tree.dom_to_box[&icon]).kind {
        BoxKind::Replaced(ReplacedKind::Svg {
            paths,
            intrinsic_size,
            ..
        }) => {
            assert_eq!((intrinsic_size.width, intrinsic_size.height), (16.0, 16.0));
            assert_eq!(paths.len(), 1);
            assert!(paths[0].stroke.is_some());
        }
        other => panic!("expected replaced svg box, got {other:?}"),
    }
}
