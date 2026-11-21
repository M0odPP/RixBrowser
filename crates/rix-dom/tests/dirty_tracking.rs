use rix_dom::{hash_ident, DirtyFlags, Document, ElementData, ElementState, NodeData};

#[test]
fn hover_diff_across_10k_nodes_touches_only_symmetric_ancestor_diff() {
    let mut doc = Document::new();
    let html = doc.create_element("html");
    let body = doc.create_element("body");
    doc.append_child(doc.root(), html);
    doc.append_child(html, body);

    // Build 100 branches x 100 nested/sibling nodes = > 10,000 elements.
    let mut branch_a_leaf = html;
    let mut branch_b_leaf = html;
    let mut untouched_sibling = html;

    for b in 0..100u32 {
        let section = doc.create_element("section");
        doc.append_child(body, section);
        let mut chain_cur = section;
        for d in 0..5u32 {
            let wrap = doc.create_element("div");
            doc.append_child(chain_cur, wrap);
            chain_cur = wrap;
            if b == 0 && d == 4 {
                branch_a_leaf = wrap;
            }
            if b == 1 && d == 4 {
                branch_b_leaf = wrap;
            }
            if b == 50 && d == 0 {
                untouched_sibling = wrap;
            }
        }
        for _ in 0..95u32 {
            let item = doc.create_element("span");
            doc.append_child(section, item);
        }
    }
    assert!(doc.node_count() > 10_000);

    doc.clear_all_dirty_flags();
    let root_mark_before = doc.style_mark(doc.root());

    // Hover leaf A: touches only [branch_a_leaf..section_0, body, html] (8 elements), never root.
    let diff1 = doc.set_hovered_diff(Some(branch_a_leaf));
    assert_eq!(diff1.len(), 8);
    assert_eq!(doc.style_mark(doc.root()), root_mark_before);
    assert!(!doc
        .dirty_flags(doc.root())
        .contains(DirtyFlags::SELF_STYLE_DIRTY));
    assert!(doc
        .element_state(branch_a_leaf)
        .contains(ElementState::HOVER));
    assert!(doc.element_state(body).contains(ElementState::HOVER));
    assert_eq!(doc.dirty_flags(untouched_sibling), DirtyFlags::empty());

    doc.clear_all_dirty_flags();

    // Transition hover from leaf A to leaf B: `body` & `html` stay hovered and stay clean!
    let diff2 = doc.set_hovered_diff(Some(branch_b_leaf));
    assert_eq!(
        diff2.len(),
        12,
        "only 6 leaving nodes in branch 0 + 6 entering nodes in branch 1"
    );
    assert!(!diff2.contains(&body));
    assert!(!diff2.contains(&html));
    assert!(!doc.dirty_flags(body).contains(DirtyFlags::SELF_STYLE_DIRTY));
    assert!(doc
        .dirty_flags(body)
        .contains(DirtyFlags::DESCENDANT_STYLE_DIRTY));
    assert!(!doc
        .element_state(branch_a_leaf)
        .contains(ElementState::HOVER));
    assert!(doc
        .element_state(branch_b_leaf)
        .contains(ElementState::HOVER));
    assert_eq!(doc.dirty_flags(untouched_sibling), DirtyFlags::empty());
}

#[test]
fn focus_diff_updates_focus_and_focus_within_cleanly() {
    let mut doc = Document::new();
    let form = doc.create_element("form");
    let input_a = doc.create_element("input");
    let input_b = doc.create_element("input");
    doc.append_child(doc.root(), form);
    doc.append_child(form, input_a);
    doc.append_child(form, input_b);
    doc.clear_all_dirty_flags();

    let affected1 = doc.set_focused_diff(Some(input_a), true);
    assert!(affected1.contains(&form));
    assert!(affected1.contains(&input_a));
    assert!(doc
        .element_state(input_a)
        .contains(ElementState::FOCUS | ElementState::FOCUS_VISIBLE | ElementState::FOCUS_WITHIN));
    assert!(doc.element_state(form).contains(ElementState::FOCUS_WITHIN));
    assert!(!doc.element_state(form).contains(ElementState::FOCUS));

    doc.clear_all_dirty_flags();
    let affected2 = doc.set_focused_diff(Some(input_b), false);
    assert!(affected2.contains(&input_a));
    assert!(affected2.contains(&input_b));
    assert!(!affected2.contains(&form)); // `form` kept FOCUS_WITHIN without changing!
    assert!(!doc.element_state(input_a).contains(ElementState::FOCUS));
    assert!(doc.element_state(input_b).contains(ElementState::FOCUS));
    assert!(!doc
        .element_state(input_b)
        .contains(ElementState::FOCUS_VISIBLE));
    assert!(doc.element_state(form).contains(ElementState::FOCUS_WITHIN));
}

#[test]
fn element_data_syncs_cached_id_class_hashes_and_intrinsic_states() {
    let mut anchor = ElementData::new_html("a");
    assert!(!anchor.pseudo_state.contains(ElementState::LINK));
    anchor.set_attribute("href", "https://example.org");
    anchor.set_attribute("id", "hero-link");
    anchor.set_attribute("class", "btn primary btn");
    assert!(anchor.pseudo_state.contains(ElementState::LINK));
    assert_eq!(anchor.cached_id.as_deref(), Some("hero-link"));
    assert_eq!(anchor.class_hashes.len(), 2);
    assert!(anchor.has_class_hash(hash_ident("btn")));
    assert!(anchor.has_class_hash(hash_ident("primary")));

    anchor.remove_attribute("id");
    assert_eq!(anchor.cached_id, None);

    let mut btn = ElementData::new_html("button");
    assert!(btn.pseudo_state.contains(ElementState::ENABLED));
    assert!(!btn.pseudo_state.contains(ElementState::DISABLED));
    btn.set_attribute("disabled", "");
    assert!(btn.pseudo_state.contains(ElementState::DISABLED));
    assert!(!btn.pseudo_state.contains(ElementState::ENABLED));

    let mut chk = ElementData::new_html("input");
    chk.set_attribute("checked", "");
    assert!(chk.pseudo_state.contains(ElementState::CHECKED));
    let _ = NodeData::Element(chk);
}
