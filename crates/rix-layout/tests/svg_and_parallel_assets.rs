use rix_core::Size;
use rix_css::{ColorRgba, MediaContext, StyleContext};
use rix_html::parse_html;
use rix_layout::{
    decode_raster_batch_parallel, extract_svg_from_document, parse_best_srcset_url, BoxKind,
    BoxTree, LayoutEngine, ReplacedKind,
};
use rix_text::FontDatabase;

fn make_tiny_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(4, 4);
    for px in img.pixels_mut() {
        *px = image::Rgba(rgba);
    }
    let mut out = Vec::new();
    let dyn_img = image::DynamicImage::ImageRgba8(img);
    dyn_img
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode tiny png");
    out
}

#[test]
fn srcset_selector_picks_best_candidate() {
    let srcset = "/img-400.png 400w, /img-1200.png 1200w, /img-800.png 800w";
    assert_eq!(
        parse_best_srcset_url(srcset).as_deref(),
        Some("/img-1200.png")
    );

    let dpr_srcset = "/icon-1x.png 1x, /icon-2x.png 2x";
    assert_eq!(
        parse_best_srcset_url(dpr_srcset).as_deref(),
        Some("/icon-2x.png")
    );
}

#[test]
fn parallel_raster_decode_and_svg_in_img_render() {
    let png_a = make_tiny_png([255, 0, 0, 255]);
    let png_b = make_tiny_png([0, 128, 255, 255]);
    let decoded = decode_raster_batch_parallel(&[
        ("https://example.com/a.png".to_string(), png_a),
        ("https://example.com/b.png".to_string(), png_b),
    ]);
    assert_eq!(decoded.len(), 2);
    let first = decoded[0].1.as_ref().expect("decode png_a");
    let second = decoded[1].1.as_ref().expect("decode png_b");
    assert_eq!(first.width, 4);
    assert_eq!(first.height, 4);
    assert_eq!(second.rgba.len(), 4 * 4 * 4);

    let svg_markup = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="48" height="48"><rect x="2" y="2" width="20" height="20" fill="#2563eb"/><circle cx="12" cy="12" r="6" fill="#ffffff"/></svg>"##;
    let svg_doc = parse_html(svg_markup);
    let parsed_svg =
        extract_svg_from_document(&svg_doc, ColorRgba::BLACK).expect("extract svg paths");
    assert_eq!(parsed_svg.paths.len(), 2);
    assert!((parsed_svg.intrinsic_size.width - 48.0).abs() < 0.01);

    let mut engine = LayoutEngine::new();
    engine
        .assets
        .register_svg("https://example.com/logo.svg", parsed_svg);
    engine
        .assets
        .register_alias("/logo.svg", "https://example.com/logo.svg");

    let mut doc =
        parse_html(r#"<!doctype html><html><body><img src="/logo.svg" alt="Logo"></body></html>"#);
    let mut style_ctx = StyleContext::new(MediaContext::default());
    style_ctx.restyle_dirty(&mut doc);

    let box_tree = BoxTree::build(&doc, &style_ctx, &engine.assets);
    let mut has_svg_replaced = false;
    for b in &box_tree.boxes {
        if let BoxKind::Replaced(ReplacedKind::Svg { paths, .. }) = &b.kind {
            assert_eq!(paths.len(), 2);
            has_svg_replaced = true;
        }
    }
    assert!(
        has_svg_replaced,
        "<img src='/logo.svg'> must resolve to ReplacedKind::Svg"
    );

    let font_db = FontDatabase::new();
    let (frag, _metrics) = engine.layout_only(&doc, &style_ctx, Size::new(800.0, 600.0), &font_db);
    assert!(frag.content_bounds.size.height >= 48.0);
}
