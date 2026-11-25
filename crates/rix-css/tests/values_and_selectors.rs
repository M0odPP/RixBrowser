//! Tests for typed CSS Color 4/5 values, math functions, and Selectors Level 4 matching.

use lightningcss::properties::{Property, PropertyId};
use lightningcss::stylesheet::ParserOptions;
use rix_css::values::{convert_color, convert_length_percentage};
use rix_css::{
    matches_selector_list_for_element, parse_selector_list, ColorRgba, CssColor, DomElement,
    EvalContext, RixPseudoElement,
};
use rix_dom::{Document, ElementState, NodeData, NodeId};
use rix_html::parse_html;
use selectors::bloom::BloomFilter;
use selectors::context::{QuirksMode, SelectorCaches};
use selectors::matching::selector_may_match;
use selectors::parser::AncestorHashes;
use selectors::Element as _;

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
    panic!("element with id '{target_id}' not found");
}

fn match_sel(doc: &Document, id: NodeId, sel: &str) -> bool {
    let list = parse_selector_list(sel).unwrap_or_else(|| panic!("invalid selector: {sel}"));
    let mut caches = SelectorCaches::default();
    let el = DomElement::new(doc, id);
    matches_selector_list_for_element(&list, &el, None, &mut caches, None)
}

#[test]
fn css_color_4_and_5_conversions() {
    let opts = ParserOptions::default();

    let Property::Color(hsl) = Property::parse_string(
        PropertyId::from("color"),
        "hsl(120deg 100% 50%)",
        opts.clone(),
    )
    .expect("parse hsl") else {
        panic!("expected color");
    };
    let hsl_rgba = convert_color(&hsl)
        .expect("convert hsl")
        .resolve(ColorRgba::BLACK, false);
    assert_eq!(hsl_rgba, ColorRgba::rgb(0, 255, 0));

    let Property::Color(oklch) = Property::parse_string(
        PropertyId::from("color"),
        "oklch(0.627955 0.257683 29.233885)",
        opts.clone(),
    )
    .expect("parse oklch") else {
        panic!("expected color");
    };
    let oklch_rgba = convert_color(&oklch)
        .expect("convert oklch")
        .resolve(ColorRgba::BLACK, false);
    assert!(oklch_rgba.r > 240 && oklch_rgba.g < 15 && oklch_rgba.b < 15);

    let Property::Color(mixed) = Property::parse_string(
        PropertyId::from("color"),
        "color-mix(in srgb, red 50%, blue 50%)",
        opts.clone(),
    )
    .expect("parse color-mix") else {
        panic!("expected color");
    };
    let mixed_rgba = convert_color(&mixed)
        .expect("convert color-mix")
        .resolve(ColorRgba::BLACK, false);
    assert!((i16::from(mixed_rgba.r) - 128).abs() <= 1);
    assert_eq!(mixed_rgba.g, 0);
    assert!((i16::from(mixed_rgba.b) - 128).abs() <= 1);

    let Property::Color(ld) = Property::parse_string(
        PropertyId::from("color"),
        "light-dark(#111111, #eeeeee)",
        opts,
    )
    .expect("parse light-dark") else {
        panic!("expected color");
    };
    let converted = convert_color(&ld).expect("convert light-dark");
    assert_eq!(
        converted.resolve(ColorRgba::BLACK, false),
        ColorRgba::rgb(0x11, 0x11, 0x11)
    );
    assert_eq!(
        converted.resolve(ColorRgba::BLACK, true),
        ColorRgba::rgb(0xee, 0xee, 0xee)
    );
    assert_eq!(
        CssColor::CurrentColor.resolve(ColorRgba::rgb(10, 20, 30), false),
        ColorRgba::rgb(10, 20, 30)
    );
}

