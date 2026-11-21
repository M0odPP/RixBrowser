//! End-to-end tests for stylesheet compilation, compact `ComputedStyle`,
//! Cascade Level 5 (`@layer`, `!important`, `revert-layer`), `@media`, `@supports`,
//! `@font-face`, `@keyframes`, CSS nesting, and `var()` cycle/fallback resolution.

use std::mem::size_of;
use std::sync::Arc;

use rix_css::{
    matches_single_selector, parse_inline_style, resolve_element_style, CascadeOrigin, ColorRgba,
    CompiledRule, ComputedStyle, CursorKind, DisplayMode, FontFaceSource, LengthPercentage,
    LineStyle, MediaContext, MissingCssKind, PropertyIdKind, PropertyValue, SizeValue, Stylesheet,
    VisibilityMode,
};
use rix_dom::{Document, NodeData, NodeId};
use rix_html::parse_html;
use selectors::context::SelectorCaches;

fn find_by_id(doc: &Document, target_id: &str) -> NodeId {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
        if let Some(NodeData::Element(el)) = doc.get(id) {
            if el.cached_id.as_deref() == Some(target_id) {
                return id;
            }
        }
        for child in doc.children(id) {
            stack.push(child);
        }
    }
    panic!("element #{target_id} not found");
}

fn matched_rules_for<'a>(
    sheet: &'a Stylesheet,
    doc: &Document,
    node: NodeId,
) -> Vec<&'a CompiledRule> {
    let el = rix_css::DomElement::new(doc, node);
    let mut caches = SelectorCaches::default();
    sheet
        .rules
        .iter()
        .filter(|r| matches_single_selector(&r.selector, &el, None, &mut caches, None))
        .collect()
}

#[test]
fn computed_style_size_arc_sharing_and_zero_alloc_layout_eq() {
    assert!(
        size_of::<ComputedStyle>() <= 96,
        "ComputedStyle grew beyond 96-byte budget: {} bytes",
        size_of::<ComputedStyle>()
    );

    let mut parent = ComputedStyle::default();
    Arc::make_mut(&mut parent.text).font_size_px = 20.0;
    Arc::make_mut(&mut parent.text).color = ColorRgba::rgb(12, 34, 56);

    let child = ComputedStyle::inherit_from(&parent);
    assert!(Arc::ptr_eq(&parent.text, &child.text));
    assert!(Arc::ptr_eq(
        &parent.custom_properties,
        &child.custom_properties
    ));
    assert_eq!(child.text.font_size_px, 20.0);

    // Paint-only changes must preserve layout_eq == true.
    let mut paint_changed = child.clone();
    paint_changed.opacity = 0.5;
    paint_changed.visibility = VisibilityMode::Hidden;
    paint_changed.cursor = CursorKind::Pointer;
    Arc::make_mut(&mut paint_changed.visual).background_color = ColorRgba::rgb(255, 0, 128);
    Arc::make_mut(&mut paint_changed.text).color = ColorRgba::rgb(0, 255, 0);
    assert!(child.layout_eq(&paint_changed));

    // Layout-affecting changes must make layout_eq == false.
    let mut layout_changed = child.clone();
    layout_changed.display = DisplayMode::Flex;
    assert!(!child.layout_eq(&layout_changed));

    let mut width_changed = child.clone();
    Arc::make_mut(&mut width_changed.box_model).width =
        SizeValue::Length(LengthPercentage::px(240.0));
    assert!(!child.layout_eq(&width_changed));

    let mut font_changed = child.clone();
    Arc::make_mut(&mut font_changed.text).font_size_px = 24.0;
    assert!(!child.layout_eq(&font_changed));
}

