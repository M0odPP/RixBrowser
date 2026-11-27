use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use markup5ever::{Attribute, QualName};
use rix_dom::node::{ElementData, NodeData};
use rix_dom::tree::Document;
use rix_dom::NodeId;
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

#[derive(Debug, Clone)]
enum PiCommentAction {
    KeepComment,
    ProcessingInstruction { target: String, data: String },
    DropEof,
}

pub struct DomTreeSink {
    document: Rc<RefCell<Document>>,
    quirks_mode: Cell<QuirksMode>,
    pi_actions: RefCell<VecDeque<PiCommentAction>>,
    dropped_nodes: RefCell<HashSet<NodeId>>,
}

impl Default for DomTreeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl DomTreeSink {
    pub fn new() -> Self {
        Self {
            document: Rc::new(RefCell::new(Document::new())),
            quirks_mode: Cell::new(QuirksMode::NoQuirks),
            pi_actions: RefCell::new(VecDeque::new()),
            dropped_nodes: RefCell::new(HashSet::new()),
        }
    }

    pub fn with_document(document: Rc<RefCell<Document>>) -> Self {
        Self {
            document,
            quirks_mode: Cell::new(QuirksMode::NoQuirks),
            pi_actions: RefCell::new(VecDeque::new()),
            dropped_nodes: RefCell::new(HashSet::new()),
        }
    }

    /// Creates a tree sink pre-indexed with WHATWG HTML Processing Instruction classifications
    /// when `source` contains `"<?"`. Zero overhead for documents without `"<?"`.
    pub fn with_source_pi_scan(source: &str, scripting_enabled: bool) -> Self {
        let sink = Self::new();
        if source.contains("<?") {
            *sink.pi_actions.borrow_mut() = scan_pi_comment_actions(source, scripting_enabled);
        }
        sink
    }

    pub fn document(&self) -> Rc<RefCell<Document>> {
        self.document.clone()
    }

    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode.get()
    }
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