#[test]
fn css_math_calc_min_max_clamp_resolution() {
    let ctx = EvalContext {
        font_size_px: 20.0,
        root_font_size_px: 16.0,
        viewport_width_px: 1000.0,
        viewport_height_px: 500.0,
        container_width_px: 800.0,
        container_height_px: 400.0,
        ..EvalContext::default()
    };
    let eval_width = |expr: &str, basis: f32| -> f32 {
        let Property::Width(lightningcss::properties::size::Size::LengthPercentage(lp)) =
            Property::parse_string(PropertyId::from("width"), expr, ParserOptions::default())
                .unwrap_or_else(|e| panic!("failed parsing {expr}: {e:?}"))
        else {
            panic!("expected length-percentage for {expr}");
        };
        convert_length_percentage(&lp).resolve_px(basis, &ctx)
    };

    assert!((eval_width("calc(50% + 2rem - 10px)", 400.0) - 222.0).abs() < 0.01);
    assert!((eval_width("min(50vw, 300px)", 400.0) - 300.0).abs() < 0.01);
    assert!((eval_width("max(10vw, 150px)", 400.0) - 150.0).abs() < 0.01);
    assert!((eval_width("clamp(100px, 50%, 250px)", 400.0) - 200.0).abs() < 0.01);
    assert!((eval_width("clamp(100px, 50%, 250px)", 120.0) - 100.0).abs() < 0.01);
    assert!((eval_width("clamp(100px, 50%, 250px)", 900.0) - 250.0).abs() < 0.01);
}

#[test]
fn selectors_l4_is_where_has_not_nth_of_and_states() {
    let mut doc = parse_html(
        r#"<!doctype html>
        <html lang="en-US">
        <body>
            <section id="card" class="panel primary">
                <h2 id="title">Heading</h2>
                <ul id="list">
                    <li id="i1" class="item featured">One</li>
                    <li id="i2" class="item">Two</li>
                    <li id="i3" class="item featured">Three</li>
                    <li id="i4" class="item featured">Four</li>
                </ul>
                <input id="chk" type="checkbox" checked>
                <a id="lnk" href="https://example.com">Link</a>
            </section>
        </body>
        </html>"#,
    );

    let card = find_by_id(&doc, "card");
    let i1 = find_by_id(&doc, "i1");
    let i2 = find_by_id(&doc, "i2");
    let i3 = find_by_id(&doc, "i3");
    let i4 = find_by_id(&doc, "i4");
    let chk = find_by_id(&doc, "chk");
    let lnk = find_by_id(&doc, "lnk");

    // :is() and :where() specificity and matching
    let is_list = parse_selector_list(":is(#card, .other)").expect("parse :is");
    let where_list = parse_selector_list(":where(#card, .other)").expect("parse :where");
    assert!(is_list.slice()[0].specificity() > 0);
    assert_eq!(where_list.slice()[0].specificity(), 0);
    assert!(match_sel(&doc, card, ":is(#card, .other)"));
    assert!(match_sel(&doc, card, ":where(#card, .other)"));

    // :has() matching parent with matching descendant
    assert!(match_sel(&doc, card, "section:has(> ul > li.featured)"));
    assert!(!match_sel(&doc, card, "section:has(> nav)"));

    // :not() complex selector list (Selectors L4)
    assert!(match_sel(&doc, i2, "li:not(.featured, #i4)"));
    assert!(!match_sel(&doc, i3, "li:not(.featured, #i4)"));

    // :nth-child(An+B of S)
    assert!(match_sel(&doc, i1, "li:nth-child(odd of .featured)"));
    assert!(!match_sel(&doc, i3, "li:nth-child(odd of .featured)"));
    assert!(match_sel(&doc, i4, "li:nth-child(odd of .featured)"));
    assert!(match_sel(&doc, i3, "li:nth-child(2 of .featured)"));

    // Interactive & form states + :lang()
    assert!(match_sel(&doc, chk, "input:checked:enabled"));
    assert!(match_sel(&doc, lnk, "a:link:lang(en)"));
    assert!(!match_sel(&doc, lnk, "a:hover"));
    doc.set_hovered_diff(Some(lnk));
    assert!(match_sel(&doc, lnk, "a:hover"));
    doc.set_focused_diff(Some(lnk), true);
    assert!(match_sel(&doc, lnk, "a:focus:focus-visible"));
    assert!(match_sel(&doc, card, "section:focus-within"));
    if let Some(NodeData::Element(el)) = doc.get_mut(lnk) {
        el.pseudo_state.insert(ElementState::VISITED);
    }
    assert!(match_sel(&doc, lnk, "a:visited"));

    // Pseudo-elements ::before / ::after
    let before_sel = parse_selector_list("#title::before").expect("parse ::before");
    let title = find_by_id(&doc, "title");
    let mut caches = SelectorCaches::default();
    let before_el = DomElement::with_pseudo(&doc, title, RixPseudoElement::Before);
    let after_el = DomElement::with_pseudo(&doc, title, RixPseudoElement::After);
    assert!(matches_selector_list_for_element(
        &before_sel,
        &before_el,
        None,
        &mut caches,
        None
    ));
    assert!(!matches_selector_list_for_element(
        &before_sel,
        &after_el,
        None,
        &mut caches,
        None
    ));

    // Bloom filter agreement between AncestorHashes and ElementData hashes
    let mut bloom = BloomFilter::new();
    let mut anc = doc.parent(i3);
    while let Some(a) = anc {
        if matches!(doc.get(a), Some(NodeData::Element(_))) {
            DomElement::new(&doc, a).add_element_unique_hashes(&mut bloom);
        }
        anc = doc.parent(a);
    }
    let matching_desc = parse_selector_list("#card.panel ul li.featured").expect("desc");
    let hashes_ok = AncestorHashes::new(&matching_desc.slice()[0], QuirksMode::NoQuirks);
    assert!(selector_may_match(&hashes_ok, &bloom));

    let missing_desc = parse_selector_list(".nonexistent-ancestor li.featured").expect("miss");
    let hashes_miss = AncestorHashes::new(&missing_desc.slice()[0], QuirksMode::NoQuirks);
    assert!(!selector_may_match(&hashes_miss, &bloom));
}

