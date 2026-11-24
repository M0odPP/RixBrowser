use crate::sink::DomTreeSink;
use encoding_rs::{Decoder, Encoding, UTF_8};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{parse_document, parse_fragment, ParseOpts, Parser};
use markup5ever::{QualName, TokenizerResult};
use rix_dom::node::NodeData;
use rix_dom::tree::Document;
use rix_dom::NodeId;
use std::cell::RefCell;
use std::rc::Rc;

const MAX_CHARSET_SNIFF_BYTES: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ParserStreamStep {
    Script(NodeId),
    Done,
}

pub struct HtmlParserStream {
    parser: Parser<DomTreeSink>,
    document: Rc<RefCell<Document>>,
    finished: bool,
    charset_hint: Option<&'static Encoding>,
    encoding: Option<&'static Encoding>,
    decoder: Option<Decoder>,
    sniff_buf: Vec<u8>,
}

impl Default for HtmlParserStream {
    fn default() -> Self {
        Self::new()
    }
}

impl HtmlParserStream {
    pub fn new() -> Self {
        let doc = Rc::new(RefCell::new(Document::new()));
        Self::with_document(doc)
    }

    pub fn with_document(document: Rc<RefCell<Document>>) -> Self {
        let sink = DomTreeSink::with_document(document.clone());
        let opts = ParseOpts::default();
        let parser = parse_document(sink, opts);
        Self {
            parser,
            document,
            finished: false,
            charset_hint: None,
            encoding: None,
            decoder: None,
            sniff_buf: Vec::new(),
        }
    }

    pub fn with_charset_hint(mut self, content_type_or_label: &str) -> Self {
        self.charset_hint = parse_charset_from_header(content_type_or_label);
        self
    }

    pub fn document(&self) -> Rc<RefCell<Document>> {
        self.document.clone()
    }

    pub fn resolved_encoding_name(&self) -> &'static str {
        self.encoding.or(self.charset_hint).unwrap_or(UTF_8).name()
    }

    pub fn feed(&mut self, chunk: &str) {
        if self.encoding.is_none() {
            self.encoding = Some(UTF_8);
            self.decoder = Some(UTF_8.new_decoder_without_bom_handling());
        }
        self.parser
            .input_buffer
            .push_back(StrTendril::from_slice(chunk));
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.finished {
            return;
        }
        if let Some(decoder) = self.decoder.as_mut() {
            decode_into_buffer(decoder, bytes, false, &mut self.parser);
            return;
        }

        self.sniff_buf.extend_from_slice(bytes);
        if let Some((enc, bom_len)) = Encoding::for_bom(&self.sniff_buf) {
            self.lock_encoding(enc, bom_len);
            return;
        }

        // Wait up to 3 bytes for BOM before trusting an HTTP header hint.
        if let Some(hint) = self.charset_hint {
            if self.sniff_buf.len() >= 3 {
                self.lock_encoding(hint, 0);
            }
            return;
        }

        if let Some(meta_enc) = prescan_meta_charset(&self.sniff_buf) {
            self.lock_encoding(meta_enc, 0);
            return;
        }

        if contains_head_boundary(&self.sniff_buf)
            || self.sniff_buf.len() >= MAX_CHARSET_SNIFF_BYTES
        {
            self.lock_encoding(UTF_8, 0);
        }
    }

    /// Inserts `document.write()` markup directly at the current tokenizer insertion point.
    pub fn insert_document_write(&mut self, html: &str) {
        if !html.is_empty() && !self.finished {
            self.parser
                .input_buffer
                .push_front(StrTendril::from_slice(html));
        }
    }

    pub fn flush_sniff_for_stepping(&mut self) {
        if self.decoder.is_none() && !self.sniff_buf.is_empty() {
            let chosen = Encoding::for_bom(&self.sniff_buf).unwrap_or_else(|| {
                let enc = self
                    .charset_hint
                    .or_else(|| prescan_meta_charset(&self.sniff_buf))
                    .unwrap_or(UTF_8);
                (enc, 0)
            });
            self.lock_encoding(chosen.0, chosen.1);
        }
    }

    pub fn step(&mut self) -> ParserStreamStep {
        if self.finished {
            return ParserStreamStep::Done;
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Script(node_id) => {
                    attach_declarative_shadow_roots(&mut self.document.borrow_mut());
                    return ParserStreamStep::Script(node_id);
                }
                TokenizerResult::Done => return ParserStreamStep::Done,
                TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
    }

    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.flush_sniff_for_stepping();
        if let Some(decoder) = self.decoder.as_mut() {
            decode_into_buffer(decoder, &[], true, &mut self.parser);
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Done => break,
                TokenizerResult::Script(_) | TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
        self.parser.tokenizer.end();
        attach_declarative_shadow_roots(&mut self.document.borrow_mut());
        self.finished = true;
    }

    pub fn finish_bytes(&mut self) {
        self.finish();
    }

    fn lock_encoding(&mut self, enc: &'static Encoding, skip_prefix: usize) {
        self.encoding = Some(enc);
        let mut decoder = enc.new_decoder_without_bom_handling();
        let buffered = std::mem::take(&mut self.sniff_buf);
        if buffered.len() > skip_prefix {
            decode_into_buffer(
                &mut decoder,
                &buffered[skip_prefix..],
                false,
                &mut self.parser,
            );
        }
        self.decoder = Some(decoder);
    }
}

