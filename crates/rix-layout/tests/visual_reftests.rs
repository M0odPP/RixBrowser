use std::sync::{Arc, Mutex, OnceLock};

use image::{ImageBuffer, ImageFormat, Rgba};
use rix_core::{Point, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::{hit_test, LayoutEngine};
use rix_render::{encode_display_list, EncodeOptions, ResourceRegistry, VelloRenderer};
use rix_text::FontDatabase;
use vello::peniko::Color;
use vello::Scene;

const CANVAS_BG: Color = Color::from_rgba8(245, 246, 248, 255);

fn shared_fonts() -> &'static FontDatabase {
    static DB: OnceLock<FontDatabase> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = FontDatabase::new();
        db.scan_system_fonts();
        db
    })
}

fn render_html_to_rgba(
    html: &str,
    css: &str,
    width: u32,
    height: u32,
    setup: impl FnOnce(&mut LayoutEngine),
) -> (Vec<u8>, rix_layout::LayoutFrame) {
    let viewport = Size::new(width as f32, height as f32);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(html);
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    setup(&mut engine);

    let pipeline = PipelineId(1);
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        viewport,
        shared_fonts(),
        pipeline,
        &mut resources,
    );

    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        &frame.display_list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );

    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut renderer = VelloRenderer::new();
    let pixels = renderer
        .render_to_rgba_blocking(&scene, width, height, CANVAS_BG)
        .expect("headless GPU render succeeds");
    (pixels, frame)
}

#[inline]
fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

#[inline]
fn approx_rgb(px: [u8; 4], expected: [u8; 3]) -> bool {
    (i16::from(px[0]) - i16::from(expected[0])).abs() <= 14
        && (i16::from(px[1]) - i16::from(expected[1])).abs() <= 14
        && (i16::from(px[2]) - i16::from(expected[2])).abs() <= 14
}

#[test]
fn gpu_reftest_collapsing_margins_floats_and_clear() {
    let html = r#"
        <html><body>
            <div id="top"></div>
            <div id="bot"></div>
            <div id="fl"></div>
            <div id="fr"></div>
            <div id="cl"></div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; }
        #top { height: 20px; margin-bottom: 30px; background: #dc2626; }
        #bot { height: 20px; margin-top: 20px; background: #16a34a; }
        #fl { float: left; width: 80px; height: 60px; background: #2563eb; }
        #fr { float: right; width: 80px; height: 40px; background: #d97706; }
        #cl { clear: both; height: 30px; background: #7c3aed; }
    "#;
    let (px, _) = render_html_to_rgba(html, css, 320, 200, |_| {});
    // #top occupies y=0..20
    assert!(approx_rgb(pixel(&px, 320, 40, 10), [220, 38, 38]));
    // Collapsed 30px margin gap at y=20..50 stays canvas background (NOT 50px!)
    assert!(approx_rgb(pixel(&px, 320, 40, 35), [245, 246, 248]));
    // #bot occupies y=50..70
    assert!(approx_rgb(pixel(&px, 320, 40, 60), [22, 163, 74]));
    // #fl occupies x=0..80, y=70..130; #fr occupies x=240..320, y=70..110
    assert!(approx_rgb(pixel(&px, 320, 40, 100), [37, 99, 235]));
    assert!(approx_rgb(pixel(&px, 320, 280, 90), [217, 119, 6]));
    // #cl clears both floats -> starts at y=130..160
    assert!(approx_rgb(pixel(&px, 320, 160, 145), [124, 58, 237]));
}

