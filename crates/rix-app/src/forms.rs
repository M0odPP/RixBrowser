//! In-page HTML form control editing (`<input>`, `<textarea>`, `<select>`,
//! `<button>`), GET/POST form submission encoding, text selection extraction,
//! and OS clipboard bridge (`arboard` with headless in-memory fallback).

use rix_dom::{DirtyFlags, Document, ElementState, NodeData, NodeId};
use url::Url;

/// OS clipboard wrapper using `arboard` with an in-memory fallback for headless
/// Linux/CI environments lacking an X11 or Wayland display server.
pub struct ClipboardBridge {
    system: Option<arboard::Clipboard>,
    fallback: String,
}

impl Default for ClipboardBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardBridge {
    #[must_use]
    pub fn new() -> Self {
        Self {
            system: arboard::Clipboard::new().ok(),
            fallback: String::new(),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        let value = text.into();
        self.fallback = value.clone();
        if let Some(cb) = &mut self.system {
            let _ = cb.set_text(value);
        }
    }

    #[must_use]
    pub fn get_text(&mut self) -> String {
        if let Some(cb) = &mut self.system {
            if let Ok(text) = cb.get_text() {
                if !text.is_empty() {
                    return text;
                }
            }
        }
        self.fallback.clone()
    }
}

/// HTTP submission method for an HTML `<form>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

/// Encoded HTML `<form>` submission ready to dispatch via `rix-net`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormSubmission {
    pub action_url: String,
    pub method: FormMethod,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub fields: Vec<(String, String)>,
}

/// Result of clicking a DOM element on a web page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageClickOutcome {
    Navigate(String),
    SubmitForm(FormSubmission),
    ControlUpdated,
    SelectedText(String),
    None,
}

/// Per-tab interactive focus and text selection state inside a web document.
#[derive(Debug, Default, Clone)]
pub struct InPageInteractionState {
    pub focused_control: Option<NodeId>,
    pub selected_text: String,
}

impl InPageInteractionState {
    pub fn clear(&mut self) {
        self.focused_control = None;
        self.selected_text.clear();
    }

    /// Handles a click on `hit_node` inside `doc`, supporting `<a href>`,
    /// `<input>`, `<textarea>`, `<select>`, `<button>`, and text selection.
    pub fn handle_click(
        &mut self,
        doc: &mut Document,
        hit_node: NodeId,
        base_url: &str,
    ) -> PageClickOutcome {
        // 1. Check if the click lands on or inside an interactive form control.
        if let Some(control) = find_ancestor_form_control(doc, hit_node) {
            let tag = element_tag_lower(doc, control).unwrap_or_default();
            let input_type = element_attr(doc, control, "type")
                .unwrap_or_else(|| "text".to_string())
                .to_ascii_lowercase();

            match tag.as_str() {
                "button" => {
                    let btn_type = element_attr(doc, control, "type")
                        .unwrap_or_else(|| "submit".to_string())
                        .to_ascii_lowercase();
                    if btn_type == "submit" {
                        if let Some(sub) = collect_form_submission(doc, control, base_url) {
                            return PageClickOutcome::SubmitForm(sub);
                        }
                    } else if btn_type == "reset" {
                        reset_enclosing_form(doc, control);
                        return PageClickOutcome::ControlUpdated;
                    }
                    return PageClickOutcome::None;
                }
                "input" if input_type == "submit" => {
                    if let Some(sub) = collect_form_submission(doc, control, base_url) {
                        return PageClickOutcome::SubmitForm(sub);
                    }
                    return PageClickOutcome::None;
                }
                "input" if input_type == "checkbox" => {
                    toggle_checkbox(doc, control);
                    self.focus_control(doc, control);
                    return PageClickOutcome::ControlUpdated;
                }
                "input" if input_type == "radio" => {
                    select_radio(doc, control);
                    self.focus_control(doc, control);
                    return PageClickOutcome::ControlUpdated;
                }
                "select" => {
                    cycle_select_option(doc, control);
                    self.focus_control(doc, control);
                    return PageClickOutcome::ControlUpdated;
                }
                "input" | "textarea" => {
                    self.focus_control(doc, control);
                    return PageClickOutcome::ControlUpdated;
                }
                _ => {}
            }
        }

        // 2. Check for ancestor `<a href="...">` hyperlink.
        if let Some(href) = rix_chrome::find_link_href(doc, hit_node) {
            self.blur(doc);
            if let Some(resolved) = resolve_relative_url(base_url, &href) {
                return PageClickOutcome::Navigate(resolved);
            }
        }

        // 3. Otherwise blur any active form control and select text from clicked element.
        self.blur(doc);
        let text = collect_element_text(doc, hit_node);
        if !text.is_empty() {
            self.selected_text = text.clone();
            return PageClickOutcome::SelectedText(text);
        }
        PageClickOutcome::None
    }