pub fn is_blocking_classic_script(doc: &Document, node: NodeId) -> bool {
    let Some(NodeData::Element(elem)) = doc.get(node) else {
        return false;
    };
    if elem.name.local.as_str() != "script" {
        return false;
    }
    if elem.has_attribute("async") || elem.has_attribute("defer") {
        return false;
    }
    if let Some(kind) = elem.get_attribute("type") {
        if kind.trim().eq_ignore_ascii_case("module") {
            return false;
        }
    }
    true
}

fn decode_into_buffer(
    decoder: &mut Decoder,
    bytes: &[u8],
    last: bool,
    parser: &mut Parser<DomTreeSink>,
) {
    let cap = decoder
        .max_utf8_buffer_length(bytes.len())
        .unwrap_or(bytes.len().saturating_mul(4).max(16));
    let mut out = String::with_capacity(cap.min(64 * 1024));
    let mut offset = 0usize;
    loop {
        out.clear();
        let (_res, read, _had_errors) = decoder.decode_to_string(&bytes[offset..], &mut out, last);
        if !out.is_empty() {
            parser.input_buffer.push_back(StrTendril::from_slice(&out));
        }
        offset += read;
        if offset >= bytes.len() && (out.is_empty() || !last) {
            break;
        }
        if read == 0 && out.is_empty() {
            break;
        }
    }
}

fn parse_charset_from_header(input: &str) -> Option<&'static Encoding> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(pos) = lower.find("charset=") {
        let rest = &trimmed[pos + 8..];
        let val = rest
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        return Encoding::for_label(val.as_bytes());
    }
    Encoding::for_label(trimmed.as_bytes())
}

fn contains_head_boundary(bytes: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    lower.contains("</head>") || lower.contains("<body") || lower.contains("</script>")
}

fn prescan_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let scan_len = bytes.len().min(MAX_CHARSET_SNIFF_BYTES);
    let text = String::from_utf8_lossy(&bytes[..scan_len]);
    let lower = text.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let len = lb.len();
    let mut i = 0usize;

    while i < len {
        if lb[i] != b'<' {
            i += 1;
            continue;
        }
        // WHATWG §13.2.3.2: skip HTML comments `<!-- ... -->`.
        if lower[i..].starts_with("<!--") {
            if let Some(rel) = lower[i + 4..].find("-->") {
                i = i + 4 + rel + 3;
                continue;
            }
            return None;
        }
        // Skip rawtext tags (`<script>`, `<style>`, `<title>`, `<xmp>`).
        for raw_tag in ["<script", "<style", "<title", "<xmp"] {
            if lower[i..].starts_with(raw_tag)
                && lb
                    .get(i + raw_tag.len())
                    .is_some_and(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/')
            {
                let close = format!("</{}", &raw_tag[1..]);
                if let Some(rel) = lower[i + raw_tag.len()..].find(&close) {
                    i = i + raw_tag.len() + rel + close.len();
                    if let Some(gt) = lower[i..].find('>') {
                        i += gt + 1;
                    }
                    continue;
                }
                return None;
            }
        }
        if lower[i..].starts_with("<meta")
            && lb
                .get(i + 5)
                .is_some_and(|&b| b.is_ascii_whitespace() || b == b'/')
        {
            let rel_end = lower[i..].find('>')?;
            let tag = &text[i..=i + rel_end];
            let tag_lower = &lower[i..=i + rel_end];
            if let Some(charset) = extract_attr_value(tag, tag_lower, "charset") {
                if let Some(enc) = Encoding::for_label(charset.trim().as_bytes()) {
                    return Some(enc);
                }
            }
            if extract_attr_value(tag, tag_lower, "http-equiv")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-type"))
            {
                if let Some(content) = extract_attr_value(tag, tag_lower, "content") {
                    if let Some(enc) = parse_charset_from_header(content) {
                        return Some(enc);
                    }
                }
            }
            i += rel_end + 1;
            continue;
        }
        i += 1;
    }
    None
}

