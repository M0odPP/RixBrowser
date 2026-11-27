#![deny(unsafe_code)]

use markup5ever::{local_name, ns, QualName};
use rix_dom::NodeData;
use rix_html::{detect_html_charset, parse_html, parse_html_fragment, parse_html_with_scripting};

#[test]
fn foster_parented_text_coalesces_with_preceding_text_node() {
    let doc = parse_html("<table>A<td>B</td>C</table>");
    let body = doc
        .descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "body"))
        .expect("body element");
    let texts: Vec<&str> = doc
        .children(body)
        .filter_map(|c| match doc.get(c) {
            Some(NodeData::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["AC"]);
}

#[test]
fn mathml_annotation_xml_html_integration_point_keeps_html_children() {
    let doc = parse_html(
        r#"<math><annotation-xml encoding="text/html"><div>ok</div></annotation-xml></math>"#,
    );
    let annot = doc
        .descendants(doc.root())
        .find(|&n| {
            matches!(
                doc.get(n),
                Some(NodeData::Element(e)) if &*e.name.local == "annotation-xml"
            )
        })
        .expect("annotation-xml element");
    let has_div_child = doc
        .children(annot)
        .any(|c| matches!(doc.get(c), Some(NodeData::Element(e)) if &*e.name.local == "div"));
    assert!(
        has_div_child,
        "<div> must remain inside HTML-encoding annotation-xml"
    );
}

#[test]
fn html_fragment_context_preserves_head_and_body() {
    let ctx = QualName::new(None, ns!(html), local_name!("html"));
    let (doc, nodes) = parse_html_fragment("<body><span>hi</span></body>", Some(&ctx));
    let tags: Vec<&str> = nodes
        .iter()
        .filter_map(|&n| match doc.get(n) {
            Some(NodeData::Element(e)) => Some(e.name.local.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec!["head", "body"]);
}

#[test]
fn processing_instructions_follow_whatwg_rules() {
    let doc = parse_html("<body><?something good?><?xml-stylesheet?><?a$><?eof_dropped");
    let body = doc
        .descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "body"))
        .expect("body");
    let children: Vec<&NodeData> = doc.children(body).filter_map(|c| doc.get(c)).collect();
    assert_eq!(children.len(), 3);
    assert_eq!(
        children[0],
        &NodeData::ProcessingInstruction {
            target: "something".into(),
            data: "good".into()
        }
    );
    assert!(matches!(children[1], NodeData::Comment(_)));
    assert!(matches!(children[2], NodeData::Comment(_)));
}

#[test]
fn meta_charset_prescan_skips_html_comments() {
    let html = r#"<!DOCTYPE html><html><head><!--<meta charset="ISO-8859-8">--><meta charset="ISO-8859-5"></head><body></body></html>"#;
    assert_eq!(detect_html_charset(html.as_bytes()), "ISO-8859-5");
    let doc = parse_html(html);
    assert_eq!(doc.character_set(), "ISO-8859-5");
    assert_eq!(doc.compat_mode(), "CSS1Compat");
}

#[test]
fn scripting_off_parses_noscript_children_as_elements() {
    let doc = parse_html_with_scripting("<body><noscript><p>fallback</p></noscript></body>", false);
    let noscript = doc
        .descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "noscript"))
        .expect("noscript");
    let has_p = doc
        .children(noscript)
        .any(|c| matches!(doc.get(c), Some(NodeData::Element(e)) if &*e.name.local == "p"));
    assert!(has_p, "scripting-off must parse <p> inside <noscript>");
}

