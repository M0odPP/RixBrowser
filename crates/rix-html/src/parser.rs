use crate::sink::DomTreeSink;
use crate::tree_builder::{TreeBuilder, TreeBuilderOpts};
use encoding_rs::{Decoder, Encoding, UTF_8};
use html5ever::interface::tree_builder::{create_element, TreeSink};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{BufferQueue, Tokenizer, TokenizerOpts};
use markup5ever::{QualName, TokenizerResult};
use rix_dom::node::NodeData;
use rix_dom::tree::Document;
use rix_dom::NodeId;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

const MAX_CHARSET_SNIFF_BYTES: usize = 1024;

struct RixHtmlParser {
    tokenizer: Tokenizer<TreeBuilder<NodeId, DomTreeSink>>,
    input_buffer: BufferQueue,
}

impl RixHtmlParser {
    fn new_document(sink: DomTreeSink, opts: TreeBuilderOpts) -> Self {
        let tb = TreeBuilder::new(sink, opts);
        let tokenizer = Tokenizer::new(tb, TokenizerOpts::default());
        Self {
            tokenizer,
            input_buffer: BufferQueue::default(),
        }
    }

    fn new_fragment(
        sink: DomTreeSink,
        context_name: QualName,
        form_elem: Option<NodeId>,
        scripting_enabled: bool,
        opts: TreeBuilderOpts,
    ) -> Self {
        let context_elem = create_element(&sink, context_name, Vec::new());
        let tb = TreeBuilder::new_for_fragment(sink, context_elem, form_elem, opts);
        let tok_opts = TokenizerOpts {
            initial_state: Some(tb.tokenizer_state_for_context_elem(scripting_enabled)),
            ..TokenizerOpts::default()
        };
        let tokenizer = Tokenizer::new(tb, tok_opts);
        Self {
            tokenizer,
            input_buffer: BufferQueue::default(),
        }
    }