#[test]
fn gpu_reftest_multiline_inline_boxes_table_flex_and_grid() {
    let html = r#"
        <html><body>
            <div id="wrap"><span id="pill">one two three four five six</span></div>
            <table id="tbl">
                <tr>
                    <td id="a" colspan="2"></td>
                    <td id="b" rowspan="2"></td>
                </tr>
                <tr>
                    <td id="c"></td>
                    <td id="d"></td>
                </tr>
            </table>
            <div id="flex"><div id="f1"></div><div id="f2"></div></div>
            <div id="grid"><div id="g1"></div><div id="g2"></div></div>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; font-size: 14px; }
        #wrap { width: 120px; height: 48px; }
        #pill { background: #fde047; padding-left: 6px; padding-right: 6px; }
        #tbl { width: 240px; border-collapse: collapse; table-layout: fixed; }
        td { height: 24px; }
        #a { background: #ef4444; }
        #b { background: #22c55e; }
        #c { background: #3b82f6; }
        #d { background: #f97316; }
        #flex { display: flex; width: 240px; height: 24px; }
        #f1 { flex-grow: 1; background: #06b6d4; }
        #f2 { flex-grow: 1; background: #a855f7; }
        #grid { display: grid; width: 240px; grid-template-columns: 80px 1fr; height: 24px; }
        #g1 { background: #ec4899; }
        #g2 { background: #14b8a6; }
    "#;
    let (px, _) = render_html_to_rgba(html, css, 300, 180, |_| {});
    // Table starts at y=48: #a (0..160, 48..72), #b (160..240, 48..96), #c (0..80, 72..96), #d (80..160, 72..96)
    assert!(approx_rgb(pixel(&px, 300, 80, 58), [239, 68, 68]));
    assert!(approx_rgb(pixel(&px, 300, 200, 82), [34, 197, 94]));
    assert!(approx_rgb(pixel(&px, 300, 40, 82), [59, 130, 246]));
    assert!(approx_rgb(pixel(&px, 300, 120, 82), [249, 115, 22]));
    // Flex row at y=96..120: f1 (0..120), f2 (120..240)
    assert!(approx_rgb(pixel(&px, 300, 60, 108), [6, 182, 212]));
    assert!(approx_rgb(pixel(&px, 300, 180, 108), [168, 85, 247]));
    // Grid row at y=120..144: g1 (0..80), g2 (80..240)
    assert!(approx_rgb(pixel(&px, 300, 40, 132), [236, 72, 153]));
    assert!(approx_rgb(pixel(&px, 300, 160, 132), [20, 184, 166]));
}

#[test]
fn gpu_reftest_decoded_png_image_inline_svg_and_nested_z_index_isolation() {
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(40, 40, Rgba([16, 185, 129, 255]));
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .expect("encode png");

    let html = r##"
        <html><body>
            <img id="pic" src="tile.png" width="40" height="40">
            <svg id="vec" width="40" height="40" viewBox="0 0 40 40">
                <rect x="0" y="0" width="40" height="40" fill="#0ea5e9"></rect>
            </svg>
            <div id="sc-low">
                <div id="trapped-high"></div>
            </div>
            <div id="sc-mid"></div>
            <div id="sc-neg"></div>
        </body></html>
    "##;
    let css = r#"
        body { margin: 0; }
        #pic { position: absolute; left: 10px; top: 10px; }
        #vec { position: absolute; left: 60px; top: 10px; }
        /* Stacking context isolation: #trapped-high (z:999 inside z:1) MUST paint BEHIND #sc-mid (z:2)! */
        #sc-low { position: absolute; left: 120px; top: 10px; width: 60px; height: 60px; z-index: 1; background: #facc15; }
        #trapped-high { position: absolute; left: 15px; top: 15px; width: 60px; height: 60px; z-index: 999; background: #ef4444; }
        #sc-mid { position: absolute; left: 145px; top: 35px; width: 60px; height: 60px; z-index: 2; background: #2563eb; }
        #sc-neg { position: absolute; left: 10px; top: 70px; width: 50px; height: 50px; z-index: -1; background: #9333ea; }
    "#;
    let (px, frame) = render_html_to_rgba(html, css, 240, 140, |engine| {
        engine
            .assets
            .register_encoded("tile.png", &png)
            .expect("decode png");
    });

    // Decoded PNG pixels rendered at (30, 30)
    assert!(
        approx_rgb(pixel(&px, 240, 30, 30), [16, 185, 129]),
        "decoded PNG pixel={:?}",
        pixel(&px, 240, 30, 30)
    );
    // Inline SVG rect rendered at (80, 30)
    assert!(
        approx_rgb(pixel(&px, 240, 80, 30), [14, 165, 233]),
        "inline SVG pixel={:?}",
        pixel(&px, 240, 80, 30)
    );
    // Overlap between #trapped-high (z:999 inside z:1) and #sc-mid (z:2) at (160, 50) MUST be blue (#sc-mid)!
    assert!(
        approx_rgb(pixel(&px, 240, 160, 50), [37, 99, 235]),
        "nested z-index isolation failed; got {:?}",
        pixel(&px, 240, 160, 50)
    );
    // Non-overlapping corner of #trapped-high at (140, 30) is red
    assert!(
        approx_rgb(pixel(&px, 240, 140, 30), [239, 68, 68]),
        "trapped child visible corner got {:?}",
        pixel(&px, 240, 140, 30)
    );
    // Hit-test verifies interactive point resolution works on the painted FragmentTree.
    assert!(hit_test(&frame.fragment_tree, Point::new(30.0, 30.0)).is_some());
}

