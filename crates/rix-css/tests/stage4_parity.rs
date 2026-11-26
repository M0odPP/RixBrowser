//! Specification regression tests for CSS 2.1 §10.8.1 `line-height` inheritance,
//! CSS Fonts L4 `font` shorthand CSS-wide keywords, CSS Values L3 §4.5 stylesheet-relative
//! `url(...)` resolution, and HTML5 User-Agent defaults.

use std::sync::Arc;

use rix_css::{
    BackgroundImageValue, CascadeOrigin, LengthPercentage, LengthUnit, LineHeightValue,
    MediaContext, OverflowMode, StyleContext, Stylesheet,
};
use rix_dom::{Document, NodeData};

fn parse_and_cascade(html: &str, css: &str, base_url: Option<&str>) -> (Document, StyleContext) {
    let mut doc = rix_html::parse_html(html);
    let media = MediaContext::default();
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(Stylesheet::parse_cached_with_base(
        css,
        CascadeOrigin::Author,
        &media,
        base_url,
    ));
    ctx.cascade_full(&mut doc);
    (doc, ctx)
}

fn find_by_id(doc: &Document, id: &str) -> rix_dom::NodeId {
    doc.descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if e.get_attribute("id") == Some(id)))
        .expect("element with id exists")
}

#[test]
fn font_shorthand_inherit_expands_all_font_longhands() {
    let (doc, ctx) = parse_and_cascade(
        r#"<!doctype html><html><body style="font-size: 20px; font-weight: 700; font-family: Georgia, serif;"><button id="b">Click</button></body></html>"#,
        "button { font: inherit; }",
        None,
    );
    let b = find_by_id(&doc, "b");
    let st = ctx.style_for(b).expect("button styled");
    assert!(
        (st.text.font_size_px - 20.0).abs() < 0.01,
        "font: inherit must override UA 13.3333px font-size, got {}",
        st.text.font_size_px
    );
    assert!((st.text.font_weight - 700.0).abs() < 0.01);
    assert_eq!(
        st.text.font_family.first().map(|s| s.as_str()),
        Some("Georgia")
    );
}

#[test]
fn line_height_percentage_and_em_inherit_as_computed_px_while_number_inherits_as_factor() {
    let (doc, ctx) = parse_and_cascade(
        r#"<!doctype html><html><body>
            <div id="p_pct" style="font-size: 16px; line-height: 130%;"><h1 id="c_pct" style="font-size: 48px;">A</h1></div>
            <div id="p_em" style="font-size: 16px; line-height: 1.5em;"><h1 id="c_em" style="font-size: 48px;">B</h1></div>
            <div id="p_num" style="font-size: 16px; line-height: 1.3;"><h1 id="c_num" style="font-size: 48px;">C</h1></div>
        </body></html>"#,
        "",
        None,
    );
    let c_pct = ctx.style_for(find_by_id(&doc, "c_pct")).unwrap();
    let c_em = ctx.style_for(find_by_id(&doc, "c_em")).unwrap();
    let c_num = ctx.style_for(find_by_id(&doc, "c_num")).unwrap();

    assert_eq!(
        c_pct.text.line_height,
        LineHeightValue::Length(LengthPercentage::Length(20.8, LengthUnit::Px)),
        "CSS 2.1 §10.8.1: percentage line-height inherits as computed px (16 * 1.3 = 20.8px)"
    );
    assert_eq!(
        c_em.text.line_height,
        LineHeightValue::Length(LengthPercentage::Length(24.0, LengthUnit::Px)),
        "CSS 2.1 §10.8.1: em line-height inherits as computed px (16 * 1.5 = 24px)"
    );
    assert_eq!(
        c_num.text.line_height,
        LineHeightValue::Number(1.3),
        "CSS 2.1 §10.8.1: unitless number line-height inherits as factor"
    );
    assert!((c_num.text.eval_ctx.line_height_px - 62.4).abs() < 0.05);
}

#[test]
fn root_font_family_inherit_falls_back_to_initial_serif() {
    let (doc, ctx) = parse_and_cascade(
        "<!doctype html><html id='r'><body>Hello</body></html>",
        "html { font-family: inherit; }",
        None,
    );
    let r = find_by_id(&doc, "r");
    let st = ctx.style_for(r).unwrap();
    assert_eq!(
        st.text.font_family.first().map(|s| s.as_str()),
        Some("serif")
    );
}

#[test]
fn stylesheet_relative_background_urls_resolve_against_base_url() {
    let (doc, ctx) = parse_and_cascade(
        r#"<!doctype html><html><body><div id="a"></div><div id="b"></div><div id="c"></div></body></html>"#,
        r#"
            #a { background-image: url("../media/bg.svg"); }
            #b { background-image: url("/static/hero.png"); }
            #c { background-image: url("data:image/svg+xml,<svg/>"); }
        "#,
        Some("https://example.org/theme/css/main.css"),
    );
    let url_of = |id: &str| -> String {
        let st = ctx.style_for(find_by_id(&doc, id)).unwrap();
        match &st.visual.background_layers[0].image {
            BackgroundImageValue::Url(u) => u.as_str().to_string(),
            other => panic!("expected Url, got {other:?}"),
        }
    };
    assert_eq!(url_of("a"), "https://example.org/theme/media/bg.svg");
    assert_eq!(url_of("b"), "https://example.org/static/hero.png");
    assert_eq!(url_of("c"), "data:image/svg+xml,<svg/>");
}

#[test]
fn html5_ua_defaults_match_spec_for_forms_hr_and_nested_lists() {
    let (doc, ctx) = parse_and_cascade(
        r#"<!doctype html><html><body>
            <hr id="h">
            <input id="txt" type="text">
            <input id="chk" type="checkbox">
            <ul><li><ul id="nested"><li>x</li></ul></li></ul>
        </body></html>"#,
        "",
        None,
    );
    let hr = ctx.style_for(find_by_id(&doc, "h")).unwrap();
    assert_eq!(hr.overflow.x, OverflowMode::Hidden);
    assert_eq!(hr.overflow.y, OverflowMode::Hidden);

    let txt = ctx.style_for(find_by_id(&doc, "txt")).unwrap();
    assert_eq!(
        txt.text.font_family.first().map(|s| s.as_str()),
        Some("sans-serif")
    );
    assert_eq!(txt.overflow.x, OverflowMode::Clip);

    let chk = ctx.style_for(find_by_id(&doc, "chk")).unwrap();
    assert_eq!(chk.visual.background_color.a, 0);
    assert_eq!(chk.box_model.border_width.top, 0.0);

    let nested = ctx.style_for(find_by_id(&doc, "nested")).unwrap();
    assert_eq!(
        nested.box_model.margin.top,
        rix_css::LengthPercentageAuto::Value(LengthPercentage::Zero)
    );
    let _ = Arc::new(());
}