    fn parse_one(self, html: &str) -> Document {
        if !html.is_empty() {
            self.input_buffer.push_back(StrTendril::from_slice(html));
        }
        loop {
            if matches!(
                self.tokenizer.feed(&self.input_buffer),
                TokenizerResult::Done
            ) {
                break;
            }
        }
        self.tokenizer.end();
        self.tokenizer.sink.sink.finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParserStreamStep {
    Script(NodeId),
    CustomElement(NodeId),
    Done,
}

pub struct HtmlParserStream {
    parser: RixHtmlParser,
    document: Rc<RefCell<Document>>,
    finished: bool,
    charset_hint: Option<&'static Encoding>,
    encoding: Option<&'static Encoding>,
    decoder: Option<Decoder>,
    sniff_buf: Vec<u8>,
    has_declarative_shadow: bool,
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
        Self::with_document_and_scripting(document, true)
    }

    pub fn with_document_and_scripting(
        document: Rc<RefCell<Document>>,
        scripting_enabled: bool,
    ) -> Self {
        let doc_root = document.borrow().root();
        Self::with_document_root_and_scripting(document, doc_root, scripting_enabled)
    }

    pub fn with_document_root_and_scripting(
        document: Rc<RefCell<Document>>,
        doc_root: NodeId,
        scripting_enabled: bool,
    ) -> Self {
        let sink = DomTreeSink::with_document_root(document.clone(), doc_root);
        let opts = TreeBuilderOpts {
            scripting_enabled,
            ..Default::default()
        };
        let parser = RixHtmlParser::new_document(sink, opts);
        Self {
            parser,
            document,
            finished: false,
            charset_hint: None,
            encoding: None,
            decoder: None,
            sniff_buf: Vec::new(),
            has_declarative_shadow: false,
        }
    }

    pub fn quirks_mode_code(&self) -> u8 {
        match self.parser.tokenizer.sink.sink.quirks_mode() {
            html5ever::tree_builder::QuirksMode::NoQuirks => 0,
            html5ever::tree_builder::QuirksMode::LimitedQuirks => 1,
            html5ever::tree_builder::QuirksMode::Quirks => 2,
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
        if !self.has_declarative_shadow
            && chunk
                .as_bytes()
                .windows(14)
                .any(|w| w.eq_ignore_ascii_case(b"shadowrootmode"))
        {
            self.has_declarative_shadow = true;
        }
        self.parser
            .input_buffer
            .push_back(StrTendril::from_slice(chunk));
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.finished {
            return;
        }
        if !self.has_declarative_shadow
            && bytes
                .windows(14)
                .any(|w| w.eq_ignore_ascii_case(b"shadowrootmode"))
        {
            self.has_declarative_shadow = true;
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

    /// Saves the current unparsed input tail after the insertion point and queues `html`
    /// for synchronous tokenization per WHATWG HTML §13.2.4.1. Callers MUST pair this with
    /// [`Self::end_document_write`] after pumping all written `<script>` elements.
    pub fn begin_document_write(&mut self, html: &str) -> VecDeque<StrTendril> {
        let mut saved = VecDeque::new();
        if html.is_empty() || self.finished {
            return saved;
        }
        while let Some(chunk) = self.parser.input_buffer.pop_front() {
            saved.push_back(chunk);
        }
        self.parser
            .input_buffer
            .push_back(StrTendril::from_slice(html));
        saved
    }

    /// Restores the saved unparsed input tail after a `document.write()` call completes.
    pub fn end_document_write(&mut self, mut saved: VecDeque<StrTendril>) {
        while let Some(chunk) = saved.pop_front() {
            self.parser.input_buffer.push_back(chunk);
        }
    }

    /// Inserts `document.write()` markup at the current tokenizer insertion point in FIFO order
    /// and synchronously tokenizes it into the live DOM per WHATWG HTML §8.4.5.
    pub fn insert_document_write(&mut self, html: &str) -> Option<NodeId> {
        if html.is_empty() || self.finished {
            return None;
        }
        let saved = self.begin_document_write(html);
        let script = self.pump_written_buffer();
        self.end_document_write(saved);
        script
    }

    /// Continues tokenizing only the currently written `document.write()` slice without
    /// consuming any outer saved tail.
    pub fn pump_written_buffer(&mut self) -> Option<NodeId> {
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Script(node_id) => {
                    if self.has_declarative_shadow {
                        attach_declarative_shadow_roots(&mut self.document.borrow_mut());
                    }
                    return Some(node_id);
                }
                TokenizerResult::Done => return None,
                TokenizerResult::EncodingIndicator(_) => continue,
            }
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

    pub fn set_pause_on_custom_elements(&mut self, enabled: bool) {
        self.parser
            .tokenizer
            .sink
            .set_pause_on_custom_elements(enabled);
    }

    pub fn finish_pending_custom_element_insert(&mut self) -> bool {
        let Some(ce_id) = self.parser.tokenizer.sink.pending_custom_element_handle() else {
            return false;
        };
        let already_reparented = self.document.borrow().parent(ce_id).is_some();
        self.parser
            .tokenizer
            .sink
            .finish_pending_custom_element_insert(already_reparented);
        !already_reparented && self.document.borrow().parent(ce_id).is_some()
    }

    pub fn step(&mut self) -> ParserStreamStep {
        if self.finished {
            return ParserStreamStep::Done;
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Script(node_id) => {
                    if self.parser.tokenizer.sink.pending_custom_element_handle() == Some(node_id) {
                        return ParserStreamStep::CustomElement(node_id);
                    }
                    if self.has_declarative_shadow {
                        attach_declarative_shadow_roots(&mut self.document.borrow_mut());
                    }
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
        self.finish_pending_custom_element_insert();
        self.flush_sniff_for_stepping();
        if let Some(decoder) = self.decoder.as_mut() {
            decode_into_buffer(decoder, &[], true, &mut self.parser);
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Done => break,
                TokenizerResult::Script(_) => {
                    self.finish_pending_custom_element_insert();
                }
                TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
        self.parser.tokenizer.end();
        {
            let mut doc = self.document.borrow_mut();
            attach_declarative_shadow_roots(&mut doc);
            sync_selectedcontent_elements(&mut doc);
        }
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

/// Incremental WHATWG HTML fragment parser supporting chunked `write()`/`close()`
/// streaming (WHATWG HTML §8.4 streaming methods).
pub struct HtmlFragmentParserStream {
    parser: RixHtmlParser,
    document: Rc<RefCell<Document>>,
    finished: bool,
}

impl HtmlFragmentParserStream {
    #[must_use]
    pub fn new(context: Option<&QualName>, scripting_enabled: bool) -> Self {
        let document = Rc::new(RefCell::new(Document::new()));
        let sink = DomTreeSink::with_document(Rc::clone(&document));
        let opts = TreeBuilderOpts {
            scripting_enabled,
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
        let parser = RixHtmlParser::new_fragment(sink, context_name, None, true, opts);
        Self {
            parser,
            document,
            finished: false,
        }
    }

    #[must_use]
    pub fn document(&self) -> Rc<RefCell<Document>> {
        Rc::clone(&self.document)
    }

    /// Feeds `chunk` into the tokenizer buffer without advancing yet.
    pub fn push_chunk(&mut self, chunk: &str) {
        if !self.finished && !chunk.is_empty() {
            self.parser
                .input_buffer
                .push_back(StrTendril::from_slice(chunk));
        }
    }

    /// Advances the tokenizer until the next closed `<script>` element or buffer exhaustion.
    pub fn step(&mut self) -> ParserStreamStep {
        if self.finished {
            return ParserStreamStep::Done;
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Script(node_id) => return ParserStreamStep::Script(node_id),
                TokenizerResult::Done => return ParserStreamStep::Done,
                TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
    }

    /// Signals EOF to the tokenizer/tree builder, flushing pending text and open elements.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        loop {
            match self.parser.tokenizer.feed(&self.parser.input_buffer) {
                TokenizerResult::Done => break,
                TokenizerResult::Script(_) | TokenizerResult::EncodingIndicator(_) => continue,
            }
        }
        self.parser.tokenizer.end();
        self.finished = true;
    }

    /// Returns the top-level parsed fragment nodes inside the synthetic `<html>` wrapper.
    #[must_use]
    pub fn fragment_roots(&self) -> Vec<NodeId> {
        let doc = self.document.borrow();
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
        fragment_nodes
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

fn decode_into_buffer(decoder: &mut Decoder, bytes: &[u8], last: bool, parser: &mut RixHtmlParser) {
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

/// WHATWG HTML §13.2.3.2 XML declaration encoding prescan (`<?xml ... encoding="..."?>` within
/// the first 1,024 bytes; `>` itself need not be inside the first 1,024 bytes).
fn prescan_xml_declaration(bytes: &[u8]) -> Option<&'static Encoding> {
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let scan = &bytes[5..bytes.len().min(MAX_CHARSET_SNIFF_BYTES)];
    let decl = match scan.iter().position(|&b| b == b'>') {
        Some(gt) => &scan[..gt],
        None => scan,
    };
    let pos = decl.windows(8).position(|w| w == b"encoding")?;
    let mut p = pos + 8;
    while p < decl.len() && decl[p] <= 0x20 {
        p += 1;
    }
    if p >= decl.len() || decl[p] != b'=' {
        return None;
    }
    p += 1;
    while p < decl.len() && decl[p] <= 0x20 {
        p += 1;
    }
    if p >= decl.len() || (decl[p] != b'"' && decl[p] != b'\'') {
        return None;
    }
    let quote = decl[p];
    let start = p + 1;
    let rel = decl[start..].iter().position(|&b| b == quote)?;
    let label = &decl[start..start + rel];
    if label.is_empty()
        || label.first().is_some_and(|&b| b <= 0x20)
        || label.last().is_some_and(|&b| b <= 0x20)
    {
        return None;
    }
    let enc = Encoding::for_label(label)?;
    Some(
        if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
            UTF_8
        } else {
            enc
        },
    )
}

fn find_ascii_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// Parses HTML attributes from a `<meta ...>` tag byte slice per WHATWG HTML §13.2.3.2,
/// properly respecting quoted attribute values so `charset=` inside `content="..."` is never
/// confused with a standalone `charset` attribute.
fn parse_meta_tag_attrs(tag_bytes: &[u8]) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let len = tag_bytes.len();
    let mut i = 0usize;
    while i < len {
        while i < len && matches!(tag_bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/') {
            i += 1;
        }
        if i >= len || tag_bytes[i] == b'>' {
            break;
        }
        let name_start = i;
        while i < len
            && !matches!(
                tag_bytes[i],
                b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'=' | b'/' | b'>'
            )
        {
            i += 1;
        }
        let name = String::from_utf8_lossy(&tag_bytes[name_start..i]).to_ascii_lowercase();
        while i < len && matches!(tag_bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
            i += 1;
        }
        let value = if i < len && tag_bytes[i] == b'=' {
            i += 1;
            while i < len && matches!(tag_bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') {
                i += 1;
            }
            if i < len && (tag_bytes[i] == b'"' || tag_bytes[i] == b'\'') {
                let q = tag_bytes[i];
                i += 1;
                let v_start = i;
                while i < len && tag_bytes[i] != q {
                    i += 1;
                }
                let v = String::from_utf8_lossy(&tag_bytes[v_start..i]).into_owned();
                if i < len {
                    i += 1;
                }
                v
            } else {
                let v_start = i;
                while i < len
                    && !matches!(tag_bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'>')
                {
                    i += 1;
                }
                String::from_utf8_lossy(&tag_bytes[v_start..i]).into_owned()
            }
        } else {
            String::new()
        };
        if !name.is_empty() && !attrs.iter().any(|(k, _): &(String, String)| k == &name) {
            attrs.push((name, value));
        }
    }
    attrs
}

fn prescan_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let len = bytes.len();
    let limit = len.min(MAX_CHARSET_SNIFF_BYTES);
    let mut i = 0usize;

    while i < limit {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // WHATWG §13.2.3.2: skip HTML comments `<!-- ... -->`.
        if bytes[i..].starts_with(b"<!--") {
            if let Some(rel) = bytes[i + 4..].windows(3).position(|w| w == b"-->") {
                i = i + 4 + rel + 3;
                continue;
            }
            break;
        }
        // WHATWG §13.2.3.2: skip `<!`, `</`, `<?` up to the next `>`.
        if i + 1 < len && matches!(bytes[i + 1], b'!' | b'/' | b'?') {
            if let Some(gt) = bytes[i + 2..].iter().position(|&b| b == b'>') {
                i = i + 2 + gt + 1;
                continue;
            }
            break;
        }
        // Skip rawtext tags (`<script>`, `<style>`, `<title>`, `<xmp>`).
        let mut skipped_raw = false;
        for (open, close) in [
            (&b"<script"[..], &b"</script"[..]),
            (&b"<style"[..], &b"</style"[..]),
            (&b"<title"[..], &b"</title"[..]),
            (&b"<xmp"[..], &b"</xmp"[..]),
        ] {
            if bytes[i..].len() > open.len()
                && bytes[i..i + open.len()].eq_ignore_ascii_case(open)
                && matches!(
                    bytes[i + open.len()],
                    b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'>' | b'/'
                )
            {
                if let Some(rel) = find_ascii_ci(&bytes[i + open.len()..], close) {
                    i = i + open.len() + rel + close.len();
                    if let Some(gt) = bytes[i..].iter().position(|&b| b == b'>') {
                        i += gt + 1;
                    }
                    skipped_raw = true;
                    break;
                }
                i = limit;
                skipped_raw = true;
                break;
            }
        }
        if skipped_raw {
            continue;
        }
        if bytes[i..].len() > 5
            && bytes[i..i + 5].eq_ignore_ascii_case(b"<meta")
            && matches!(bytes[i + 5], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/')
        {
            // Advance past `<meta` honoring quoted attribute values to find closing `>`.
            let mut j = i + 5;
            let mut quote = 0u8;
            while j < len {
                let b = bytes[j];
                if quote != 0 {
                    if b == quote {
                        quote = 0;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = b;
                } else if b == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= len {
                break;
            }
            let attrs = parse_meta_tag_attrs(&bytes[i + 5..j]);
            // WHATWG HTML §13.2.3.2: `charset` attribute takes precedence over `http-equiv`/`content`.
            if let Some((_, charset)) = attrs.iter().find(|(k, _)| k == "charset") {
                if let Some(enc) = Encoding::for_label(charset.trim().as_bytes()) {
                    return Some(enc);
                }
            }
            let has_ct_http_equiv = attrs
                .iter()
                .find(|(k, _)| k == "http-equiv")
                .is_some_and(|(_, v)| v.trim().eq_ignore_ascii_case("content-type"));
            if has_ct_http_equiv {
                if let Some((_, content)) = attrs.iter().find(|(k, _)| k == "content") {
                    if let Some(label) = crate::tree_builder::data::extract_meta_charset(
                        StrTendril::from_slice(content),
                    ) {
                        if let Some(enc) = Encoding::for_label(label.trim().as_bytes()) {
                            return Some(enc);
                        }
                    }
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    // WHATWG HTML §13.2.3.2: `<meta>` prescan takes precedence over XML declaration fallback.
    prescan_xml_declaration(bytes)
}

/// Detects the document character encoding from BOM or `<meta charset>` / `<meta http-equiv>`
/// prescan per WHATWG HTML §13.2.3.2, mapping meta UTF-16 labels to `"UTF-8"`.
pub fn detect_html_charset(bytes: &[u8]) -> &'static str {
    if let Some((enc, _)) = Encoding::for_bom(bytes) {
        return enc.name();
    }
    if bytes.starts_with(b"<\0?\0x\0") {
        return encoding_rs::UTF_16LE.name();
    }
    if bytes.starts_with(b"\0<\0?\0x") {
        return encoding_rs::UTF_16BE.name();
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

/// Checks for a late `<meta charset>` encoding change inside `<head>` per WHATWG HTML §13.2.3.3
/// (handles `<meta>` past the first 1,024 bytes or containing HTML character references).
fn scan_late_head_meta_encoding(decoded: &str) -> Option<&'static Encoding> {
    let sink = DomTreeSink::new();
    let opts = TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
    };
    let parser = RixHtmlParser::new_document(sink, opts);
    parser
        .input_buffer
        .push_back(StrTendril::from_slice(decoded));
    loop {
        match parser.tokenizer.feed(&parser.input_buffer) {
            TokenizerResult::EncodingIndicator(raw_label) => {
                if let Some(enc) = Encoding::for_label(raw_label.trim().as_bytes()) {
                    return Some(
                        if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
                            UTF_8
                        } else {
                            enc
                        },
                    );
                }
            }
            TokenizerResult::Done => return None,
            TokenizerResult::Script(_) => continue,
        }
    }
}

/// Decodes raw HTML response bytes per WHATWG HTML §13.2.3.3 (BOM -> HTTP `charset` ->
/// XML decl / `<meta>` prescan -> UTF-8), returning the decoded UTF-8 string and canonical encoding name.
pub fn decode_html_bytes(bytes: &[u8], http_charset: Option<&str>) -> (String, &'static str) {
    decode_html_bytes_with_fallback(bytes, http_charset, None)
}

/// Decodes raw HTML response bytes with an optional inherited parent-document fallback charset
/// (WHATWG HTML §13.2.3.3 step 6 for same-origin `<iframe>` documents).
pub fn decode_html_bytes_with_fallback(
    bytes: &[u8],
    http_charset: Option<&str>,
    fallback_charset: Option<&str>,
) -> (String, &'static str) {
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        let (decoded, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        return (decoded.into_owned(), enc.name());
    }
    if let Some(http_enc) = http_charset.and_then(|c| {
        let trimmed = c.trim_matches(|ch: char| matches!(ch, '\t' | '\n' | '\x0c' | '\r' | ' '));
        Encoding::for_label(trimmed.as_bytes())
    }) {
        let (decoded, _) = http_enc.decode_without_bom_handling(bytes);
        return (decoded.into_owned(), http_enc.name());
    }
    // WHATWG HTML §13.2.3.2: UTF-16 XML declaration heuristic (when no BOM or HTTP charset).
    if bytes.starts_with(b"<\0?\0x\0") {
        let (decoded, _) = encoding_rs::UTF_16LE.decode_without_bom_handling(bytes);
        return (decoded.into_owned(), encoding_rs::UTF_16LE.name());
    }
    if bytes.starts_with(b"\0<\0?\0x") {
        let (decoded, _) = encoding_rs::UTF_16BE.decode_without_bom_handling(bytes);
        return (decoded.into_owned(), encoding_rs::UTF_16BE.name());
    }
    if let Some(prescan_enc) = prescan_meta_charset(bytes) {
        let mapped = if prescan_enc == encoding_rs::UTF_16LE || prescan_enc == encoding_rs::UTF_16BE
        {
            UTF_8
        } else {
            prescan_enc
        };
        let (decoded, _) = mapped.decode_without_bom_handling(bytes);
        return (decoded.into_owned(), mapped.name());
    }
    // WHATWG HTML §13.2.3.2 step 5: only inherit ASCII-compatible encodings from parent frames
    // (never UTF-16LE / UTF-16BE).
    let tentative_enc = fallback_charset
        .and_then(|c| {
            let trimmed =
                c.trim_matches(|ch: char| matches!(ch, '\t' | '\n' | '\x0c' | '\r' | ' '));
            Encoding::for_label(trimmed.as_bytes())
        })
        .filter(|&e| e != encoding_rs::UTF_16LE && e != encoding_rs::UTF_16BE)
        .unwrap_or(UTF_8);
    let (decoded, _) = tentative_enc.decode_without_bom_handling(bytes);
    let decoded_str = decoded.into_owned();
    // WHATWG HTML §13.2.3.3: tentative encoding can switch on late `<meta>` inside `<head>`.
    if find_ascii_ci(bytes, b"<meta").is_some() {
        if let Some(late_enc) = scan_late_head_meta_encoding(&decoded_str) {
            if late_enc != tentative_enc {
                let (redecoded, _) = late_enc.decode_without_bom_handling(bytes);
                return (redecoded.into_owned(), late_enc.name());
            }
        }
    }
    (decoded_str, tentative_enc.name())
}

pub fn parse_html(html: &str) -> Document {
    parse_html_with_scripting(html, true)
}

pub fn parse_html_with_scripting(html: &str, scripting_enabled: bool) -> Document {
    parse_html_with_options(html, scripting_enabled, true)
}

pub fn parse_html_with_options(
    html: &str,
    scripting_enabled: bool,
    allow_declarative_shadow: bool,
) -> Document {
    let sink = DomTreeSink::with_source_pi_scan(html, scripting_enabled);
    let opts = TreeBuilderOpts {
        scripting_enabled,
        ..Default::default()
    };
    let mut doc = RixHtmlParser::new_document(sink, opts).parse_one(html);
    doc.set_character_set(detect_html_charset(html.as_bytes()));
    if allow_declarative_shadow {
        attach_declarative_shadow_roots(&mut doc);
    }
    sync_selectedcontent_elements(&mut doc);
    doc
}

pub fn parse_html_fragment(html: &str, context: Option<&QualName>) -> (Document, Vec<NodeId>) {
    parse_html_fragment_with_options(html, context, true, false)
}

pub fn parse_html_fragment_with_scripting(
    html: &str,
    context: Option<&QualName>,
    scripting_enabled: bool,
) -> (Document, Vec<NodeId>) {
    parse_html_fragment_with_options(html, context, scripting_enabled, false)
}

pub fn parse_html_fragment_with_options(
    html: &str,
    context: Option<&QualName>,
    scripting_enabled: bool,
    allow_declarative_shadow: bool,
) -> (Document, Vec<NodeId>) {
    parse_html_fragment_with_form_owner(
        html,
        context,
        scripting_enabled,
        allow_declarative_shadow,
        false,
    )
}

pub fn parse_html_fragment_with_form_owner(
    html: &str,
    context: Option<&QualName>,
    scripting_enabled: bool,
    allow_declarative_shadow: bool,
    has_form_ancestor: bool,
) -> (Document, Vec<NodeId>) {
    let sink = DomTreeSink::with_source_pi_scan(html, scripting_enabled);
    let opts = TreeBuilderOpts {
        scripting_enabled,
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
    let form_elem = if has_form_ancestor {
        Some(create_element(
            &sink,
            QualName::new(
                None,
                markup5ever::ns!(html),
                markup5ever::local_name!("form"),
            ),
            Vec::new(),
        ))
    } else {
        None
    };
    let mut doc =
        RixHtmlParser::new_fragment(sink, context_name, form_elem, true, opts).parse_one(html);
    if allow_declarative_shadow {
        attach_declarative_shadow_roots(&mut doc);
    }
    sync_selectedcontent_elements(&mut doc);
    // Parsed fragment nodes live as children of the synthetic root `<html>` wrapper element
    // (plus any trailing sibling nodes like comments after `</html>`).
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
                        let serializable = elem.has_attribute("shadowrootserializable");
                        let slot_assignment = if elem
                            .get_attribute("shadowrootslotassignment")
                            .is_some_and(|s| s.eq_ignore_ascii_case("manual"))
                        {
                            "manual".to_string()
                        } else {
                            "named".to_string()
                        };
                        let null_registry = elem.has_attribute("shadowrootcustomelementregistry");
                        let frag = elem.template_contents.unwrap_or(child);
                        Some((
                            mode_lower,
                            delegates_focus,
                            clonable,
                            serializable,
                            slot_assignment,
                            null_registry,
                            frag,
                        ))
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

        if let Some((
            mode_lower,
            delegates_focus,
            clonable,
            serializable,
            slot_assignment,
            null_registry,
            frag,
        )) = template_info
        {
            let parent = doc.parent(child);
            if let Some(parent) = parent {
                if can_attach_declarative_shadow(doc, parent) {
                    let shadow_root = doc.new_node(NodeData::ShadowRoot {
                        host: parent,
                        mode: mode_lower,
                        delegates_focus,
                        clonable,
                        serializable,
                        slot_assignment,
                        declarative: true,
                        null_registry,
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
        let (shadow_root_to_recurse, template_contents_to_recurse) =
            if let Some(NodeData::Element(elem)) = doc.get(child) {
                (elem.shadow_root, elem.template_contents)
            } else {
                (None, None)
            };
        if let Some(sr) = shadow_root_to_recurse {
            attach_declarative_shadow_roots_in_subtree(doc, sr);
        }
        if let Some(tc) = template_contents_to_recurse {
            attach_declarative_shadow_roots_in_subtree(doc, tc);
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

/// Synchronizes customizable `<select>` `<selectedcontent>` elements per WHATWG HTML §4.10.8
/// by cloning the selected `<option>`'s child nodes into the owning `<select>`'s first
/// `<selectedcontent>` descendant.
pub fn sync_selectedcontent_elements(doc: &mut Document) {
    let html_ns = "http://www.w3.org/1999/xhtml";
    let selects: Vec<NodeId> = doc
        .descendants(doc.root())
        .filter(|&nid| {
            matches!(
                doc.get(nid),
                Some(NodeData::Element(e))
                    if &*e.name.ns == html_ns && &*e.name.local == "select"
            )
        })
        .collect();

    for sel in selects {
        let mut selectedcontent: Option<NodeId> = None;
        let mut first_option: Option<NodeId> = None;
        let mut last_selected_option: Option<NodeId> = None;

        // Walk descendants of `sel` without descending into nested `<select>` elements.
        let mut stack: Vec<NodeId> = doc.children(sel).collect();
        stack.reverse();
        while let Some(nid) = stack.pop() {
            if let Some(NodeData::Element(e)) = doc.get(nid) {
                if &*e.name.ns == html_ns {
                    match &*e.name.local {
                        "select" => continue,
                        "selectedcontent" => {
                            if selectedcontent.is_none() {
                                selectedcontent = Some(nid);
                            }
                            continue;
                        }
                        "option" => {
                            if first_option.is_none() {
                                first_option = Some(nid);
                            }
                            if e.has_attribute("selected") {
                                last_selected_option = Some(nid);
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            let mut kids: Vec<NodeId> = doc.children(nid).collect();
            kids.reverse();
            stack.extend(kids);
        }

        let Some(sc_nid) = selectedcontent else {
            continue;
        };
        let Some(opt_nid) = last_selected_option.or(first_option) else {
            continue;
        };
        let existing: Vec<NodeId> = doc.children(sc_nid).collect();
        for old in existing {
            doc.remove_child(old);
        }
        let opt_children: Vec<NodeId> = doc.children(opt_nid).collect();
        for ch in opt_children {
            let cloned = clone_subtree_in_place(doc, ch);
            doc.append_child(sc_nid, cloned);
        }
    }
}

fn clone_subtree_in_place(doc: &mut Document, node: NodeId) -> NodeId {
    let data = doc.get(node).cloned().expect("valid node to clone");
    let new_node = doc.new_node(data);
    let children: Vec<NodeId> = doc.children(node).collect();
    for child in children {
        let cloned_child = clone_subtree_in_place(doc, child);
        doc.append_child(new_node, cloned_child);
    }
    new_node
}