    pub fn focus_control(&mut self, doc: &mut Document, node: NodeId) {
        self.focused_control = Some(node);
        doc.set_focused(Some(node), true);
    }

    pub fn blur(&mut self, doc: &mut Document) {
        if self.focused_control.take().is_some() {
            doc.set_focused(None, false);
        }
    }

    /// Inserts typed text into the currently focused `<input>` or `<textarea>`.
    pub fn insert_text(&mut self, doc: &mut Document, text: &str) -> bool {
        let Some(node) = self.focused_control else {
            return false;
        };
        let Some(tag) = element_tag_lower(doc, node) else {
            return false;
        };
        match tag.as_str() {
            "input" => {
                let clean: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
                if clean.is_empty() {
                    return false;
                }
                let mut cur = element_attr(doc, node, "value").unwrap_or_default();
                cur.push_str(&clean);
                if let Some(NodeData::Element(e)) = doc.get_mut(node) {
                    e.set_attribute("value", &cur);
                }
                doc.mark_node_dirty(
                    node,
                    DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY,
                );
                true
            }
            "textarea" => {
                let mut cur = collect_element_text(doc, node);
                cur.push_str(text);
                set_textarea_value(doc, node, &cur);
                true
            }
            _ => false,
        }
    }

    /// Deletes the last character from the currently focused `<input>` or `<textarea>`.
    pub fn backspace(&mut self, doc: &mut Document) -> bool {
        let Some(node) = self.focused_control else {
            return false;
        };
        let Some(tag) = element_tag_lower(doc, node) else {
            return false;
        };
        match tag.as_str() {
            "input" => {
                let mut cur = element_attr(doc, node, "value").unwrap_or_default();
                if cur.pop().is_none() {
                    return false;
                }
                if let Some(NodeData::Element(e)) = doc.get_mut(node) {
                    e.set_attribute("value", &cur);
                }
                doc.mark_node_dirty(
                    node,
                    DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY,
                );
                true
            }
            "textarea" => {
                let mut cur = collect_element_text(doc, node);
                if cur.pop().is_none() {
                    return false;
                }
                set_textarea_value(doc, node, &cur);
                true
            }
            _ => false,
        }
    }

    /// Handles `Enter` inside a focused form control (submits `<input>`, inserts newline in `<textarea>`).
    pub fn press_enter(&mut self, doc: &mut Document, base_url: &str) -> PageClickOutcome {
        let Some(node) = self.focused_control else {
            return PageClickOutcome::None;
        };
        let Some(tag) = element_tag_lower(doc, node) else {
            return PageClickOutcome::None;
        };
        if tag == "textarea" {
            self.insert_text(doc, "\n");
            return PageClickOutcome::ControlUpdated;
        }
        if let Some(sub) = collect_form_submission(doc, node, base_url) {
            return PageClickOutcome::SubmitForm(sub);
        }
        PageClickOutcome::None
    }
}

