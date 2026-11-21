use memchr::memchr;
use rix_dom::parse_srcset;
use std::collections::HashSet;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreloadKind {
    Style,
    Script,
    Module,
    Font,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PreloadPriority {
    Highest,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadHint {
    pub url: String,
    pub kind: PreloadKind,
    pub priority: PreloadPriority,
    pub crossorigin: Option<String>,
    pub media: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Text,
    Comment,
    Tag,
}

pub struct PreloadScanner {
    base_url: Option<Url>,
    base_locked: bool,
    scripting_enabled: bool,
    template_depth: usize,
    noscript_depth: usize,
    rawtext_closing_tag: Option<&'static [u8]>,
    state: ScanState,
    buf: Vec<u8>,
    tag_quote: Option<u8>,
    seen: HashSet<(PreloadKind, String)>,
    pending: Vec<PreloadHint>,
}

impl PreloadScanner {
    pub fn new(document_url: Option<&str>) -> Self {
        let base_url = document_url.and_then(|u| Url::parse(u).ok());
        Self {
            base_url,
            base_locked: false,
            scripting_enabled: true,
            template_depth: 0,
            noscript_depth: 0,
            rawtext_closing_tag: None,
            state: ScanState::Text,
            buf: Vec::new(),
            tag_quote: None,
            seen: HashSet::new(),
            pending: Vec::new(),
        }
    }

    pub fn set_scripting_enabled(&mut self, enabled: bool) {
        self.scripting_enabled = enabled;
    }

    pub fn base_url(&self) -> Option<&Url> {
        self.base_url.as_ref()
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let mut i = 0usize;
        while i < bytes.len() {
            match self.state {
                ScanState::Text => {
                    if let Some(close_seq) = self.rawtext_closing_tag {
                        let b = bytes[i];
                        if b == b'<' || !self.buf.is_empty() {
                            if b == b'<' {
                                self.buf.clear();
                            }
                            self.buf.push(b.to_ascii_lowercase());
                            if self.buf.len() == close_seq.len() {
                                if self.buf == close_seq {
                                    self.rawtext_closing_tag = None;
                                    self.buf.clear();
                                    // Consume up to `>` for closing rawtext tag.
                                    self.state = ScanState::Tag;
                                    self.tag_quote = None;
                                    self.buf.extend_from_slice(&close_seq[1..]);
                                } else {
                                    self.buf.clear();
                                }
                            } else if !close_seq.starts_with(&self.buf) {
                                self.buf.clear();
                            }
                        } else if let Some(rel) = memchr(b'<', &bytes[i..]) {
                            i += rel;
                            continue;
                        } else {
                            break;
                        }
                        i += 1;
                    } else if let Some(rel) = memchr(b'<', &bytes[i..]) {
                        i += rel + 1;
                        self.buf.clear();
                        self.tag_quote = None;
                        self.state = ScanState::Tag;
                    } else {
                        break;
                    }
                }
                ScanState::Comment => {
                    let b = bytes[i];
                    self.buf.push(b);
                    if self.buf.len() > 3 {
                        let start = self.buf.len() - 3;
                        self.buf.copy_within(start.., 0);
                        self.buf.truncate(3);
                    }
                    if self.buf.as_slice() == b"-->" {
                        self.buf.clear();
                        self.state = ScanState::Text;
                    }
                    i += 1;
                }
                ScanState::Tag => {
                    let b = bytes[i];
                    if self.tag_quote.is_none() && b == b'-' && self.buf == b"!-" {
                        self.buf.clear();
                        self.state = ScanState::Comment;
                        i += 1;
                        continue;
                    }
                    if let Some(q) = self.tag_quote {
                        if b == q {
                            self.tag_quote = None;
                        }
                        if self.buf.len() < 16 * 1024 {
                            self.buf.push(b);
                        }
                        i += 1;
                    } else if b == b'"' || b == b'\'' {
                        self.tag_quote = Some(b);
                        if self.buf.len() < 16 * 1024 {
                            self.buf.push(b);
                        }
                        i += 1;
                    } else if b == b'>' {
                        let tag = String::from_utf8_lossy(&self.buf).into_owned();
                        self.buf.clear();
                        self.state = ScanState::Text;
                        self.process_tag(&tag);
                        i += 1;
                    } else if self.tag_quote.is_none() && b == b'<' {
                        // Recover from unclosed `<` before a new `<`.
                        self.buf.clear();
                        i += 1;
                    } else {
                        if self.buf.len() < 16 * 1024 {
                            self.buf.push(b);
                        }
                        i += 1;
                    }
                }
            }
        }
    }

    pub fn take_hints(&mut self) -> Vec<PreloadHint> {
        std::mem::take(&mut self.pending)
    }

    fn process_tag(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            return;
        }
        let self_closing = trimmed.ends_with('/');
        let body = trimmed.trim_end_matches('/').trim();
        if let Some(closing) = body.strip_prefix('/') {
            let name = closing
                .split(|c: char| c.is_ascii_whitespace())
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            match name.as_str() {
                "template" => self.template_depth = self.template_depth.saturating_sub(1),
                "noscript" => self.noscript_depth = self.noscript_depth.saturating_sub(1),
                _ => {}
            }
            return;
        }

        let (tag_name, attr_slice) = match body.find(|c: char| c.is_ascii_whitespace()) {
            Some(pos) => (body[..pos].to_ascii_lowercase(), &body[pos..]),
            None => (body.to_ascii_lowercase(), ""),
        };

        let attrs = parse_attrs(attr_slice);
        match tag_name.as_str() {
            "template" => {
                if !self_closing {
                    self.template_depth += 1;
                }
                return;
            }
            "noscript" => {
                if !self_closing {
                    self.noscript_depth += 1;
                }
                return;
            }
            "style" => {
                if !self_closing {
                    self.rawtext_closing_tag = Some(b"</style");
                }
                return;
            }
            "script" if !self_closing => {
                self.rawtext_closing_tag = Some(b"</script");
            }
            _ => {}
        }

        if self.template_depth > 0 || (self.scripting_enabled && self.noscript_depth > 0) {
            return;
        }

        match tag_name.as_str() {
            "base" if !self.base_locked => {
                if let Some(href) = attr_value(&attrs, "href") {
                    let resolved = self
                        .resolve_url(href)
                        .and_then(|s| Url::parse(&s).ok())
                        .or_else(|| Url::parse(href).ok());
                    if let Some(url) = resolved {
                        self.base_url = Some(url);
                        self.base_locked = true;
                    }
                }
            }
            "link" => {
                let Some(rel) = attr_value(&attrs, "rel") else {
                    return;
                };
                let Some(href) = attr_value(&attrs, "href") else {
                    return;
                };
                let Some(url) = self.resolve_url(href) else {
                    return;
                };
                let media = attr_value(&attrs, "media").map(str::to_string);
                let crossorigin = attr_value(&attrs, "crossorigin").map(str::to_string);

                for token in rel.split_ascii_whitespace() {
                    if token.eq_ignore_ascii_case("stylesheet") {
                        self.emit(PreloadHint {
                            url: url.clone(),
                            kind: PreloadKind::Style,
                            priority: PreloadPriority::Highest,
                            crossorigin: crossorigin.clone(),
                            media: media.clone(),
                        });
                    } else if token.eq_ignore_ascii_case("modulepreload") {
                        self.emit(PreloadHint {
                            url: url.clone(),
                            kind: PreloadKind::Module,
                            priority: PreloadPriority::High,
                            crossorigin: crossorigin.clone(),
                            media: media.clone(),
                        });
                    } else if token.eq_ignore_ascii_case("preload") {
                        let Some(as_val) = attr_value(&attrs, "as") else {
                            continue;
                        };
                        let mapped = match as_val.to_ascii_lowercase().as_str() {
                            "style" => Some((PreloadKind::Style, PreloadPriority::Highest)),
                            "script" => Some((PreloadKind::Script, PreloadPriority::High)),
                            "font" => Some((PreloadKind::Font, PreloadPriority::High)),
                            "image" => Some((PreloadKind::Image, PreloadPriority::Low)),
                            _ => None,
                        };
                        if let Some((kind, priority)) = mapped {
                            self.emit(PreloadHint {
                                url: url.clone(),
                                kind,
                                priority,
                                crossorigin: crossorigin.clone(),
                                media: media.clone(),
                            });
                        }
                    }
                }
            }
            "script" => {
                let Some(src) = attr_value(&attrs, "src") else {
                    return;
                };
                let Some(url) = self.resolve_url(src) else {
                    return;
                };
                let is_module = attr_value(&attrs, "type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("module"));
                let is_deferred = attr_value(&attrs, "async").is_some()
                    || attr_value(&attrs, "defer").is_some()
                    || is_module;
                self.emit(PreloadHint {
                    url,
                    kind: if is_module {
                        PreloadKind::Module
                    } else {
                        PreloadKind::Script
                    },
                    priority: if is_deferred {
                        PreloadPriority::Medium
                    } else {
                        PreloadPriority::High
                    },
                    crossorigin: attr_value(&attrs, "crossorigin").map(str::to_string),
                    media: None,
                });
            }
            "img" | "source" => {
                if let Some(src) = attr_value(&attrs, "src") {
                    if let Some(url) = self.resolve_url(src) {
                        self.emit(PreloadHint {
                            url,
                            kind: PreloadKind::Image,
                            priority: PreloadPriority::Low,
                            crossorigin: attr_value(&attrs, "crossorigin").map(str::to_string),
                            media: attr_value(&attrs, "media").map(str::to_string),
                        });
                    }
                }
                if let Some(srcset) = attr_value(&attrs, "srcset") {
                    for cand in parse_srcset(srcset) {
                        if let Some(url) = self.resolve_url(cand.url) {
                            self.emit(PreloadHint {
                                url,
                                kind: PreloadKind::Image,
                                priority: PreloadPriority::Low,
                                crossorigin: attr_value(&attrs, "crossorigin").map(str::to_string),
                                media: attr_value(&attrs, "media").map(str::to_string),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn resolve_url(&self, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(base) = &self.base_url {
            base.join(trimmed).ok().map(String::from)
        } else {
            Some(trimmed.to_string())
        }
    }

    fn emit(&mut self, hint: PreloadHint) {
        if self.seen.insert((hint.kind, hint.url.clone())) {
            self.pending.push(hint);
        }
    }
}

fn attr_value<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn parse_attrs(mut input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        input = input.trim_start();
        if input.is_empty() {
            break;
        }
        let name_end = input
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(input.len());
        if name_end == 0 {
            input = &input[1..];
            continue;
        }
        let name = input[..name_end].to_ascii_lowercase();
        input = input[name_end..].trim_start();
        if !input.starts_with('=') {
            out.push((name, String::new()));
            continue;
        }
        input = input[1..].trim_start();
        let val = if let Some(rest) = input.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                let v = &rest[..end];
                input = &rest[end + 1..];
                v
            } else {
                input = "";
                rest
            }
        } else if let Some(rest) = input.strip_prefix('\'') {
            if let Some(end) = rest.find('\'') {
                let v = &rest[..end];
                input = &rest[end + 1..];
                v
            } else {
                input = "";
                rest
            }
        } else {
            let end = input
                .find(|c: char| c.is_ascii_whitespace())
                .unwrap_or(input.len());
            let v = &input[..end];
            input = &input[end..];
            v
        };
        out.push((name, val.to_string()));
    }
    out
}