#[test]
fn realistic_wiki_mdn_hn_multi_section_layout_has_zero_overlaps() {
    // Representative structure combining HN-style header + list, Wikipedia infobox float + table, and MDN 2-col grid.
    let html = r#"
        <html><body>
            <header id="nav">
                <div id="brand">RixDocs</div>
                <div id="links">Guide Reference News</div>
            </header>
            <main id="shell">
                <aside id="sidebar">
                    <div id="s1">Overview</div>
                    <div id="s2">Architecture</div>
                </aside>
                <article id="content">
                    <div id="infobox">Infobox Summary</div>
                    <p id="lead">RixBrowser uses a true CSS formatting context layout engine with BFC, IFC, Table, Flexbox, and Grid.</p>
                    <table id="stats">
                        <tr><th>Engine</th><th>Budget</th></tr>
                        <tr><td>Cascade</td><td>10ms</td></tr>
                        <tr><td>Layout</td><td>5ms</td></tr>
                    </table>
                </article>
            </main>
        </body></html>
    "#;
    let css = r#"
        body { margin: 0; font-size: 15px; background: #ffffff; color: #111827; }
        #nav { display: flex; justify-content: space-between; align-items: center; height: 48px; padding: 0 16px; background: #1e293b; color: #f8fafc; }
        #shell { display: grid; grid-template-columns: 200px 1fr; column-gap: 24px; padding: 16px; }
        #sidebar { background: #f1f5f9; padding: 12px; }
        #infobox { float: right; width: 180px; height: 90px; margin-left: 16px; background: #e2e8f0; padding: 8px; }
        #stats { clear: both; width: 100%; border-collapse: collapse; margin-top: 12px; }
        th, td { border: 1px solid #cbd5e1; padding: 6px; }
    "#;
    let (px, frame) = render_html_to_rgba(html, css, 960, 420, |_| {});
    assert!(frame.metrics.display_items >= 15);
    // Dark header bar rendered at top
    let nav_px = pixel(&px, 960, 480, 24);
    assert!(approx_rgb(nav_px, [30, 41, 59]), "nav={nav_px:?}");
    // Sidebar occupies left column below nav (sample clear area right of text)
    let side_px = pixel(&px, 960, 190, 95);
    assert!(approx_rgb(side_px, [241, 245, 249]), "sidebar={side_px:?}");
    // Right-floated infobox occupies top-right of article column (sample clear area below title)
    let box_px = pixel(&px, 960, 910, 125);
    assert!(approx_rgb(box_px, [226, 232, 240]), "infobox={box_px:?}");
}