/// Builds a GET or POST [`FormSubmission`] for the `<form>` enclosing `trigger`.
#[must_use]
pub fn collect_form_submission(
    doc: &Document,
    trigger: NodeId,
    base_url: &str,
) -> Option<FormSubmission> {
    let form = find_ancestor_form(doc, trigger)?;
    let raw_action = element_attr(doc, form, "action").unwrap_or_default();
    let action_base = if raw_action.trim().is_empty() {
        base_url.to_string()
    } else {
        resolve_relative_url(base_url, raw_action.trim())?
    };
    let method_str = element_attr(doc, form, "method")
        .unwrap_or_else(|| "get".to_string())
        .to_ascii_lowercase();
    let method = if method_str == "post" {
        FormMethod::Post
    } else {
        FormMethod::Get
    };

    let mut fields: Vec<(String, String)> = Vec::new();
    for desc in doc.descendants(form) {
        let Some(NodeData::Element(elem)) = doc.get(desc) else {
            continue;
        };
        if elem.has_attribute("disabled") {
            continue;
        }
        let Some(name) = elem.get_attribute("name").filter(|n| !n.is_empty()) else {
            continue;
        };
        let tag = elem.name.local.as_str();
        match tag {
            "input" => {
                let ty = elem
                    .get_attribute("type")
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                match ty.as_str() {
                    "submit" | "button" | "reset" => {
                        if desc == trigger {
                            fields.push((
                                name.to_string(),
                                elem.get_attribute("value").unwrap_or_default().to_string(),
                            ));
                        }
                    }
                    "checkbox" | "radio" => {
                        if elem.pseudo_state.contains(ElementState::CHECKED)
                            || elem.has_attribute("checked")
                        {
                            let val = elem.get_attribute("value").unwrap_or("on");
                            fields.push((name.to_string(), val.to_string()));
                        }
                    }
                    _ => {
                        let val = elem.get_attribute("value").unwrap_or_default();
                        fields.push((name.to_string(), val.to_string()));
                    }
                }
            }
            "textarea" => {
                fields.push((name.to_string(), collect_element_text(doc, desc)));
            }
            "select" => {
                let val = selected_option_value(doc, desc);
                fields.push((name.to_string(), val));
            }
            "button" if desc == trigger => {
                let val = elem
                    .get_attribute("value")
                    .map(str::to_string)
                    .unwrap_or_else(|| collect_element_text(doc, desc));
                fields.push((name.to_string(), val));
            }
            _ => {}
        }
    }

    let encoded = encode_form_urlencoded(&fields);
    match method {
        FormMethod::Get => {
            let mut url = Url::parse(&action_base).ok()?;
            url.set_query(if encoded.is_empty() {
                None
            } else {
                Some(&encoded)
            });
            Some(FormSubmission {
                action_url: url.to_string(),
                method: FormMethod::Get,
                content_type: None,
                body: Vec::new(),
                fields,
            })
        }
        FormMethod::Post => Some(FormSubmission {
            action_url: action_base,
            method: FormMethod::Post,
            content_type: Some("application/x-www-form-urlencoded".to_string()),
            body: encoded.into_bytes(),
            fields,
        }),
    }
}

/// Encodes `(name, value)` pairs using WHATWG `application/x-www-form-urlencoded`.
#[must_use]
pub fn encode_form_urlencoded(fields: &[(String, String)]) -> String {
    let mut out = String::new();
    for (idx, (k, v)) in fields.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        push_urlencoded(&mut out, k);
        out.push('=');
        push_urlencoded(&mut out, v);
    }
    out
}

fn push_urlencoded(out: &mut String, input: &str) {
    use std::fmt::Write as _;
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
}

/// Resolves a possibly-relative URL against `base_url`.
#[must_use]
pub fn resolve_relative_url(base_url: &str, target: &str) -> Option<String> {
    if target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("rix://")
        || target.starts_with("file://")
    {
        return Some(target.to_string());
    }
    Url::parse(base_url)
        .ok()
        .and_then(|b| b.join(target).ok())
        .map(|u| u.to_string())
}

fn find_ancestor_form_control(doc: &Document, start: NodeId) -> Option<NodeId> {
    let mut cur = Some(start);
    while let Some(node) = cur {
        if let Some(NodeData::Element(e)) = doc.get(node) {
            if matches!(
                e.name.local.as_str(),
                "input" | "textarea" | "select" | "button"
            ) {
                return Some(node);
            }
        }
        cur = doc.parent(node);
    }
    None
}

fn find_ancestor_form(doc: &Document, start: NodeId) -> Option<NodeId> {
    let mut cur = Some(start);
    while let Some(node) = cur {
        if let Some(NodeData::Element(e)) = doc.get(node) {
            if e.name.local.eq_str_ignore_ascii_case("form") {
                return Some(node);
            }
        }
        cur = doc.parent(node);
    }
    None
}

fn element_tag_lower(doc: &Document, node: NodeId) -> Option<String> {
    match doc.get(node) {
        Some(NodeData::Element(e)) => Some(e.name.local.as_str().to_ascii_lowercase()),
        _ => None,
    }
}

fn element_attr(doc: &Document, node: NodeId, name: &str) -> Option<String> {
    match doc.get(node) {
        Some(NodeData::Element(e)) => e.get_attribute(name).map(str::to_string),
        _ => None,
    }
}

