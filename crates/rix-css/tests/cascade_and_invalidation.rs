//! Tests for Stylo-style `RuleIndex`, Bloom filter pruning, `InvalidationMap`,
//! parallel cascade, `StyleMemo` sharing, and incremental-vs-full cascade parity.

use std::sync::Arc;

use rix_css::{
    CascadeOrigin, ColorRgba, DisplayMode, InvalidationScope, MediaContext, MissingCssKind,
    RixPseudoElement, StyleContext, Stylesheet,
};
use rix_dom::{Document, NodeData, NodeId};
use rix_html::parse_html;

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

fn collect_all_elements(doc: &Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
        if matches!(doc.get(id), Some(NodeData::Element(_))) {
            out.push(id);
        }
        let children: Vec<NodeId> = doc.children(id).collect();
        for c in children.into_iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn assert_incremental_matches_full(
    doc: &mut Document,
    incremental: &StyleContext,
    sheet: &Arc<Stylesheet>,
    media: MediaContext,
) {
    let mut reference = StyleContext::new(media);
    reference.add_stylesheet(Arc::clone(sheet));
    let mut doc_clone = doc.clone();
    reference.cascade_full(&mut doc_clone);

    // Compare every element in tree order (NodeIds match on cloned arena).
    let inc_nodes = collect_all_elements(doc);
    let ref_nodes = collect_all_elements(&doc_clone);
    assert_eq!(inc_nodes.len(), ref_nodes.len());
    for (inc_id, ref_id) in inc_nodes.into_iter().zip(ref_nodes) {
        let s_inc = incremental
            .style_for(inc_id)
            .expect("incremental style missing");
        let s_ref = reference
            .style_for(ref_id)
            .expect("reference style missing");
        assert_eq!(
            **s_inc, **s_ref,
            "incremental style diverged from full cascade on node {inc_id:?}"
        );
    }
}

#[test]
fn invalidation_map_scopes_and_incremental_parity() {
    let mut doc = parse_html(
        r#"<!doctype html>
        <html>
        <body>
            <div id="wrap" class="theme">
                <button id="btn" class="btn">Click</button>
                <span id="sib" class="note">Sibling</span>
                <div id="nested"><em id="deep" class="leaf">Deep</em></div>
            </div>
            <div id="unrelated" class="other">Untouched</div>
        </body>
        </html>"#,
    );

    let css = r#"
        .btn { background-color: #222222; color: #ffffff; }
        .btn:hover { background-color: #3366ff; }
        .btn.active + .note { color: #ff0000; }
        .theme.dark .leaf { color: #00cc88; }
        #wrap:has(.btn.selected) { background-color: #111111; }
        #btn::before { color: #abcdef; }
        .unused-feature { totally-unknown-prop: 1; }
    "#;

    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(Arc::clone(&sheet));

    let full = ctx.cascade_full(&mut doc);
    assert!(full.restyled_elements >= 7);

    // Verify UA defaults + pseudo-element style + missing-feature telemetry
    let wrap = find_by_id(&doc, "wrap");
    let btn = find_by_id(&doc, "btn");
    let sib = find_by_id(&doc, "sib");
    let deep = find_by_id(&doc, "deep");
    assert_eq!(ctx.style_for(wrap).unwrap().display, DisplayMode::Block);
    assert_eq!(
        ctx.pseudo_style_for(btn, RixPseudoElement::Before)
            .unwrap()
            .text
            .color,
        ColorRgba::rgb(0xab, 0xcd, 0xef)
    );
    assert!(ctx
        .missing_features()
        .iter()
        .any(|m| m.kind == MissingCssKind::Property && m.name.as_str() == "totally-unknown-prop"));

    // Verify InvalidationMap scopes
    let inv = ctx.invalidation_map();
    assert!(inv
        .scope_for_class("dark")
        .contains(InvalidationScope::DESCENDANTS));
    assert!(inv
        .scope_for_class("active")
        .contains(InvalidationScope::LATER_SIBLINGS));
    assert!(inv
        .scope_for_class("selected")
        .contains(InvalidationScope::ANCESTORS));
    assert!(inv.scope_for_class("nonexistent").is_empty());

    // 1. Hover toggle on #btn: paint-only change, restyles only the hover ancestor path (never #unrelated).
    ctx.set_hovered(&mut doc, Some(btn));
    let hover_rep = ctx.restyle_dirty(&mut doc);
    assert!(
        hover_rep.restyled_elements <= 4,
        "expected only hover ancestor chain to restyle, got {}",
        hover_rep.restyled_elements
    );
    assert_eq!(hover_rep.paint_only_count, 1);
    assert_eq!(hover_rep.layout_dirty_count, 0);
    assert_eq!(
        ctx.style_for(btn).unwrap().visual.background_color,
        ColorRgba::rgb(0x33, 0x66, 0xff)
    );
    assert_incremental_matches_full(&mut doc, &ctx, &sheet, media);

    // 2. Sibling invalidation: adding `.active` on #btn updates #sib.
    if let Some(NodeData::Element(el)) = doc.get_mut(btn) {
        el.set_attribute("class", "btn active");
    }
    ctx.invalidate_class_change(&mut doc, btn, "active");
    let sib_rep = ctx.restyle_dirty(&mut doc);
    assert!(
        sib_rep.restyled_elements <= 3,
        "expected only later siblings of #btn to restyle, got {}",
        sib_rep.restyled_elements
    );
    assert_eq!(
        ctx.style_for(sib).unwrap().text.color,
        ColorRgba::rgb(255, 0, 0)
    );
    assert_incremental_matches_full(&mut doc, &ctx, &sheet, media);

    // 3. Descendant invalidation: adding `.dark` on #wrap updates #deep.
    if let Some(NodeData::Element(el)) = doc.get_mut(wrap) {
        el.set_attribute("class", "theme dark");
    }
    ctx.invalidate_class_change(&mut doc, wrap, "dark");
    let _desc_rep = ctx.restyle_dirty(&mut doc);
    assert_eq!(
        ctx.style_for(deep).unwrap().text.color,
        ColorRgba::rgb(0x00, 0xcc, 0x88)
    );
    assert_incremental_matches_full(&mut doc, &ctx, &sheet, media);

    // 4. Ancestor :has() invalidation: adding `.selected` on #btn updates #wrap.
    if let Some(NodeData::Element(el)) = doc.get_mut(btn) {
        el.set_attribute("class", "btn active selected");
    }
    ctx.invalidate_class_change(&mut doc, btn, "selected");
    let _has_rep = ctx.restyle_dirty(&mut doc);
    assert_eq!(
        ctx.style_for(wrap).unwrap().visual.background_color,
        ColorRgba::rgb(0x11, 0x11, 0x11)
    );
    assert_incremental_matches_full(&mut doc, &ctx, &sheet, media);
}

#[test]
fn parallel_cascade_and_style_memo_on_large_sibling_list() {
    let mut html = String::from("<!doctype html><html><body><ul id=\"grid\">");
    for i in 0..500 {
        if i == 250 {
            html.push_str("<li id=\"target\" class=\"row\">Item</li>");
        } else {
            html.push_str("<li class=\"row\">Item</li>");
        }
    }
    html.push_str("</ul></body></html>");

    let mut doc = parse_html(&html);
    let css = r#"
        #grid { display: flex; flex-wrap: wrap; gap: 8px; }
        #grid > .row { display: block; padding: 6px; color: #333333; }
        #grid > .row:hover { background-color: #eeeeee; }
    "#;

    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(Arc::clone(&sheet));

    let full = ctx.cascade_full(&mut doc);
    assert_eq!(full.restyled_elements, 504); // html, head, body, ul, 500 li
    assert!(
        full.shared_memo_hits >= 400,
        "expected StyleMemo to share identical sibling styles, got {} hits",
        full.shared_memo_hits
    );

    let target = find_by_id(&doc, "target");
    ctx.set_hovered(&mut doc, Some(target));
    let inc = ctx.restyle_dirty(&mut doc);
    assert!(
        inc.restyled_elements <= 4,
        "expected only hover ancestor path (<= 4 nodes of 504), got {}",
        inc.restyled_elements
    );
    assert_eq!(inc.paint_only_count, 1);
    assert_eq!(inc.layout_dirty_count, 0);
    assert_eq!(
        ctx.style_for(target).unwrap().visual.background_color,
        ColorRgba::rgb(0xee, 0xee, 0xee)
    );
    assert_incremental_matches_full(&mut doc, &ctx, &sheet, media);
}

#[test]
fn media_query_comma_lists_evaluate_per_mq_l4() {
    // CSS Media Queries L4 §3: comma-separated media query lists match if ANY query matches.
    let media = MediaContext::default();
    assert!(media.evaluate_media_query_str("screen, print"));
    assert!(media.evaluate_media_query_str("print, screen"));
    assert!(media.evaluate_media_query_str("screen,projection"));
    assert!(media.evaluate_media_query_str("(min-width: 600px), print"));
    assert!(media.evaluate_media_query_str("height <= 999999px"));
    assert!(!media.evaluate_media_query_str("print"));
    assert!(!media.evaluate_media_query_str("print, speech"));
}

#[test]
fn registered_properties_o1_inheritance_and_non_inherited_reset() {
    // CSS Properties and Values API L1: non-inherited `@property` declarations reset to
    // `initial-value` (or absent when omitted) on children without re-cloning all registrations
    // on every descendant element.
    let mut css = String::new();
    for i in 0..100 {
        css.push_str(&format!(
            "@property --reg-ni-{i} {{ syntax: \"*\"; inherits: false; initial-value: {i}px; }}\n"
        ));
    }
    css.push_str(
        r#"
        @property --reg-no-init { syntax: "*"; inherits: false; }
        @property --reg-len { syntax: "<length>"; inherits: true; initial-value: 10px; }
        #parent { --reg-ni-0: 99px; --reg-no-init: active; font-size: 20px; --reg-len: 2em; }
        #child { font-size: 10px; }
        "#,
    );
    let html = r#"<!doctype html><html><body><div id="parent"><div id="child"><span id="grand"></span></div></div><div id="sib"></div></body></html>"#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(&css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    let parent = find_by_id(&doc, "parent");
    let child = find_by_id(&doc, "child");
    let grand = find_by_id(&doc, "grand");
    let sib = find_by_id(&doc, "sib");

    let p_st = ctx.style_for(parent).unwrap();
    let c_st = ctx.style_for(child).unwrap();
    let g_st = ctx.style_for(grand).unwrap();
    let s_st = ctx.style_for(sib).unwrap();

    assert_eq!(
        p_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-ni-0")),
        Some("99px")
    );
    assert_eq!(
        p_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-no-init")),
        Some("active")
    );
    assert_eq!(
        p_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-len")),
        Some("40px")
    );

    // Child resets non-inherited `--reg-ni-0` and `--reg-no-init`, while inheriting computed `--reg-len: 40px`!
    assert_eq!(
        c_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-ni-0")),
        Some("0px")
    );
    assert_eq!(
        c_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-no-init")),
        None
    );
    assert_eq!(
        c_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-len")),
        Some("40px")
    );

    // Grandchild shares `CustomPropertyMap` pointer directly with child in O(1)!
    assert!(
        Arc::ptr_eq(&c_st.custom_properties, &g_st.custom_properties),
        "unchanged descendant must share CustomPropertyMap in O(1) even with 100+ @property rules"
    );
    // Sibling shares untouched root-seeded initial values.
    assert_eq!(
        s_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-ni-42")),
        Some("42px")
    );
    assert_eq!(
        s_st.custom_properties
            .get_serialized(&rix_core::intern_str("--reg-len")),
        Some("10px")
    );
}

#[test]
fn bucketed_pseudo_elements_and_attribute_rules_match_accurately() {
    let css = r#"
        .card::before { content: "A"; color: #ff0000; }
        #hero::after { content: "B"; color: #00ff00; }
        [data-badge]::before { content: "C"; color: #0000ff; }
        [data-state="open"] { background-color: #123456; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div id="hero" class="card" data-state="open"></div>
        <span id="badge" data-badge="1"></span>
    </body></html>"#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    let hero = find_by_id(&doc, "hero");
    let badge = find_by_id(&doc, "badge");

    assert_eq!(
        ctx.style_for(hero).unwrap().visual.background_color,
        ColorRgba::rgb(0x12, 0x34, 0x56)
    );
    assert_eq!(
        ctx.pseudo_style_for(hero, rix_css::RixPseudoElement::Before)
            .unwrap()
            .text
            .color,
        ColorRgba::rgb(0xff, 0x00, 0x00)
    );
    assert_eq!(
        ctx.pseudo_style_for(hero, rix_css::RixPseudoElement::After)
            .unwrap()
            .text
            .color,
        ColorRgba::rgb(0x00, 0xff, 0x00)
    );
    assert_eq!(
        ctx.pseudo_style_for(badge, rix_css::RixPseudoElement::Before)
            .unwrap()
            .text
            .color,
        ColorRgba::rgb(0x00, 0x00, 0xff)
    );
}

#[test]
fn universal_noop_custom_properties_preserve_o1_arc_sharing() {
    // Tailwind v4 pattern: `*, ::before, ::after, ::backdrop` re-declares 100+ `--tw-*` custom
    // properties identical to `@property` initial values (`0` / `initial`). Unchanged elements
    // must preserve O(1) `Arc` sharing of `CustomPropertyMap` without allocating per-element maps.
    let mut css = String::new();
    for i in 0..80 {
        css.push_str(&format!(
            "@property --tw-n-{i} {{ syntax: \"*\"; inherits: false; initial-value: 0; }}\n"
        ));
    }
    for i in 0..20 {
        css.push_str(&format!(
            "@property --tw-u-{i} {{ syntax: \"*\"; inherits: false; }}\n"
        ));
    }
    css.push_str("*, ::before, ::after, ::backdrop {\n");
    for i in 0..80 {
        css.push_str(&format!("  --tw-n-{i}: 0;\n"));
    }
    for i in 0..20 {
        css.push_str(&format!("  --tw-u-{i}: initial;\n"));
    }
    css.push_str("}\n#override { --tw-n-0: 42px; }\n");

    let html = r#"<!doctype html><html><body><div id="a"><span id="b"></span></div><div id="override"><span id="reset"></span></div></body></html>"#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(&css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    let a = find_by_id(&doc, "a");
    let b = find_by_id(&doc, "b");
    let ov = find_by_id(&doc, "override");
    let rst = find_by_id(&doc, "reset");

    let a_st = ctx.style_for(a).unwrap();
    let b_st = ctx.style_for(b).unwrap();
    let ov_st = ctx.style_for(ov).unwrap();
    let rst_st = ctx.style_for(rst).unwrap();

    assert!(
        Arc::ptr_eq(&a_st.custom_properties, &b_st.custom_properties),
        "universal no-op custom property re-declarations must keep O(1) Arc sharing"
    );
    assert_eq!(
        ov_st
            .custom_properties
            .get_serialized(&rix_core::intern_str("--tw-n-0")),
        Some("42px")
    );
    assert_eq!(
        rst_st
            .custom_properties
            .get_serialized(&rix_core::intern_str("--tw-n-0")),
        Some("0")
    );
    // `::backdrop` on a non-top-layer `<div>` must not be eagerly materialized.
    assert!(ctx
        .pseudo_style_for(a, rix_css::RixPseudoElement::Backdrop)
        .is_none());
}

#[test]
fn typed_length_zero_noop_and_pseudo_memo_preserve_arc_sharing_and_restyle_identity() {
    // CSS Properties & Values API L1 + StyleMemo:
    // 1. `@property --tw-len { syntax: "<length>"; inherits: false; initial-value: 0; }`
    //    canonicalizes to `0px` at root; descendants re-declaring `--tw-len: 0` must remain O(1)
    //    no-ops sharing the same `Arc<CustomPropertyMap>`.
    // 2. Pseudo-elements (`::before`) declaring `var(...)` and custom props (without `attr()`)
    //    must hit `StyleMemo` and share identical `Arc<ComputedStyle>` across matching elements.
    // 3. Incremental `restyle_dirty` on an unchanged element must preserve `Arc::ptr_eq` on
    //    `ComputedStyle` and `custom_properties`.
    let css = r#"
        @property --tw-len { syntax: "<length>"; inherits: false; initial-value: 0; }
        @property --tw-ang { syntax: "<angle>"; inherits: false; initial-value: 0deg; }
        :root { --accent: #112233; }
        *, ::before, ::after {
            --tw-len: 0;
            --tw-ang: 0;
            border-color: var(--accent);
        }
        .item::before { content: ""; color: var(--accent); }
        .item:nth-child(even) { opacity: 0.9; }
        [data-noop="yes"] { color: #ff0000; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div id="p">
            <span id="i1" class="item" data-id="1"></span>
            <span id="i2" class="item" data-id="2"></span>
            <span id="i3" class="item" data-id="3"></span>
        </div>
    </body></html>"#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    let p = find_by_id(&doc, "p");
    let i1 = find_by_id(&doc, "i1");
    let i3 = find_by_id(&doc, "i3");

    let p_st = ctx.style_for(p).unwrap().clone();
    let i1_st = ctx.style_for(i1).unwrap().clone();
    let i3_st = ctx.style_for(i3).unwrap().clone();
    assert!(
        Arc::ptr_eq(&p_st.custom_properties, &i1_st.custom_properties),
        "typed `<length>` 0 vs 0px universal re-declaration must keep O(1) Arc sharing"
    );
    assert!(
        Arc::ptr_eq(&i1_st, &i3_st),
        "elements using `var(...)` with identical matched rules must hit StyleMemo"
    );
    let b1 = ctx
        .pseudo_style_for(i1, rix_css::RixPseudoElement::Before)
        .unwrap()
        .clone();
    let b3 = ctx
        .pseudo_style_for(i3, rix_css::RixPseudoElement::Before)
        .unwrap()
        .clone();
    assert!(
        Arc::ptr_eq(&b1, &b3),
        "pseudo-elements using `var(...)` without `attr()` must share Arc via StyleMemo"
    );

    // Trigger an incremental no-op restyle on `#p` and verify `Arc::ptr_eq` is preserved.
    ctx.invalidate_attribute_change(&mut doc, p, "data-noop");
    let report = ctx.restyle_dirty(&mut doc);
    assert!(
        report.restyled_elements >= 1,
        "expected `#p` to be restyled by `restyle_dirty`"
    );
    assert_eq!(report.layout_dirty_count, 0);
    assert_eq!(report.paint_only_count, 0);
    assert!(
        Arc::ptr_eq(&p_st, ctx.style_for(p).unwrap()),
        "unchanged restyle_dirty must preserve the existing Arc<ComputedStyle>"
    );
}

#[test]
fn font_face_parses_unicode_ranges_per_css_fonts_l4() {
    let css = r#"
        @font-face {
            font-family: "TestSans";
            font-weight: 100 900;
            src: url("/fonts/latin.woff2") format("woff2");
            unicode-range: U+0000-00FF, U+0131, U+20??;
        }
    "#;
    let sheet = Stylesheet::parse(css, CascadeOrigin::Author, &MediaContext::default());
    assert_eq!(sheet.font_faces.len(), 1);
    let ff = &sheet.font_faces[0];
    assert_eq!(ff.weight_range, (100.0, 900.0));
    assert_eq!(
        ff.unicode_ranges,
        vec![(0x0000, 0x00FF), (0x0131, 0x0131), (0x2000, 0x20FF)]
    );
}

#[test]
fn unresolved_rgb_hsl_var_alpha_and_svg_fill_cascade_accurately() {
    // CSS Color 4 / Tailwind v3: `rgb(R G B / var(--op, 1))`, `hsl(H S% L% / var(--op))`, and
    // `light-dark(var(--l), var(--d))` parsed by lightningcss as `TokenOrValue::UnresolvedColor`
    // must preserve all channels across `var()` substitution, and SVG `fill`/`fill-rule` must
    // inherit per SVG 2 §13.
    let css = r#"
        :root { --tw-bg-opacity: 1; --tw-text-opacity: 0.5; --l: #112233; --d: #abcdef; }
        .hero {
            background-color: rgb(6 9 19/var(--tw-bg-opacity,1));
            color: rgb(255 255 255/var(--tw-text-opacity,1));
            border-color: hsl(120 100% 50% / var(--tw-bg-opacity, 1));
        }
        .ripples svg { fill: #ff1f8f; fill-rule: evenodd; }
    "#;
    let html = r#"<!doctype html><html><body>
        <div id="h" class="hero"></div>
        <div class="ripples"><svg id="s"></svg></div>
    </body></html>"#;
    let mut doc = parse_html(html);
    let media = MediaContext::default();
    let sheet = Arc::new(Stylesheet::parse(css, CascadeOrigin::Author, &media));
    let mut ctx = StyleContext::new(media);
    ctx.add_stylesheet(sheet);
    ctx.cascade_full(&mut doc);

    let h = find_by_id(&doc, "h");
    let s = find_by_id(&doc, "s");
    let h_st = ctx.style_for(h).unwrap();
    assert_eq!(h_st.visual.background_color, ColorRgba::rgb(6, 9, 19));
    assert_eq!(h_st.text.color, ColorRgba::rgba(255, 255, 255, 128));
    assert_eq!(
        h_st.visual.border_color.top.resolve(h_st.text.color, false),
        ColorRgba::rgb(0, 255, 0)
    );
    let s_st = ctx.style_for(s).unwrap();
    assert_eq!(
        s_st.text.svg_fill,
        Some(Some(rix_css::CssColor::Rgba(ColorRgba::rgb(
            0xff, 0x1f, 0x8f
        ))))
    );
    assert!(s_st.text.svg_fill_rule_even_odd);
}