fn extract_attr_value<'a>(orig: &'a str, lower: &str, name: &str) -> Option<&'a str> {
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(name) {
        let idx = pos + rel;
        let before_ok = idx == 0 || !lower.as_bytes()[idx - 1].is_ascii_alphanumeric();
        let after = idx + name.len();
        if before_ok {
            let mut cur = after;
            let bytes = lower.as_bytes();
            while cur < bytes.len() && bytes[cur].is_ascii_whitespace() {
                cur += 1;
            }
            if cur < bytes.len() && bytes[cur] == b'=' {
                cur += 1;
                while cur < bytes.len() && bytes[cur].is_ascii_whitespace() {
                    cur += 1;
                }
                if cur >= orig.len() {
                    return None;
                }
                let quote = bytes[cur];
                if quote == b'"' || quote == b'\'' {
                    let start = cur + 1;
                    let end = orig[start..].find(quote as char)?;
                    return Some(&orig[start..start + end]);
                } else {
                    let end = orig[cur..]
                        .find(|c: char| c.is_ascii_whitespace() || c == '>')
                        .unwrap_or(orig.len() - cur);
                    return Some(&orig[cur..cur + end]);
                }
            }
        }
        pos = after;
    }
    None
}

/// Detects the document character encoding from BOM or `<meta charset>` / `<meta http-equiv>`
/// prescan per WHATWG HTML §13.2.3.2, mapping meta UTF-16 labels to `"UTF-8"`.
pub fn detect_html_charset(bytes: &[u8]) -> &'static str {
    if let Some((enc, _)) = Encoding::for_bom(bytes) {
        return enc.name();
    }
    if let Some(enc) = prescan_meta_charset(bytes) {
        // Per WHATWG HTML §13.2.3.2 step 8: UTF-16BE/UTF-16LE in `<meta>` maps to UTF-8.
        if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
            return UTF_8.name();
        }
        return enc.name();
    }
    UTF_8.name()
}

/// Decodes raw HTML response bytes per WHATWG HTML §13.2.3.3 (BOM -> HTTP `charset` ->
/// `<meta>` prescan -> UTF-8), returning the decoded UTF-8 string and canonical encoding name.
pub fn decode_html_bytes(bytes: &[u8], http_charset: Option<&str>) -> (String, &'static str) {
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        let (decoded, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        return (decoded.into_owned(), enc.name());
    }
    let enc = http_charset
        .and_then(|c| {
            let trimmed =
                c.trim_matches(|ch: char| matches!(ch, '\t' | '\n' | '\x0c' | '\r' | ' '));
            Encoding::for_label(trimmed.as_bytes())
        })
        .or_else(|| {
            prescan_meta_charset(bytes).map(|e| {
                if e == encoding_rs::UTF_16LE || e == encoding_rs::UTF_16BE {
                    UTF_8
                } else {
                    e
                }
            })
        })
        .unwrap_or(UTF_8);
    let (decoded, _) = enc.decode_without_bom_handling(bytes);
    (decoded.into_owned(), enc.name())
}

pub fn parse_html(html: &str) -> Document {
    parse_html_with_scripting(html, true)
}

pub fn parse_html_with_scripting(html: &str, scripting_enabled: bool) -> Document {
    let sink = DomTreeSink::with_source_pi_scan(html, scripting_enabled);
    let opts = ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut doc = parse_document(sink, opts).one(html);
    doc.set_character_set(detect_html_charset(html.as_bytes()));
    attach_declarative_shadow_roots(&mut doc);
    doc
}