fn toggle_checkbox(doc: &mut Document, node: NodeId) {
    let was_checked = match doc.get(node) {
        Some(NodeData::Element(e)) => {
            e.pseudo_state.contains(ElementState::CHECKED) || e.has_attribute("checked")
        }
        _ => false,
    };
    if let Some(NodeData::Element(e)) = doc.get_mut(node) {
        if was_checked {
            e.remove_attribute("checked");
        } else {
            e.set_attribute("checked", "");
        }
    }
    doc.mark_node_dirty(node, DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::PAINT_DIRTY);
}

fn select_radio(doc: &mut Document, node: NodeId) {
    let group = element_attr(doc, node, "name");
    if let (Some(form), Some(group_name)) = (find_ancestor_form(doc, node), group) {
        let siblings: Vec<NodeId> = doc
            .descendants(form)
            .filter(|&d| {
                matches!(
                    doc.get(d),
                    Some(NodeData::Element(e))
                        if e.name.local.eq_str_ignore_ascii_case("input")
                            && e.get_attribute("type").is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                            && e.get_attribute("name") == Some(group_name.as_str())
                )
            })
            .collect();
        for sib in siblings {
            if let Some(NodeData::Element(e)) = doc.get_mut(sib) {
                e.remove_attribute("checked");
            }
        }
    }
    if let Some(NodeData::Element(e)) = doc.get_mut(node) {
        e.set_attribute("checked", "");
    }
    doc.mark_node_dirty(node, DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::PAINT_DIRTY);
}

fn cycle_select_option(doc: &mut Document, select: NodeId) {
    let options: Vec<NodeId> = doc
        .descendants(select)
        .filter(|&d| {
            matches!(
                doc.get(d),
                Some(NodeData::Element(e)) if e.name.local.eq_str_ignore_ascii_case("option")
            )
        })
        .collect();
    if options.is_empty() {
        return;
    }
    let current = options
        .iter()
        .position(|&o| {
            matches!(
                doc.get(o),
                Some(NodeData::Element(e)) if e.has_attribute("selected")
            )
        })
        .unwrap_or(0);
    let next = (current + 1) % options.len();
    for (idx, &opt) in options.iter().enumerate() {
        if let Some(NodeData::Element(e)) = doc.get_mut(opt) {
            if idx == next {
                e.set_attribute("selected", "");
            } else {
                e.remove_attribute("selected");
            }
        }
    }
    doc.mark_node_dirty(
        select,
        DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY,
    );
}

fn selected_option_value(doc: &Document, select: NodeId) -> String {
    let options: Vec<NodeId> = doc
        .descendants(select)
        .filter(|&d| {
            matches!(
                doc.get(d),
                Some(NodeData::Element(e)) if e.name.local.eq_str_ignore_ascii_case("option")
            )
        })
        .collect();
    let chosen = options
        .iter()
        .copied()
        .find(|&o| {
            matches!(
                doc.get(o),
                Some(NodeData::Element(e)) if e.has_attribute("selected")
            )
        })
        .or_else(|| options.first().copied());
    let Some(opt) = chosen else {
        return String::new();
    };
    element_attr(doc, opt, "value").unwrap_or_else(|| collect_element_text(doc, opt))
}

fn reset_enclosing_form(doc: &mut Document, trigger: NodeId) {
    let Some(form) = find_ancestor_form(doc, trigger) else {
        return;
    };
    let nodes: Vec<NodeId> = doc.descendants(form).collect();
    for n in nodes {
        if let Some(NodeData::Element(e)) = doc.get_mut(n) {
            if e.name.local.eq_str_ignore_ascii_case("input") {
                e.set_attribute("value", "");
            }
        }
    }
}

fn set_textarea_value(doc: &mut Document, textarea: NodeId, value: &str) {
    let children: Vec<NodeId> = doc.children(textarea).collect();
    for child in children {
        if let Some(NodeData::Text(t)) = doc.get_mut(child) {
            *t = value.to_string();
            doc.mark_node_dirty(
                textarea,
                DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY,
            );
            return;
        }
    }
    let text_node = doc.create_text(value);
    doc.append_child(textarea, text_node);
    doc.mark_node_dirty(
        textarea,
        DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY,
    );
}

/// Extracts trimmed visible text content inside `node`.
#[must_use]
pub fn collect_element_text(doc: &Document, node: NodeId) -> String {
    let mut out = String::new();
    for desc in doc.descendants(node) {
        if let Some(NodeData::Text(t)) = doc.get(desc) {
            out.push_str(t);
        }
    }
    out.trim().to_string()
}
