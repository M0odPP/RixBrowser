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