#[test]
fn cascade_level_5_layers_important_revert_layer_and_specificity() {
    let doc = parse_html(r#"<div id="hero" class="box"></div>"#);
    let hero = find_by_id(&doc, "hero");
    let media = MediaContext::default();

    let css = r#"
        @layer reset, theme, overrides;

        @layer overrides {
            .box { color: #0000ff; width: 300px; }
        }
        @layer theme {
            #hero.box { color: #00ff00; width: 200px; }
        }
        @layer reset {
            #hero { color: #ff0000 !important; height: 50px !important; }
        }
        @layer overrides {
            div { color: #123456 !important; height: 99px !important; }
        }
        /* Unlayered normal beats all layered normal; unlayered !important loses to layered !important */
        div { width: 420px; height: 77px !important; }
    "#;

    let sheet = Stylesheet::parse(css, CascadeOrigin::Author, &media);
    let matched = matched_rules_for(&sheet, &doc, hero);
    let style = resolve_element_style(&matched, &[], None, 16.0, &media);

    // Normal width: unlayered `div` beats layered `#hero.box` and `.box`.
    assert_eq!(
        style.box_model.width,
        SizeValue::Length(LengthPercentage::px(420.0))
    );
    // Important color & height: earlier layer `reset` !important beats `overrides` !important and unlayered !important.
    assert_eq!(style.text.color, ColorRgba::rgb(255, 0, 0));
    assert_eq!(
        style.box_model.height,
        SizeValue::Length(LengthPercentage::px(50.0))
    );

    // Test revert-layer rolling back to the previous layer.
    let revert_css = r#"
        @layer base, accent;
        @layer base { #hero { background-color: #112233; } }
        @layer accent { #hero { background-color: #abcdef; background-color: revert-layer; } }
    "#;
    let revert_sheet = Stylesheet::parse(revert_css, CascadeOrigin::Author, &media);
    let revert_matched = matched_rules_for(&revert_sheet, &doc, hero);
    let revert_style = resolve_element_style(&revert_matched, &[], None, 16.0, &media);
    assert_eq!(
        revert_style.visual.background_color,
        ColorRgba::rgb(0x11, 0x22, 0x33)
    );

    // Inline style beats author normal, loses to author !important, and inline !important wins.
    let mut missing = Vec::new();
    let inline_normal = parse_inline_style("width: 600px; color: #abcdef;", &mut missing);
    let with_inline = resolve_element_style(&matched, &inline_normal, None, 16.0, &media);
    assert_eq!(
        with_inline.box_model.width,
        SizeValue::Length(LengthPercentage::px(600.0))
    );
    assert_eq!(with_inline.text.color, ColorRgba::rgb(255, 0, 0));

    let inline_imp = parse_inline_style("color: #fedcba !important;", &mut missing);
    let with_inline_imp = resolve_element_style(&matched, &inline_imp, None, 16.0, &media);
    assert_eq!(with_inline_imp.text.color, ColorRgba::rgb(0xfe, 0xdc, 0xba));
}

#[test]
fn media_supports_font_face_keyframes_nesting_and_missing_dedup() {
    let doc = parse_html(r#"<article class="card"><span class="badge">Hi</span></article>"#);
    let badge = doc
        .children(doc.children(doc.root()).find(|&n| {
            matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "html")
        }).unwrap())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "body"))
        .and_then(|body| doc.children(body).next())
        .and_then(|article| doc.children(article).next())
        .expect("badge node");

    let css = r#"
        @font-face {
            font-family: "Inter";
            src: local("Inter"), url("/fonts/inter.woff2") format("woff2");
            font-weight: 400 700;
            font-style: italic;
        }
        @keyframes fade-in {
            from { opacity: 0; }
            50% { opacity: 0.5; }
            to { opacity: 1; }
        }
        @media screen and (width >= 1000px) and (prefers-color-scheme: dark) {
            .card {
                & .badge { color: #eeeeee; }
                .badge { display: flex; }
            }
        }
        @media (max-width: 600px) {
            .badge { color: #ff0000; }
        }
        @supports (display: grid) and selector(:has(> span)) {
            .badge { background-color: #102030; }
        }
        @supports (nonexistent-prop: 123px) {
            .badge { background-color: #ff00ff; }
        }
        .badge { unknown-prop-x: 1; unknown-prop-x: 2; }
        @container (min-width: 300px) { .badge { opacity: 0.2; } }
        @container (min-width: 400px) { .badge { opacity: 0.3; } }
    "#;

    let dark_wide = MediaContext {
        viewport_width: 1280.0,
        viewport_height: 720.0,
        dark_mode: true,
        ..MediaContext::default()
    };
    let sheet = Stylesheet::parse(css, CascadeOrigin::Author, &dark_wide);

    // @font-face
    assert_eq!(sheet.font_faces.len(), 1);
    assert_eq!(sheet.font_faces[0].family.as_deref(), Some("Inter"));
    assert_eq!(sheet.font_faces[0].weight_range, (400.0, 700.0));
    assert!(sheet.font_faces[0].italic);
    assert!(sheet.font_faces[0].sources.iter().any(|s| matches!(
        s,
        FontFaceSource::Url { url, format }
            if url.as_str() == "/fonts/inter.woff2" && format.as_deref() == Some("woff2")
    )));

    // @keyframes
    assert_eq!(sheet.keyframes.len(), 1);
    assert_eq!(sheet.keyframes[0].name.as_str(), "fade-in");
    assert_eq!(sheet.keyframes[0].steps.len(), 3);
    assert_eq!(sheet.keyframes[0].steps[1].offsets, vec![0.5]);
    assert!(sheet.keyframes[0].steps[1]
        .declarations
        .iter()
        .any(|d| d.property == PropertyIdKind::Opacity && d.value == PropertyValue::Number(0.5)));

    // @media + @supports + CSS Nesting
    let matched = matched_rules_for(&sheet, &doc, badge);
    let style = resolve_element_style(&matched, &[], None, 16.0, &dark_wide);
    assert_eq!(style.text.color, ColorRgba::rgb(0xee, 0xee, 0xee));
    assert_eq!(style.display, DisplayMode::Flex);
    assert_eq!(
        style.visual.background_color,
        ColorRgba::rgb(0x10, 0x20, 0x30)
    );

    // MissingCssFeature deduplication
    let unknown_count = sheet
        .missing_features
        .iter()
        .filter(|m| m.kind == MissingCssKind::Property && m.name.as_str() == "unknown-prop-x")
        .count();
    let container_count = sheet
        .missing_features
        .iter()
        .filter(|m| m.kind == MissingCssKind::AtRule && m.name.as_str() == "@container")
        .count();
    assert_eq!(unknown_count, 1);
    assert_eq!(container_count, 1);
}

#[test]
fn css_custom_properties_inheritance_fallback_calc_and_cycle_guard() {
    let doc = parse_html(r#"<div id="parent"><div id="child"></div></div>"#);
    let parent_node = find_by_id(&doc, "parent");
    let child_node = find_by_id(&doc, "child");
    let media = MediaContext::default();

    let css = r#"
        #parent {
            --brand: #224466;
            --space: 12px;
            --border-spec: 2px solid #112233;
            --cyc-a: var(--cyc-b);
            --cyc-b: var(--cyc-a);
        }
        #child {
            color: var(--brand);
            background-color: var(--undefined-bg, #abcdef);
            width: calc(var(--space) + 8px);
            border: var(--border-spec);
            opacity: var(--cyc-a);
            padding-top: var(--cyc-a, 14px);
        }
    "#;

    let sheet = Stylesheet::parse(css, CascadeOrigin::Author, &media);
    let parent_rules = matched_rules_for(&sheet, &doc, parent_node);
    let parent_style = resolve_element_style(&parent_rules, &[], None, 16.0, &media);

    let child_rules = matched_rules_for(&sheet, &doc, child_node);
    let child_style = resolve_element_style(&child_rules, &[], Some(&parent_style), 16.0, &media);

    assert_eq!(child_style.text.color, ColorRgba::rgb(0x22, 0x44, 0x66));
    assert_eq!(
        child_style.visual.background_color,
        ColorRgba::rgb(0xab, 0xcd, 0xef)
    );
    let eval = media.eval_context(16.0, 16.0);
    let SizeValue::Length(ref w) = child_style.box_model.width else {
        panic!("expected calc width, got {:?}", child_style.box_model.width);
    };
    assert!((w.resolve_px(0.0, &eval) - 20.0).abs() < 0.01);
    assert_eq!(child_style.box_model.border_width.top, 2.0);
    assert_eq!(child_style.visual.border_style.left, LineStyle::Solid);
    // Cyclic `--cyc-a` is IACVT so opacity remains initial 1.0, and fallback 14px applies to padding-top.
    assert_eq!(child_style.opacity, 1.0);
    assert!((child_style.box_model.padding.top.resolve_px(0.0, &eval) - 14.0).abs() < 0.01);
}
