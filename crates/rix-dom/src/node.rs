use crate::state::{hash_ident, hash_ident_ascii_lower, DirtyFlags, ElementState};
use indextree::NodeId;
use markup5ever::{LocalName, Namespace, QualName};
use rix_core::{intern_str, InternedStr};
use smallvec::SmallVec;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeData {
    Document,
    DocumentFragment,
    ShadowRoot {
        host: NodeId,
        mode: String,
        delegates_focus: bool,
        clonable: bool,
    },
    DocumentType {
        name: String,
        public_id: String,
        system_id: String,
    },
    Element(ElementData),
    Text(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementData {
    pub name: QualName,
    pub attributes: Vec<(QualName, String)>,
    pub template_contents: Option<NodeId>,
    pub shadow_root: Option<NodeId>,
    pub content_document: Option<NodeId>,
    pub dirty_flags: DirtyFlags,
    pub pseudo_state: ElementState,
    pub cached_id: Option<InternedStr>,
    pub class_hashes: SmallVec<[u64; 4]>,
    pub tag_hash: u64,
}

pub fn parse_non_negative_integer(input: &str) -> Option<u32> {
    let bytes = input.as_bytes();
    let mut index = 0;
    while bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
    {
        index += 1;
    }
    if bytes.get(index) == Some(&b'-') {
        return None;
    }
    if bytes.get(index) == Some(&b'+') {
        index += 1;
    }
    let start = index;
    let mut value = 0u32;
    while let Some(byte @ b'0'..=b'9') = bytes.get(index).copied() {
        value = value.wrapping_mul(10).wrapping_add(u32::from(byte - b'0'));
        index += 1;
    }
    (index > start).then_some(value)
}

impl ElementData {
    pub fn new(name: QualName) -> Self {
        let tag_hash = hash_ident_ascii_lower(&name.local);
        let mut elem = Self {
            name,
            attributes: Vec::new(),
            template_contents: None,
            shadow_root: None,
            content_document: None,
            dirty_flags: DirtyFlags::SELF_STYLE_DIRTY
                | DirtyFlags::SELF_LAYOUT_DIRTY
                | DirtyFlags::PAINT_DIRTY,
            pseudo_state: ElementState::empty(),
            cached_id: None,
            class_hashes: SmallVec::new(),
            tag_hash,
        };
        elem.sync_intrinsic_state();
        elem
    }

    pub fn sync_cached_attrs(&mut self) {
        self.cached_id = self.id().filter(|id| !id.is_empty()).map(intern_str);
        let mut hashes = SmallVec::new();
        if let Some(classes) = self.get_attribute("class") {
            for cls in classes.split_ascii_whitespace() {
                let h = hash_ident(cls);
                if !hashes.contains(&h) {
                    hashes.push(h);
                }
            }
        }
        self.class_hashes = hashes;
        self.sync_intrinsic_state();
    }

    fn sync_intrinsic_state(&mut self) {
        let local = self.name.local.as_str();
        let is_link = matches!(local, "a" | "area") && self.has_attribute("href");
        self.pseudo_state.set(ElementState::LINK, is_link);

        let is_checked = self.has_attribute("checked");
        self.pseudo_state.set(ElementState::CHECKED, is_checked);

        let is_disableable = matches!(
            local,
            "button" | "input" | "select" | "textarea" | "fieldset" | "option" | "optgroup"
        );
        if is_disableable {
            let disabled = self.has_attribute("disabled");
            self.pseudo_state.set(ElementState::DISABLED, disabled);
            self.pseudo_state.set(ElementState::ENABLED, !disabled);
        } else {
            self.pseudo_state
                .remove(ElementState::DISABLED | ElementState::ENABLED);
        }
    }

    #[inline]
    pub fn has_class_hash(&self, hash: u64) -> bool {
        self.class_hashes.contains(&hash)
    }

    pub fn new_html(local_name: &str) -> Self {
        let qual_name = QualName::new(
            None,
            Namespace::from("http://www.w3.org/1999/xhtml"),
            LocalName::from(local_name),
        );
        Self::new(qual_name)
    }

    pub fn get_attribute(&self, local_name: &str) -> Option<&str> {
        for (name, value) in &self.attributes {
            if name.ns.is_empty()
                && (name.local.as_str() == local_name
                    || name.local.as_str().eq_ignore_ascii_case(local_name))
            {
                return Some(value);
            }
        }
        None
    }

    pub fn has_attribute(&self, local_name: &str) -> bool {
        self.get_attribute(local_name).is_some()
    }

    pub fn set_attribute(&mut self, local_name: &str, value: &str) {
        for (name, current) in &mut self.attributes {
            if name.ns.is_empty()
                && (name.local.as_str() == local_name
                    || name.local.as_str().eq_ignore_ascii_case(local_name))
            {
                if current == value {
                    return;
                }
                *current = value.to_string();
                self.dirty_flags |= DirtyFlags::SELF_STYLE_DIRTY
                    | DirtyFlags::SELF_LAYOUT_DIRTY
                    | DirtyFlags::PAINT_DIRTY;
                self.sync_cached_attrs();
                return;
            }
        }

        let qname = QualName::new(None, Namespace::from(""), LocalName::from(local_name));
        self.attributes.push((qname, value.to_string()));
        self.dirty_flags |=
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY;
        self.sync_cached_attrs();
    }

    pub fn remove_attribute(&mut self, local_name: &str) -> bool {
        let initial_len = self.attributes.len();
        self.attributes.retain(|(name, _)| {
            !name.ns.is_empty() || (!name.local.as_str().eq_ignore_ascii_case(local_name))
        });
        let changed = self.attributes.len() < initial_len;
        if changed {
            self.dirty_flags |= DirtyFlags::SELF_STYLE_DIRTY
                | DirtyFlags::SELF_LAYOUT_DIRTY
                | DirtyFlags::PAINT_DIRTY;
            self.sync_cached_attrs();
        }
        changed
    }

    pub fn get_attribute_ns(&self, namespace_uri: &str, local_name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(name, _)| &*name.ns == namespace_uri && &*name.local == local_name)
            .map(|(_, value)| value.as_str())
    }

    pub fn set_attribute_ns(
        &mut self,
        namespace_uri: &str,
        prefix: Option<&str>,
        local_name: &str,
        value: &str,
    ) -> Option<(String, Option<String>)> {
        if let Some((name, current)) = self
            .attributes
            .iter_mut()
            .find(|(name, _)| &*name.ns == namespace_uri && &*name.local == local_name)
        {
            if current == value && name.prefix.as_deref() == prefix {
                return Some((
                    current.clone(),
                    name.prefix.as_ref().map(ToString::to_string),
                ));
            }
            let old_value = std::mem::replace(current, value.to_string());
            let old_prefix = name.prefix.as_ref().map(ToString::to_string);
            name.prefix = prefix.map(Into::into);
            self.dirty_flags |= DirtyFlags::SELF_STYLE_DIRTY
                | DirtyFlags::SELF_LAYOUT_DIRTY
                | DirtyFlags::PAINT_DIRTY;
            self.sync_cached_attrs();
            return Some((old_value, old_prefix));
        }

        let name = QualName::new(
            prefix.map(Into::into),
            Namespace::from(namespace_uri),
            LocalName::from(local_name),
        );
        self.attributes.push((name, value.to_string()));
        self.dirty_flags |=
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY;
        self.sync_cached_attrs();
        None
    }

    pub fn remove_attribute_ns(&mut self, namespace_uri: &str, local_name: &str) -> Option<String> {
        let index = self
            .attributes
            .iter()
            .position(|(name, _)| &*name.ns == namespace_uri && &*name.local == local_name)?;
        let removed = self.attributes.remove(index).1;
        self.dirty_flags |=
            DirtyFlags::SELF_STYLE_DIRTY | DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::PAINT_DIRTY;
        self.sync_cached_attrs();
        Some(removed)
    }

    pub fn id(&self) -> Option<&str> {
        self.get_attribute("id")
    }

    pub fn classes_iter(&self) -> impl Iterator<Item = &str> {
        self.get_attribute("class")
            .into_iter()
            .flat_map(|classes| classes.split_ascii_whitespace())
    }

    pub fn classes(&self) -> Vec<&str> {
        self.classes_iter().collect()
    }

    pub fn has_class(&self, class_name: &str) -> bool {
        self.classes_iter().any(|c| c == class_name)
    }
}

impl fmt::Display for ElementData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}", self.name.local)?;
        for (qname, val) in &self.attributes {
            write!(f, " {}=\"{}\"", qname.local, val)?;
        }
        write!(f, ">")
    }
}

