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