fn is_pi_target_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_pi_target_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Scans an HTML source string in tokenizer order and records how each comment/bogus-comment
/// emitted by `html5ever` maps to a WHATWG HTML Processing Instruction, Comment, or EOF drop.
fn scan_pi_comment_actions(html: &str, scripting_enabled: bool) -> VecDeque<PiCommentAction> {
    let mut actions = VecDeque::new();
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Standard HTML comment `<!-- ... -->`
        if html[i..].starts_with("<!--") {
            actions.push_back(PiCommentAction::KeepComment);
            i += 4;
            if i < len && bytes[i] == b'>' {
                i += 1;
                continue;
            }
            if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'>' {
                i += 2;
                continue;
            }
            if let Some(rel) = html[i..].find("-->") {
                i += rel + 3;
            } else {
                break;
            }
            continue;
        }
        // Processing instruction candidate `<?...`
        if i + 1 < len && bytes[i + 1] == b'?' {
            let start = i + 2;
            let mut end = start;
            while end < len && bytes[end] != b'>' {
                end += 1;
            }
            let closed = end < len && bytes[end] == b'>';
            if start >= len {
                // `<?` at EOF
                actions.push_back(PiCommentAction::DropEof);
                break;
            }
            if !is_pi_target_start(bytes[start]) {
                // Invalid PI target start -> bogus comment (e.g. `<? `, `<?1st`, `<?-x`)
                actions.push_back(PiCommentAction::KeepComment);
                i = if closed { end + 1 } else { len };
                continue;
            }
            let mut t_end = start + 1;
            while t_end < end && is_pi_target_cont(bytes[t_end]) {
                t_end += 1;
            }
            let target = &html[start..t_end];
            let valid_delim = t_end == end || is_ascii_ws(bytes[t_end]) || bytes[t_end] == b'?';
            let is_xml_reserved = target.len() >= 3 && target[..3].eq_ignore_ascii_case("xml");
            if !valid_delim || is_xml_reserved {
                actions.push_back(PiCommentAction::KeepComment);
            } else if !closed {
                actions.push_back(PiCommentAction::DropEof);
            } else {
                let mut rest = &html[t_end..end];
                if rest.as_bytes().first().is_some_and(|&b| is_ascii_ws(b)) {
                    rest = rest.trim_start_matches(|c: char| {
                        matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
                    });
                }
                let normalized = rest.replace("\r\n", "\n").replace('\r', "\n");
                let data = normalized
                    .strip_suffix('?')
                    .unwrap_or(&normalized)
                    .to_string();
                actions.push_back(PiCommentAction::ProcessingInstruction {
                    target: target.to_string(),
                    data,
                });
            }
            i = if closed { end + 1 } else { len };
            continue;
        }
        // `<!DOCTYPE ...>` vs other bogus `<!...>` comments
        if i + 1 < len && bytes[i + 1] == b'!' {
            let after = &html[i + 2..];
            let is_doctype = after.len() >= 7 && after[..7].eq_ignore_ascii_case("doctype");
            let is_cdata = after.starts_with("[CDATA[");
            if is_cdata {
                if let Some(rel) = after.find("]]>") {
                    i = i + 2 + rel + 3;
                } else {
                    break;
                }
                continue;
            }
            if !is_doctype {
                actions.push_back(PiCommentAction::KeepComment);
            }
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
            if i < len {
                i += 1;
            }
            continue;
        }
        // Element start tag: skip attributes and rawtext/RCDATA bodies so `<?...>` inside them stays text.
        if i + 1 < len && bytes[i + 1].is_ascii_alphabetic() {
            let tstart = i + 1;
            let mut tend = tstart;
            while tend < len && (bytes[tend].is_ascii_alphanumeric() || bytes[tend] == b'-') {
                tend += 1;
            }
            let tag = &html[tstart..tend];
            let mut p = tend;
            let mut quote = 0u8;
            while p < len {
                let b = bytes[p];
                if quote != 0 {
                    if b == quote {
                        quote = 0;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = b;
                } else if b == b'>' {
                    p += 1;
                    break;
                }
                p += 1;
            }
            i = p;
            let is_raw = tag.eq_ignore_ascii_case("script")
                || tag.eq_ignore_ascii_case("style")
                || tag.eq_ignore_ascii_case("textarea")
                || tag.eq_ignore_ascii_case("title")
                || tag.eq_ignore_ascii_case("xmp")
                || tag.eq_ignore_ascii_case("iframe")
                || tag.eq_ignore_ascii_case("noembed")
                || tag.eq_ignore_ascii_case("noframes")
                || (scripting_enabled && tag.eq_ignore_ascii_case("noscript"));
            if is_raw {
                let close_pat = format!("</{}", tag.to_ascii_lowercase());
                let lower_rest = html[i..].to_ascii_lowercase();
                if let Some(rel) = lower_rest.find(&close_pat) {
                    i += rel;
                    while i < len && bytes[i] != b'>' {
                        i += 1;
                    }
                    if i < len {
                        i += 1;
                    }
                } else {
                    break;
                }
            }
            continue;
        }
        i += 1;
    }

    actions
}

fn is_host_including_inclusive_ancestor(
    doc: &Document,
    ancestor: NodeId,
    mut node: NodeId,
) -> bool {
    let mut visited = 0usize;
    loop {
        if node == ancestor {
            return true;
        }
        visited += 1;
        if visited > 4096 {
            return true;
        }
        if let Some(p) = doc.parent(node) {
            node = p;
            continue;
        }
        if let Some(NodeData::ShadowRoot { host, .. }) = doc.get(node) {
            node = *host;
            continue;
        }
        if matches!(doc.get(node), Some(NodeData::DocumentFragment)) {
            if let Some(tmpl_owner) = doc.descendants(doc.root()).find(|&cand| {
                matches!(
                    doc.get(cand),
                    Some(NodeData::Element(e)) if e.template_contents == Some(node)
                )
            }) {
                node = tmpl_owner;
                continue;
            }
        }
        return false;
    }
}

impl TreeSink for DomTreeSink {
    type Handle = NodeId;
    type Output = Document;
    type ElemName<'a> = std::cell::Ref<'a, QualName>;

    fn finish(self) -> Self::Output {
        match Rc::try_unwrap(self.document) {
            Ok(ref_cell) => ref_cell.into_inner(),
            Err(rc) => rc.borrow().clone(),
        }
    }