#[test]
fn shadow_dom_host_and_slotted_selectors() {
    let mut doc = Document::new();
    let html = doc.create_element("html");
    let body = doc.create_element("body");
    let host = doc.create_element("x-card");
    if let Some(NodeData::Element(el)) = doc.get_mut(host) {
        el.set_attribute("class", "dark");
    }
    let light_span = doc.create_element("span");
    if let Some(NodeData::Element(el)) = doc.get_mut(light_span) {
        el.set_attribute("slot", "header");
    }
    doc.append_child(doc.root(), html);
    doc.append_child(html, body);
    doc.append_child(body, host);
    doc.append_child(host, light_span);

    let shadow_root = doc.new_node(NodeData::ShadowRoot {
        host,
        mode: "open".to_owned(),
        delegates_focus: false,
        clonable: false,
        serializable: false,
        slot_assignment: "named".to_owned(),
        declarative: false,
        null_registry: false,
    });
    if let Some(NodeData::Element(el)) = doc.get_mut(host) {
        el.shadow_root = Some(shadow_root);
    }
    let slot = doc.create_element("slot");
    if let Some(NodeData::Element(el)) = doc.get_mut(slot) {
        el.set_attribute("name", "header");
    }
    doc.append_child(shadow_root, slot);

    let mut caches = SelectorCaches::default();
    let host_el = DomElement::new(&doc, host);
    let host_opaque = host_el.opaque();

    let host_sel = parse_selector_list(":host(.dark)").expect("parse :host");
    assert!(matches_selector_list_for_element(
        &host_sel,
        &host_el,
        None,
        &mut caches,
        Some(host_opaque)
    ));
    assert!(!matches_selector_list_for_element(
        &host_sel,
        &host_el,
        None,
        &mut caches,
        None
    ));

    let slotted_sel = parse_selector_list("::slotted(span)").expect("parse ::slotted");
    let light_el = DomElement::new(&doc, light_span);
    assert!(matches_selector_list_for_element(
        &slotted_sel,
        &light_el,
        None,
        &mut caches,
        Some(host_opaque)
    ));
}
