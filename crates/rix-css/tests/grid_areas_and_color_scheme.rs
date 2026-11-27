use std::sync::Arc;

use rix_css::{CascadeOrigin, ColorRgba, MediaContext, StyleContext, Stylesheet};
use rix_html::parse_html;

#[test]
fn color_scheme_light_dark_adapts_root_canvas_text_and_background() {
    let css = r#"
        html {
            color-scheme: light dark;
            background: light-dark(#eeeeee, #222222);
        }
    "#;
    let html = "<!doctype html><html><body><p id='msg'>Hello</p></body></html>";

    let dark_media = MediaContext {
        dark_mode: true,
        ..MediaContext::default()
    };
    let mut doc_dark = parse_html(html);
    let mut ctx_dark = StyleContext::new(dark_media);
    ctx_dark.add_stylesheet(Stylesheet::parse_cached(
        css,
        CascadeOrigin::Author,
        &dark_media,
    ));
    ctx_dark.cascade_full(&mut doc_dark);

    let html_node = doc_dark
        .descendants(doc_dark.root())
        .find(|&n| {
            matches!(
                doc_dark.get(n),
                Some(rix_dom::NodeData::Element(e)) if e.name.local.eq_str_ignore_ascii_case("html")
            )
        })
        .expect("html node");
    let p_node = doc_dark.get_element_by_id("msg").expect("msg node");

    let html_st = ctx_dark.style_for(html_node).expect("html style");
    assert!(html_st.color_scheme_dark);
    assert_eq!(
        html_st.visual.background_color,
        ColorRgba::rgb(0x22, 0x22, 0x22)
    );
    assert_eq!(
        ctx_dark.style_for(p_node).expect("p style").text.color,
        rix_css::color::SystemColor::CanvasText.resolve(true),
        "p must inherit light CanvasText when html opts into dark color-scheme"
    );

    let light_media = MediaContext {
        dark_mode: false,
        ..MediaContext::default()
    };
    let mut doc_light = parse_html(html);
    let mut ctx_light = StyleContext::new(light_media);
    ctx_light.add_stylesheet(Stylesheet::parse_cached(
        css,
        CascadeOrigin::Author,
        &light_media,
    ));
    ctx_light.cascade_full(&mut doc_light);
    let p_light = doc_light.get_element_by_id("msg").expect("msg node");
    assert_eq!(
        ctx_light.style_for(p_light).expect("p style").text.color,
        ColorRgba::BLACK
    );
}

#[test]
fn html_presentational_hints_apply_below_author_css() {
    let html = r##"<!doctype html><html><body>
        <table id="t1" width="85%" bgcolor="#f6f6ef" cellspacing="4">
            <tr><td id="c1" bgcolor="#ff6600">Header</td><td id="c2" bgcolor="#ff6600" class="override">Override</td></tr>
        </table>
    </body></html>"##;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(Arc::new(Stylesheet::parse(
        ".override { background-color: #112233; }",
        CascadeOrigin::Author,
        &media,
    )));
    ctx.cascade_full(&mut doc);

    let t1 = doc.get_element_by_id("t1").unwrap();
    let c1 = doc.get_element_by_id("c1").unwrap();
    let c2 = doc.get_element_by_id("c2").unwrap();

    assert_eq!(
        ctx.style_for(t1).unwrap().visual.background_color,
        ColorRgba::rgb(0xf6, 0xf6, 0xef)
    );
    assert_eq!(
        ctx.style_for(c1).unwrap().visual.background_color,
        ColorRgba::rgb(0xff, 0x66, 0x00)
    );
    assert_eq!(
        ctx.style_for(c2).unwrap().visual.background_color,
        ColorRgba::rgb(0x11, 0x22, 0x33),
        "author CSS rule must override HTML bgcolor presentational hint"
    );
}

#[test]
fn grid_template_areas_and_grid_area_resolve_1_based_lines() {
    let css = r#"
        .shell {
            display: grid;
            grid-template: auto 1fr auto / 220px 1fr;
            grid-template-areas:
                'header header'
                'sidebar main'
                'footer footer';
        }
        .sidebar { grid-area: sidebar; }
        .main { grid-area: main; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div id="shell" class="shell">
            <aside id="side" class="sidebar">TOC</aside>
            <main id="main" class="main">Article</main>
        </div>
    </body></html>"#;

    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(Arc::new(Stylesheet::parse(
        css,
        CascadeOrigin::Author,
        &media,
    )));
    ctx.cascade_full(&mut doc);

    let shell = doc.get_element_by_id("shell").unwrap();
    let fg = ctx
        .style_for(shell)
        .unwrap()
        .flex_grid
        .as_ref()
        .expect("grid flex_grid style");
    assert_eq!(fg.grid_template_areas.columns, 2);
    assert_eq!(
        fg.grid_template_areas.find_area_lines("header"),
        Some((1, 2, 1, 3))
    );
    assert_eq!(
        fg.grid_template_areas.find_area_lines("sidebar"),
        Some((2, 3, 1, 2))
    );
    assert_eq!(
        fg.grid_template_areas.find_area_lines("main"),
        Some((2, 3, 2, 3))
    );
}