pub fn parse_html_fragment(html: &str, context: Option<&QualName>) -> (Document, Vec<NodeId>) {
    parse_html_fragment_with_scripting(html, context, true)
}

pub fn parse_html_fragment_with_scripting(
    html: &str,
    context: Option<&QualName>,
    scripting_enabled: bool,
) -> (Document, Vec<NodeId>) {
    let sink = DomTreeSink::with_source_pi_scan(html, scripting_enabled);
    let opts = ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled,
            ..Default::default()
        },
        ..Default::default()
    };
    let context_name = match context {
        Some(qname) => qname.clone(),
        None => QualName::new(
            None,
            markup5ever::ns!(html),
            markup5ever::local_name!("body"),
        ),
    };
    let mut doc = parse_fragment(sink, opts, context_name, Vec::new(), true).one(html);
    attach_declarative_shadow_roots(&mut doc);
    // Per html5ever `parse_fragment`, parsed fragment nodes live as children of the synthetic
    // root `<html>` wrapper element (plus any trailing sibling nodes like comments after `</html>`).
    let mut fragment_nodes = Vec::new();
    let mut found_html_wrapper = false;
    for child in doc.children(doc.root()) {
        if !found_html_wrapper {
            if let Some(NodeData::Element(elem)) = doc.get(child) {
                if &*elem.name.local == "html" {
                    found_html_wrapper = true;
                    for node in doc.children(child) {
                        fragment_nodes.push(node);
                    }
                    continue;
                }
            }
        }
        fragment_nodes.push(child);
    }
    (doc, fragment_nodes)
}

pub fn attach_declarative_shadow_roots(doc: &mut Document) {
    attach_declarative_shadow_roots_in_subtree(doc, doc.root());
}

fn attach_declarative_shadow_roots_in_subtree(doc: &mut Document, root: NodeId) {
    let children: Vec<NodeId> = doc.children(root).collect();
    for child in children {
        let template_info = if let Some(NodeData::Element(elem)) = doc.get(child) {
            if &*elem.name.local == "template" {
                let shadow_mode = elem
                    .get_attribute("shadowrootmode")
                    .or_else(|| elem.get_attribute("shadowroot"))
                    .map(|s| s.to_ascii_lowercase());
                if let Some(mode_lower) = shadow_mode {
                    if mode_lower == "open" || mode_lower == "closed" {
                        let delegates_focus = elem.has_attribute("shadowrootdelegatesfocus");
                        let clonable = elem.has_attribute("shadowrootclonable");
                        let frag = elem.template_contents.unwrap_or(child);
                        Some((mode_lower, delegates_focus, clonable, frag))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some((mode_lower, delegates_focus, clonable, frag)) = template_info {
            let parent = doc.parent(child);
            if let Some(parent) = parent {
                if can_attach_declarative_shadow(doc, parent) {
                    let shadow_root = doc.new_node(NodeData::ShadowRoot {
                        host: parent,
                        mode: mode_lower,
                        delegates_focus,
                        clonable,
                    });
                    if let Some(NodeData::Element(parent_elem)) = doc.get_mut(parent) {
                        parent_elem.shadow_root = Some(shadow_root);
                    }
                    let template_children: Vec<NodeId> = doc.children(frag).collect();
                    for tc in template_children {
                        doc.append_child(shadow_root, tc);
                    }
                    doc.remove_child(child);
                    attach_declarative_shadow_roots_in_subtree(doc, shadow_root);
                    continue;
                }
            }
        }

        attach_declarative_shadow_roots_in_subtree(doc, child);
        let shadow_root_to_recurse = if let Some(NodeData::Element(elem)) = doc.get(child) {
            elem.shadow_root
        } else {
            None
        };
        if let Some(sr) = shadow_root_to_recurse {
            attach_declarative_shadow_roots_in_subtree(doc, sr);
        }
    }
}

fn can_attach_declarative_shadow(doc: &Document, host: NodeId) -> bool {
    let Some(NodeData::Element(elem)) = doc.get(host) else {
        return false;
    };
    if elem.shadow_root.is_some() {
        return false;
    }
    let html_ns = &*elem.name.ns == "http://www.w3.org/1999/xhtml";
    html_ns && rix_dom::is_valid_shadow_host_name(&elem.name.local)
}