    fn parse_error(&self, _msg: std::borrow::Cow<'static, str>) {}

    fn get_document(&self) -> Self::Handle {
        self.document.borrow().root()
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
        let code = match mode {
            QuirksMode::NoQuirks => 0,
            QuirksMode::LimitedQuirks => 1,
            QuirksMode::Quirks => 2,
        };
        self.document.borrow_mut().set_quirks_mode(code);
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
        x == y
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
        std::cell::Ref::map(self.document.borrow(), |doc| match doc.get(*target) {
            Some(NodeData::Element(elem)) => &elem.name,
            _ => panic!("Expected element node in elem_name"),
        })
    }

    fn create_element(
        &self,
        name: QualName,
        html_attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> Self::Handle {
        let mut elem_data = ElementData::new(name);
        for attr in html_attrs {
            elem_data
                .attributes
                .push((attr.name, attr.value.to_string()));
        }
        elem_data.sync_cached_attrs();

        // WHATWG HTML §13.2.6.1: capture `is` value for customized built-in elements
        // and `customelementregistry` boolean attribute for scoped null-registry elements.
        if &*elem_data.name.ns == "http://www.w3.org/1999/xhtml" {
            if let Some(is_val) = elem_data.get_attribute("is").map(str::to_string) {
                elem_data.is_value = Some(is_val);
                elem_data.set_custom_element_state(rix_dom::CustomElementState::Undefined);
            }
            if elem_data.has_attribute("customelementregistry") {
                elem_data.null_registry = true;
            }
        }

        if &*elem_data.name.ns == "http://www.w3.org/1999/xhtml"
            && &*elem_data.name.local == "template"
        {
            let frag = self
                .document
                .borrow_mut()
                .new_node(NodeData::DocumentFragment);
            elem_data.template_contents = Some(frag);
        }

        if &*elem_data.name.local == "iframe" {
            if let Some(srcdoc) = elem_data
                .attributes
                .iter()
                .find(|(k, _)| &*k.local == "srcdoc")
                .map(|(_, v)| v.clone())
            {
                let parsed = crate::parse_html(&srcdoc);
                let parsed_root = parsed.root();
                let mut doc = self.document.borrow_mut();
                let sub_root = doc.new_node(NodeData::Document);
                for child in parsed.children(parsed_root) {
                    let cloned = doc.clone_subtree_from(&parsed, child);
                    doc.append_child(sub_root, cloned);
                }
                elem_data.content_document = Some(sub_root);
            }
        }

        self.document
            .borrow_mut()
            .new_node(NodeData::Element(elem_data))
    }

    fn create_comment(&self, text: StrTendril) -> Self::Handle {
        let action = self.pi_actions.borrow_mut().pop_front();
        match action {
            Some(PiCommentAction::ProcessingInstruction { target, data }) => self
                .document
                .borrow_mut()
                .create_processing_instruction(&target, &data),
            Some(PiCommentAction::DropEof) => {
                let placeholder = self
                    .document
                    .borrow_mut()
                    .new_node(NodeData::Comment(String::new()));
                self.dropped_nodes.borrow_mut().insert(placeholder);
                placeholder
            }
            _ => self
                .document
                .borrow_mut()
                .new_node(NodeData::Comment(text.to_string())),
        }
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Self::Handle {
        self.document
            .borrow_mut()
            .create_processing_instruction(&target, &data)
    }

    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
        let mut doc = self.document.borrow_mut();
        // WHATWG HTML §13.2.6.1: if the adjusted insertion location's parent is an HTML
        // `<template>` element, redirect into its template contents fragment.
        let target_parent = match doc.get(*parent) {
            Some(NodeData::Element(e))
                if &*e.name.ns == "http://www.w3.org/1999/xhtml"
                    && &*e.name.local == "template" =>
            {
                e.template_contents.unwrap_or(*parent)
            }
            _ => *parent,
        };
        let parent_is_doc = matches!(doc.get(target_parent), Some(NodeData::Document));
        match child {
            NodeOrText::AppendNode(node) => {
                if self.dropped_nodes.borrow().contains(&node) {
                    return;
                }
                if is_host_including_inclusive_ancestor(&doc, node, target_parent) {
                    return;
                }
                if parent_is_doc
                    && matches!(doc.get(node), Some(NodeData::Element(_)))
                    && doc
                        .children(target_parent)
                        .any(|c| matches!(doc.get(c), Some(NodeData::Element(_))))
                {
                    return;
                }
                doc.append_child(target_parent, node);
            }
            NodeOrText::AppendText(text) => {
                if parent_is_doc {
                    return;
                }
                let last_child = doc.children(target_parent).last();
                if let Some(last) = last_child {
                    if let Some(NodeData::Text(existing)) = doc.get_mut(last) {
                        existing.push_str(&text);
                        return;
                    }
                }
                let text_node = doc.create_text(&text);
                doc.append_child(target_parent, text_node);
            }
        }
    }

    fn append_before_sibling(&self, sibling: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
        let mut doc = self.document.borrow_mut();
        let Some(parent_id) = doc.parent(*sibling) else {
            return;
        };
        // WHATWG HTML §13.2.6.1 ("appropriate place for inserting a node" step 3):
        // if a script moved `<table>` directly under a `<template>` element, foster-parenting
        // redirects into `<template>.content` (appended as last child).
        if let Some(NodeData::Element(e)) = doc.get(parent_id) {
            if &*e.name.ns == "http://www.w3.org/1999/xhtml" && &*e.name.local == "template" {
                if let Some(frag) = e.template_contents {
                    drop(doc);
                    self.append(&frag, new_node);
                    return;
                }
            }
        }
        let parent_is_doc = matches!(doc.get(parent_id), Some(NodeData::Document));
        match new_node {
            NodeOrText::AppendNode(node) => {
                if self.dropped_nodes.borrow().contains(&node) {
                    return;
                }
                if is_host_including_inclusive_ancestor(&doc, node, parent_id) {
                    return;
                }
                // WHATWG DOM §4.2.4 / HTML §13.2.6.1: a `Document` node cannot accept a second
                // element child (e.g., when a script moves `<table>` directly under `Document`
                // and subsequent tokens trigger foster parenting before `<table>`).
                if parent_is_doc
                    && matches!(doc.get(node), Some(NodeData::Element(_)))
                    && doc
                        .children(parent_id)
                        .any(|c| matches!(doc.get(c), Some(NodeData::Element(_))))
                {
                    return;
                }
                doc.insert_before(*sibling, node);
            }
            NodeOrText::AppendText(text) => {
                // WHATWG DOM §4.2.4: a `Text` node cannot be inserted directly under `Document`.
                if parent_is_doc {
                    return;
                }
                // Per WHATWG HTML §13.2.6.1 ("insert a character"): merge with immediately
                // preceding Text sibling when present (critical for foster-parented text).
                if let Some(prev) = doc.previous_sibling(*sibling) {
                    if let Some(NodeData::Text(existing)) = doc.get_mut(prev) {
                        existing.push_str(&text);
                        return;
                    }
                }
                let text_node = doc.create_text(&text);
                doc.insert_before(*sibling, text_node);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        let has_parent = self.document.borrow().parent(*element).is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let mut doc = self.document.borrow_mut();
        let root = doc.root();
        let doctype = doc.new_node(NodeData::DocumentType {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
        doc.append_child(root, doctype);
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
        let mut doc = self.document.borrow_mut();
        if let Some(NodeData::Element(elem)) = doc.get_mut(*target) {
            let mut changed = false;
            for attr in attrs {
                let exists = elem.attributes.iter().any(|(qname, _)| qname == &attr.name);
                if !exists {
                    elem.attributes.push((attr.name, attr.value.to_string()));
                    changed = true;
                }
            }
            if changed {
                elem.sync_cached_attrs();
            }
        }
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
        self.document.borrow_mut().remove_child(*target);
    }

    fn reparent_children(&self, node: &Self::Handle, new_parent: &Self::Handle) {
        let mut doc = self.document.borrow_mut();
        let children: Vec<NodeId> = doc.children(*node).collect();
        for child in children {
            if !is_host_including_inclusive_ancestor(&doc, child, *new_parent) {
                doc.append_child(*new_parent, child);
            }
        }
    }

    fn associate_with_form(
        &self,
        target: &Self::Handle,
        form: &Self::Handle,
        _nodes: (&Self::Handle, Option<&Self::Handle>),
    ) {
        let mut doc = self.document.borrow_mut();
        if let Some(NodeData::Element(elem)) = doc.get_mut(*target) {
            elem.form_owner = Some(*form);
        }
    }

    fn mark_script_already_started(&self, _node: &Self::Handle) {}

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
        let doc = self.document.borrow();
        match doc.get(*target) {
            Some(NodeData::Element(elem)) => elem.template_contents.unwrap_or(*target),
            _ => *target,
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Self::Handle) -> bool {
        let doc = self.document.borrow();
        let Some(NodeData::Element(elem)) = doc.get(*handle) else {
            return false;
        };
        if &*elem.name.ns != "http://www.w3.org/1998/Math/MathML"
            || &*elem.name.local != "annotation-xml"
        {
            return false;
        }
        elem.get_attribute("encoding").is_some_and(|enc| {
            enc.eq_ignore_ascii_case("text/html")
                || enc.eq_ignore_ascii_case("application/xhtml+xml")
        })
    }
}