#[test]
fn search_element_is_special_for_adoption_agency_and_list_closing() {
    let doc = parse_html("<!doctype html><a><search>x</a>y");
    let body = doc
        .descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "body"))
        .expect("body");
    let tags: Vec<&str> = doc
        .children(body)
        .filter_map(|c| match doc.get(c) {
            Some(NodeData::Element(e)) => Some(e.name.local.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec!["a", "search"]);
}

#[test]
fn template_in_head_preserves_frameset_ok_and_nested_forms() {
    let doc = parse_html("<template>x</template><p><frameset>");
    let html_el = doc
        .children(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "html"))
        .expect("html");
    let tags: Vec<&str> = doc
        .children(html_el)
        .filter_map(|c| match doc.get(c) {
            Some(NodeData::Element(e)) => Some(e.name.local.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec!["head", "frameset"]);

    let ctx = QualName::new(None, ns!(html), local_name!("template"));
    let (frag_doc, roots) = parse_html_fragment("<form><form>", Some(&ctx));
    assert_eq!(roots.len(), 1);
    let inner_forms: usize = frag_doc
        .children(roots[0])
        .filter(
            |&c| matches!(frag_doc.get(c), Some(NodeData::Element(e)) if &*e.name.local == "form"),
        )
        .count();
    assert_eq!(inner_forms, 1, "template fragment must allow nested <form>");
}

#[test]
fn select_fragment_ignores_input_and_populates_selectedcontent() {
    let ctx = QualName::new(None, ns!(html), local_name!("select"));
    let (frag_doc, roots) = parse_html_fragment("<input><option>", Some(&ctx));
    let tags: Vec<&str> = roots
        .iter()
        .filter_map(|&n| match frag_doc.get(n) {
            Some(NodeData::Element(e)) => Some(e.name.local.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec!["option"]);

    let doc = parse_html("<select><button><selectedcontent></button><option>X<option selected>Y");
    let sc = doc
        .descendants(doc.root())
        .find(|&n| {
            matches!(
                doc.get(n),
                Some(NodeData::Element(e)) if &*e.name.local == "selectedcontent"
            )
        })
        .expect("selectedcontent");
    assert_eq!(doc.inner_text(sc), "Y");
}

#[test]
fn doctype_empty_system_id_triggers_quirks_mode() {
    let doc = parse_html(
        r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//" ""><html></html>"#,
    );
    assert_eq!(doc.compat_mode(), "BackCompat");
    let limited = parse_html(
        r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01 Transitional//" "http://www.w3.org/TR/html4/loose.dtd"><html></html>"#,
    );
    assert_eq!(limited.compat_mode(), "CSS1Compat");
}

#[test]
fn xml_decl_and_meta_charset_prescan_and_late_head_switch() {
    use rix_html::decode_html_bytes_with_fallback;

    // 1. `<meta>` takes precedence over XML declaration per WHATWG HTML §13.2.3.2.
    let xml_and_meta =
        b"<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n<meta charset=\"windows-1253\">\n\xe6";
    let (dec, cs) = decode_html_bytes_with_fallback(xml_and_meta, None, None);
    assert_eq!(cs, "windows-1253");
    assert!(dec.contains('\u{03B6}'));

    // 2. XML declaration allows control bytes <= 0x20 around `=` and `>` at byte 1025.
    let one_around = b"<?xml version=\"1.0\" encoding\x01=\x01\"windows-1251\"?>\n\xe6";
    let (dec2, cs2) = decode_html_bytes_with_fallback(one_around, None, None);
    assert_eq!(cs2, "windows-1251");
    assert!(dec2.contains('\u{0436}'));

    // 3. UTF-16LE XML declaration heuristic when no BOM or HTTP charset.
    let utf16le_xml = b"<\0?\0x\0>\0";
    let (_, cs3) = decode_html_bytes_with_fallback(utf16le_xml, None, None);
    assert_eq!(cs3, "UTF-16LE");

    // 4. Late `<meta charset>` inside `<head>` past 1,024 bytes switches tentative encoding,
    //    while `<meta>` after `</head>` past 1,024 bytes does NOT.
    let mut after_1kb = b"<!DOCTYPE html><head>".to_vec();
    after_1kb.resize(1030, b'\n');
    after_1kb.extend_from_slice(b"<meta charset=\"windows-1251\"></head><body>\xe6</body>");
    let (dec4, cs4) = decode_html_bytes_with_fallback(&after_1kb, None, Some("windows-1253"));
    assert_eq!(cs4, "windows-1251");
    assert!(dec4.contains('\u{0436}'));

    let mut after_head_1kb = b"<!DOCTYPE html><head></head>".to_vec();
    after_head_1kb.resize(1030, b'\n');
    after_head_1kb.extend_from_slice(b"<meta charset=\"windows-1251\"><body>\xe6</body>");
    let (dec5, cs5) = decode_html_bytes_with_fallback(&after_head_1kb, None, Some("windows-1253"));
    assert_eq!(cs5, "windows-1253");
    assert!(dec5.contains('\u{03B6}'));

    // 5. Non-ASCII-compatible parent fallback (UTF-16LE) is never inherited by child frames.
    let (_, cs6) = decode_html_bytes_with_fallback(b"<p>hello</p>", None, Some("UTF-16LE"));
    assert_eq!(cs6, "UTF-8");
}

#[test]
fn noscript_serialization_respects_scripting_flag_and_escapes_attr_lt_gt() {
    let doc = parse_html("<div><a b=\"<>\"></a><noscript><p>&amp;</p></noscript></div>");
    let div = doc
        .descendants(doc.root())
        .find(|&n| matches!(doc.get(n), Some(NodeData::Element(e)) if &*e.name.local == "div"))
        .expect("div");
    let with_script = doc.to_html_string_with_scripting(div, true);
    assert_eq!(
        with_script,
        "<div><a b=\"&lt;&gt;\"></a><noscript><p>&amp;</p></noscript></div>"
    );
    let without_script = doc.to_html_string_with_scripting(div, false);
    assert_eq!(
        without_script,
        "<div><a b=\"&lt;&gt;\"></a><noscript>&lt;p&gt;&amp;amp;&lt;/p&gt;</noscript></div>"
    );
}