pub fn is_valid_shadow_host_name(local_name: &str) -> bool {
    matches!(
        local_name,
        "article"
            | "aside"
            | "blockquote"
            | "body"
            | "div"
            | "footer"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "main"
            | "nav"
            | "p"
            | "section"
            | "span"
    ) || is_valid_custom_element_name(local_name)
}

pub fn is_valid_custom_element_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "annotation-xml",
        "color-profile",
        "font-face",
        "font-face-src",
        "font-face-uri",
        "font-face-format",
        "font-face-name",
        "missing-glyph",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && name.contains('-')
        && !RESERVED.contains(&name)
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '.' | '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_attribute_case_insensitivity_and_dedup() {
        let mut element = ElementData::new_html("div");
        element.set_attribute("class", "container");
        assert_eq!(element.get_attribute("class"), Some("container"));
        assert_eq!(element.get_attribute("CLASS"), Some("container"));
        assert_eq!(element.get_attribute("Class"), Some("container"));

        element.set_attribute("Class", "container");
        assert_eq!(element.attributes.len(), 1);

        element.set_attribute("CLASS", "wrapper");
        assert_eq!(element.attributes.len(), 1);
        assert_eq!(element.get_attribute("class"), Some("wrapper"));

        let res1 = element.set_attribute_ns("", None, "tabindex", "0");
        assert!(res1.is_none());
        assert_eq!(element.attributes.len(), 2);

        let res2 = element.set_attribute_ns("", None, "tabindex", "0");
        assert_eq!(res2, Some(("0".to_string(), None)));
        assert_eq!(element.attributes.len(), 2);
    }
}
