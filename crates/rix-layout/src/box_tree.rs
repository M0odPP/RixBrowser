//! CSS Box Tree construction from `Document` + `StyleContext`.
//!
//! Performs `display: none` pruning, `display: contents` flattening,
//! anonymous block wrapper generation for mixed block/inline children,
//! CSS 2.1 anonymous table fixup (`table` -> `table-row-group` -> `table-row` -> `table-cell`),
//! `::before`/`::after` pseudo-box emission, replaced element metadata extraction
//! (`<img>`, `<svg>`, `<input>`, `<button>`, `<textarea>`, `<select>`, `<br>`, `<hr>`),
//! and structural/style subtree hashing for O(1) incremental fragment caching.

use std::collections::HashMap;
use std::sync::Arc;

use rix_core::Size;
use rix_css::{
    ColorRgba, ComputedStyle, DisplayMode, FloatMode, PositionMode, RixPseudoElement, StyleContext,
};
use rix_dom::{
    svg_element_to_path_d, DirtyFlags, Document, ElementData, NodeData, NodeId, SvgViewBox,
};
use rix_ipc::{ImageId, SurfaceId};

/// Deterministic GPU `ExternalSurface` identifier derived from a DOM `NodeId`
/// (zero pixel arrays or graphics state stored inside DOM nodes!).
#[inline]
#[must_use]
pub fn surface_id_for_node(node: NodeId) -> SurfaceId {
    SurfaceId((usize::from(node) as u64).wrapping_add(1))
}

use crate::assets::{parse_html_dimension_attr, parse_svg_color, AssetRegistry};

/// Dense index identifying a [`LayoutBox`] in a [`BoxTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BoxId(pub u32);

/// Formatting context established by a container box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormattingContextKind {
    Block,
    Inline,
    Flex,
    Grid,
    Table,
    Replaced,
}

/// Kind of form control replaced element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormControlKind {
    TextInput,
    Button,
    Checkbox,
    Radio,
    TextArea,
    Select,
}

/// Extracted vector primitive inside an inline `<svg>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct SvgPathEntry {
    pub path_d: String,
    pub fill: Option<ColorRgba>,
    pub stroke: Option<(ColorRgba, f32)>,
}

/// Replaced element payload carried on [`BoxKind::Replaced`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReplacedKind {
    Image {
        src: String,
        image_id: Option<ImageId>,
        intrinsic_size: Size,
    },
    Svg {
        view_box: Option<SvgViewBox>,
        intrinsic_size: Size,
        paths: Vec<SvgPathEntry>,
    },
    FormControl {
        kind: FormControlKind,
        text: String,
        placeholder: String,
        intrinsic_size: Size,
    },
    ExternalSurface {
        surface_id: SurfaceId,
        intrinsic_size: Size,
    },
    LineBreak,
    HorizontalRule,
}

/// Semantic kind of a CSS [`LayoutBox`].
#[derive(Debug, Clone, PartialEq)]
pub enum BoxKind {
    Block {
        formatting_context: FormattingContextKind,
    },
    Inline,
    InlineBlock {
        formatting_context: FormattingContextKind,
    },
    Text(String),
    Replaced(ReplacedKind),
    Table,
    TableRowGroup,
    TableRow,
    TableCell {
        colspan: usize,
        rowspan: usize,
    },
    AnonymousBlock,
    AnonymousTable,
    AnonymousTableRow,
    AnonymousTableCell,
}

/// Single node in the CSS [`BoxTree`].
#[derive(Debug, Clone)]
pub struct LayoutBox {
    pub id: BoxId,
    pub dom_node: Option<NodeId>,
    pub kind: BoxKind,
    pub style: Arc<ComputedStyle>,
    pub children: Vec<BoxId>,
    pub is_out_of_flow: bool,
    pub is_float: bool,
    pub establishes_bfc: bool,
    pub subtree_dirty: bool,
    pub subtree_hash: u64,
}

impl LayoutBox {
    /// Returns `true` if this box participates as a block-level box in flow.
    #[must_use]
    pub fn is_block_level(&self) -> bool {
        if self.is_out_of_flow {
            return false;
        }
        matches!(
            self.kind,
            BoxKind::Block { .. }
                | BoxKind::Table
                | BoxKind::AnonymousBlock
                | BoxKind::AnonymousTable
                | BoxKind::Replaced(ReplacedKind::HorizontalRule)
        )
    }

    /// Returns `true` if this box participates as an inline-level box in flow.
    #[must_use]
    pub fn is_inline_level(&self) -> bool {
        if self.is_out_of_flow || self.is_float {
            return false;
        }
        matches!(
            self.kind,
            BoxKind::Inline
                | BoxKind::InlineBlock { .. }
                | BoxKind::Text(_)
                | BoxKind::Replaced(
                    ReplacedKind::Image { .. }
                        | ReplacedKind::Svg { .. }
                        | ReplacedKind::FormControl { .. }
                        | ReplacedKind::ExternalSurface { .. }
                        | ReplacedKind::LineBreak
                )
        )
    }

    #[must_use]
    pub fn is_table_internal(&self) -> bool {
        matches!(
            self.kind,
            BoxKind::TableRowGroup
                | BoxKind::TableRow
                | BoxKind::TableCell { .. }
                | BoxKind::AnonymousTableRow
                | BoxKind::AnonymousTableCell
        )
    }

    #[must_use]
    pub fn formatting_context(&self) -> FormattingContextKind {
        match self.kind {
            BoxKind::Block { formatting_context } | BoxKind::InlineBlock { formatting_context } => {
                formatting_context
            }
            BoxKind::Table | BoxKind::AnonymousTable => FormattingContextKind::Table,
            BoxKind::Replaced(_) => FormattingContextKind::Replaced,
            BoxKind::Inline | BoxKind::Text(_) => FormattingContextKind::Inline,
            _ => FormattingContextKind::Block,
        }
    }
}

/// Arena-backed CSS Box Tree produced from a styled DOM [`Document`].
#[derive(Debug, Default)]
pub struct BoxTree {
    pub boxes: Vec<LayoutBox>,
    pub root: Option<BoxId>,
    pub dom_to_box: HashMap<NodeId, BoxId>,
}

impl BoxTree {
    #[must_use]
    pub fn get(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id.0 as usize]
    }

    pub fn get_mut(&mut self, id: BoxId) -> &mut LayoutBox {
        &mut self.boxes[id.0 as usize]
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// Builds a full CSS [`BoxTree`] from `doc` and its resolved `styles`.
    #[must_use]
    pub fn build(doc: &Document, styles: &StyleContext, assets: &AssetRegistry) -> Self {
        let mut builder = BoxTreeBuilder {
            tree: Self::default(),
            doc,
            styles,
            assets,
        };
        if let Some(doc_elem) = doc.document_element() {
            let roots = builder.build_dom_node(doc_elem, None);
            if let Some(&first) = roots.first() {
                builder.tree.root = Some(first);
                builder.finalize_subtree_hashes(first);
            }
        }
        builder.tree
    }
}

struct BoxTreeBuilder<'a> {
    tree: BoxTree,
    doc: &'a Document,
    styles: &'a StyleContext,
    assets: &'a AssetRegistry,
}

impl<'a> BoxTreeBuilder<'a> {
    fn alloc_box(&mut self, mut layout_box: LayoutBox) -> BoxId {
        let id = BoxId(self.tree.boxes.len() as u32);
        layout_box.id = id;
        if let Some(dom) = layout_box.dom_node {
            self.tree.dom_to_box.entry(dom).or_insert(id);
        }
        self.tree.boxes.push(layout_box);
        id
    }

    fn build_dom_node(
        &mut self,
        node: NodeId,
        parent_style: Option<&Arc<ComputedStyle>>,
    ) -> Vec<BoxId> {
        let Some(data) = self.doc.get(node) else {
            return Vec::new();
        };
        match data {
            NodeData::Text(text) => {
                if text.is_empty() {
                    return Vec::new();
                }
                let style = parent_style
                    .cloned()
                    .unwrap_or_else(|| Arc::new(ComputedStyle::default()));
                let id = self.alloc_box(LayoutBox {
                    id: BoxId(0),
                    dom_node: Some(node),
                    kind: BoxKind::Text(text.clone()),
                    style,
                    children: Vec::new(),
                    is_out_of_flow: false,
                    is_float: false,
                    establishes_bfc: false,
                    subtree_dirty: false,
                    subtree_hash: 0,
                });
                vec![id]
            }
            NodeData::Element(elem) => self.build_element(node, elem, parent_style),
            NodeData::ShadowRoot { .. } | NodeData::DocumentFragment => {
                let mut out = Vec::new();
                for child in self.doc.children(node) {
                    out.extend(self.build_dom_node(child, parent_style));
                }
                out
            }
            NodeData::Document | NodeData::DocumentType { .. } | NodeData::Comment(_) => Vec::new(),
        }
    }

    fn build_element(
        &mut self,
        node: NodeId,
        elem: &ElementData,
        parent_style: Option<&Arc<ComputedStyle>>,
    ) -> Vec<BoxId> {
        let tag = elem.name.local.as_str();
        if matches!(
            tag,
            "head" | "script" | "style" | "meta" | "link" | "title" | "noscript" | "template"
        ) {
            return Vec::new();
        }

        let style = self.styles.style_for(node).cloned().unwrap_or_else(|| {
            parent_style.map_or_else(
                || Arc::new(ComputedStyle::default()),
                |p| Arc::new(ComputedStyle::inherit_from(p)),
            )
        });

        if style.display == DisplayMode::None {
            return Vec::new();
        }

        // `display: contents` generates no box of its own; its children participate directly.
        if style.display == DisplayMode::Contents {
            let mut flattened = Vec::new();
            let children: Vec<NodeId> = self.doc.children(node).collect();
            for child in children {
                flattened.extend(self.build_dom_node(child, Some(&style)));
            }
            return flattened;
        }

        let is_dirty = elem
            .dirty_flags
            .intersects(DirtyFlags::SELF_LAYOUT_DIRTY | DirtyFlags::DESCENDANT_LAYOUT_DIRTY);

        // Replaced elements (`<img>`, `<svg>`, `<input>`, `<button>`, `<textarea>`, `<select>`, `<br>`, `<hr>`)
        if let Some(replaced) = self.try_build_replaced(node, elem, &style) {
            let is_out_of_flow =
                matches!(style.position, PositionMode::Absolute | PositionMode::Fixed);
            let is_float = !is_out_of_flow && style.float_mode != FloatMode::None;
            let id = self.alloc_box(LayoutBox {
                id: BoxId(0),
                dom_node: Some(node),
                kind: BoxKind::Replaced(replaced),
                style,
                children: Vec::new(),
                is_out_of_flow,
                is_float,
                establishes_bfc: true,
                subtree_dirty: is_dirty,
                subtree_hash: 0,
            });
            return vec![id];
        }

        let is_out_of_flow = matches!(style.position, PositionMode::Absolute | PositionMode::Fixed);
        let is_float = !is_out_of_flow && style.float_mode != FloatMode::None;
        let kind = classify_element_box_kind(tag, elem, &style, is_float, is_out_of_flow);
        let establishes_bfc = box_establishes_bfc(&kind, &style, is_float, is_out_of_flow);

        let mut raw_children = Vec::new();

        // Emit `::before` generated pseudo-box if `content` is specified.
        if let Some(before_box) = self.build_pseudo_box(node, RixPseudoElement::Before) {
            raw_children.push(before_box);
        }

        // If element hosts a Shadow DOM, render its shadow root children; otherwise light children.
        let child_source = elem.shadow_root.unwrap_or(node);
        let dom_children: Vec<NodeId> = self.doc.children(child_source).collect();
        for child in dom_children {
            raw_children.extend(self.build_dom_node(child, Some(&style)));
        }

        // Emit `::after` generated pseudo-box if `content` is specified.
        if let Some(after_box) = self.build_pseudo_box(node, RixPseudoElement::After) {
            raw_children.push(after_box);
        }

        let fixed_children = self.fixup_children(&kind, &style, raw_children);

        let id = self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: Some(node),
            kind,
            style,
            children: fixed_children,
            is_out_of_flow,
            is_float,
            establishes_bfc,
            subtree_dirty: is_dirty,
            subtree_hash: 0,
        });
        vec![id]
    }

    fn build_pseudo_box(&mut self, host: NodeId, pseudo: RixPseudoElement) -> Option<BoxId> {
        let pseudo_style = self.styles.pseudo_style_for(host, pseudo)?.clone();
        if pseudo_style.display == DisplayMode::None {
            return None;
        }
        let content_text = pseudo_style.box_model.content.as_ref()?.to_string();
        let text_id = self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: None,
            kind: BoxKind::Text(content_text),
            style: Arc::clone(&pseudo_style),
            children: Vec::new(),
            is_out_of_flow: false,
            is_float: false,
            establishes_bfc: false,
            subtree_dirty: false,
            subtree_hash: 0,
        });
        let is_out_of_flow = matches!(
            pseudo_style.position,
            PositionMode::Absolute | PositionMode::Fixed
        );
        let is_float = !is_out_of_flow && pseudo_style.float_mode != FloatMode::None;
        let kind = match pseudo_style.display {
            DisplayMode::Block | DisplayMode::FlowRoot | DisplayMode::ListItem => BoxKind::Block {
                formatting_context: FormattingContextKind::Block,
            },
            DisplayMode::InlineBlock => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Block,
            },
            DisplayMode::Flex => BoxKind::Block {
                formatting_context: FormattingContextKind::Flex,
            },
            DisplayMode::InlineFlex => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Flex,
            },
            DisplayMode::Grid => BoxKind::Block {
                formatting_context: FormattingContextKind::Grid,
            },
            DisplayMode::InlineGrid => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Grid,
            },
            _ => BoxKind::Inline,
        };
        let establishes_bfc = box_establishes_bfc(&kind, &pseudo_style, is_float, is_out_of_flow);
        Some(self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: None,
            kind,
            style: pseudo_style,
            children: vec![text_id],
            is_out_of_flow,
            is_float,
            establishes_bfc,
            subtree_dirty: false,
            subtree_hash: 0,
        }))
    }

    fn try_build_replaced(
        &self,
        node: NodeId,
        elem: &ElementData,
        style: &ComputedStyle,
    ) -> Option<ReplacedKind> {
        match elem.name.local.as_str() {
            "br" => Some(ReplacedKind::LineBreak),
            "hr" => Some(ReplacedKind::HorizontalRule),
            "canvas" => {
                let iw = elem
                    .get_attribute("width")
                    .and_then(parse_html_dimension_attr)
                    .unwrap_or(300.0);
                let ih = elem
                    .get_attribute("height")
                    .and_then(parse_html_dimension_attr)
                    .unwrap_or(150.0);
                Some(ReplacedKind::ExternalSurface {
                    surface_id: surface_id_for_node(node),
                    intrinsic_size: Size::new(iw, ih),
                })
            }
            "img" => {
                let src = elem.get_attribute("src").unwrap_or_default().to_string();
                let decoded = self.assets.get_by_src(&src);
                let attr_w = elem
                    .get_attribute("width")
                    .and_then(parse_html_dimension_attr);
                let attr_h = elem
                    .get_attribute("height")
                    .and_then(parse_html_dimension_attr);
                let (iw, ih) = match (decoded, attr_w, attr_h) {
                    (Some(_), Some(w), Some(h)) => (w, h),
                    (Some(img), Some(w), None) => {
                        let ratio = img.height as f32 / (img.width.max(1) as f32);
                        (w, (w * ratio).max(1.0))
                    }
                    (Some(img), None, Some(h)) => {
                        let ratio = img.width as f32 / (img.height.max(1) as f32);
                        ((h * ratio).max(1.0), h)
                    }
                    (Some(img), None, None) => (img.width as f32, img.height as f32),
                    (None, Some(w), Some(h)) => (w, h),
                    (None, Some(w), None) => (w, w),
                    (None, None, Some(h)) => (h, h),
                    (None, None, None) => (0.0, 0.0),
                };
                Some(ReplacedKind::Image {
                    src,
                    image_id: decoded.map(|d| d.image_id),
                    intrinsic_size: Size::new(iw, ih),
                })
            }
            "svg" => {
                let view_box = elem.get_attribute("viewBox").and_then(SvgViewBox::parse);
                let attr_w = elem
                    .get_attribute("width")
                    .and_then(parse_html_dimension_attr);
                let attr_h = elem
                    .get_attribute("height")
                    .and_then(parse_html_dimension_attr);
                let iw = attr_w
                    .or_else(|| view_box.map(|vb| vb.width))
                    .unwrap_or(16.0);
                let ih = attr_h
                    .or_else(|| view_box.map(|vb| vb.height))
                    .unwrap_or(16.0);
                let mut paths = Vec::new();
                let inherited_fill = elem
                    .get_attribute("fill")
                    .map(|raw| parse_svg_color(raw, style.text.color));
                let inherited_stroke = elem
                    .get_attribute("stroke")
                    .and_then(|raw| parse_svg_color(raw, style.text.color));
                let inherited_stroke_w = elem
                    .get_attribute("stroke-width")
                    .and_then(parse_html_dimension_attr)
                    .unwrap_or(1.5);
                self.collect_svg_paths(
                    node,
                    style.text.color,
                    inherited_fill,
                    inherited_stroke,
                    inherited_stroke_w,
                    &mut paths,
                );
                Some(ReplacedKind::Svg {
                    view_box,
                    intrinsic_size: Size::new(iw, ih),
                    paths,
                })
            }
            "input" => {
                let input_type = elem
                    .get_attribute("type")
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                let value = elem.get_attribute("value").unwrap_or_default().to_string();
                let placeholder = elem
                    .get_attribute("placeholder")
                    .unwrap_or_default()
                    .to_string();
                let (kind, size) = match input_type.as_str() {
                    "hidden" => return None,
                    "checkbox" => (FormControlKind::Checkbox, Size::new(16.0, 16.0)),
                    "radio" => (FormControlKind::Radio, Size::new(16.0, 16.0)),
                    "submit" | "button" | "reset" => {
                        let label_w =
                            (value.len().max(6) as f32 * style.text.font_size_px * 0.56).max(64.0);
                        (
                            FormControlKind::Button,
                            Size::new(label_w + 16.0, style.text.font_size_px * 1.8),
                        )
                    }
                    _ => (
                        FormControlKind::TextInput,
                        Size::new(180.0, (style.text.font_size_px * 1.9).max(28.0)),
                    ),
                };
                Some(ReplacedKind::FormControl {
                    kind,
                    text: value,
                    placeholder,
                    intrinsic_size: size,
                })
            }
            "button" => {
                let text = self.collect_plain_text(node);
                let label_w =
                    (text.chars().count().max(4) as f32 * style.text.font_size_px * 0.56).max(48.0);
                Some(ReplacedKind::FormControl {
                    kind: FormControlKind::Button,
                    text,
                    placeholder: String::new(),
                    intrinsic_size: Size::new(
                        label_w + 20.0,
                        (style.text.font_size_px * 1.9).max(28.0),
                    ),
                })
            }
            "textarea" => {
                let text = self.collect_plain_text(node);
                let placeholder = elem
                    .get_attribute("placeholder")
                    .unwrap_or_default()
                    .to_string();
                let cols = elem
                    .get_attribute("cols")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(24.0);
                let rows = elem
                    .get_attribute("rows")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(3.0);
                Some(ReplacedKind::FormControl {
                    kind: FormControlKind::TextArea,
                    text,
                    placeholder,
                    intrinsic_size: Size::new(
                        cols * style.text.font_size_px * 0.6,
                        rows * style.text.font_size_px * 1.4 + 12.0,
                    ),
                })
            }
            "select" => {
                let text = self.first_option_text(node);
                Some(ReplacedKind::FormControl {
                    kind: FormControlKind::Select,
                    text,
                    placeholder: String::new(),
                    intrinsic_size: Size::new(140.0, (style.text.font_size_px * 1.9).max(28.0)),
                })
            }
            _ => None,
        }
    }

    fn collect_svg_paths(
        &self,
        parent: NodeId,
        current_color: ColorRgba,
        inherited_fill: Option<Option<ColorRgba>>,
        inherited_stroke: Option<ColorRgba>,
        inherited_stroke_w: f32,
        out: &mut Vec<SvgPathEntry>,
    ) {
        for child in self.doc.children(parent) {
            let Some(NodeData::Element(elem)) = self.doc.get(child) else {
                continue;
            };
            let fill = match elem.get_attribute("fill") {
                Some(raw) => Some(parse_svg_color(raw, current_color)),
                None => inherited_fill,
            };
            let stroke = match elem.get_attribute("stroke") {
                Some(raw) => parse_svg_color(raw, current_color),
                None => inherited_stroke,
            };
            let stroke_w = elem
                .get_attribute("stroke-width")
                .and_then(parse_html_dimension_attr)
                .unwrap_or(inherited_stroke_w);

            if let Some(d) = svg_element_to_path_d(elem) {
                let resolved_fill = match fill {
                    Some(opt) => opt,
                    None if stroke.is_some() => None,
                    None => Some(current_color),
                };
                out.push(SvgPathEntry {
                    path_d: d,
                    fill: resolved_fill,
                    stroke: stroke.map(|c| (c, stroke_w)),
                });
            }
            self.collect_svg_paths(child, current_color, fill, stroke, stroke_w, out);
        }
    }

    fn collect_plain_text(&self, node: NodeId) -> String {
        let mut out = String::new();
        for desc in self.doc.descendants(node) {
            if let Some(NodeData::Text(t)) = self.doc.get(desc) {
                out.push_str(t);
            }
        }
        out.trim().to_string()
    }

    fn first_option_text(&self, node: NodeId) -> String {
        for desc in self.doc.descendants(node) {
            if let Some(NodeData::Element(e)) = self.doc.get(desc) {
                if e.name.local.eq_str_ignore_ascii_case("option") {
                    return self.collect_plain_text(desc);
                }
            }
        }
        String::new()
    }

    /// Performs CSS 2.1 anonymous box fixup (mixed block/inline children + table fixup).
    fn fixup_children(
        &mut self,
        parent_kind: &BoxKind,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        if children.is_empty() {
            return children;
        }
        match parent_kind {
            BoxKind::Table | BoxKind::AnonymousTable => {
                self.fixup_table_children(parent_style, children)
            }
            BoxKind::TableRowGroup => self.fixup_row_group_children(parent_style, children),
            BoxKind::TableRow | BoxKind::AnonymousTableRow => {
                self.fixup_row_children(parent_style, children)
            }
            BoxKind::Block {
                formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
            }
            | BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
            } => self.fixup_flex_grid_children(parent_style, children),
            BoxKind::Block { .. }
            | BoxKind::InlineBlock { .. }
            | BoxKind::TableCell { .. }
            | BoxKind::AnonymousBlock
            | BoxKind::AnonymousTableCell => {
                // First wrap any stray table-internal children inside an anonymous table.
                let children = self.wrap_stray_table_internals(parent_style, children);
                self.fixup_block_children(parent_style, children)
            }
            BoxKind::Inline => children,
            _ => children,
        }
    }

    fn wrap_stray_table_internals(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        let has_stray = children
            .iter()
            .any(|&cid| self.tree.get(cid).is_table_internal());
        if !has_stray {
            return children;
        }
        let mut out = Vec::with_capacity(children.len());
        let mut run = Vec::new();
        for cid in children {
            if self.tree.get(cid).is_table_internal()
                || (!run.is_empty() && self.is_whitespace_only(cid))
            {
                run.push(cid);
            } else {
                if !run.is_empty() {
                    let anon_children =
                        self.fixup_table_children(parent_style, std::mem::take(&mut run));
                    let anon = self.alloc_anonymous(
                        BoxKind::AnonymousTable,
                        parent_style,
                        anon_children,
                        true,
                    );
                    out.push(anon);
                }
                out.push(cid);
            }
        }
        if !run.is_empty() {
            let anon_children = self.fixup_table_children(parent_style, run);
            let anon =
                self.alloc_anonymous(BoxKind::AnonymousTable, parent_style, anon_children, true);
            out.push(anon);
        }
        out
    }

    fn fixup_table_children(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        let mut out = Vec::new();
        let mut stray_cells = Vec::new();
        for cid in children {
            match self.tree.get(cid).kind {
                BoxKind::TableRowGroup | BoxKind::TableRow | BoxKind::AnonymousTableRow => {
                    if !stray_cells.is_empty() {
                        let row_children =
                            self.fixup_row_children(parent_style, std::mem::take(&mut stray_cells));
                        out.push(self.alloc_anonymous(
                            BoxKind::AnonymousTableRow,
                            parent_style,
                            row_children,
                            false,
                        ));
                    }
                    out.push(cid);
                }
                BoxKind::Text(ref t) if t.trim().is_empty() => {}
                _ => stray_cells.push(cid),
            }
        }
        if !stray_cells.is_empty() {
            let row_children = self.fixup_row_children(parent_style, stray_cells);
            out.push(self.alloc_anonymous(
                BoxKind::AnonymousTableRow,
                parent_style,
                row_children,
                false,
            ));
        }
        out
    }

    fn fixup_row_group_children(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        let mut out = Vec::new();
        let mut stray = Vec::new();
        for cid in children {
            match self.tree.get(cid).kind {
                BoxKind::TableRow | BoxKind::AnonymousTableRow => {
                    if !stray.is_empty() {
                        let row_children =
                            self.fixup_row_children(parent_style, std::mem::take(&mut stray));
                        out.push(self.alloc_anonymous(
                            BoxKind::AnonymousTableRow,
                            parent_style,
                            row_children,
                            false,
                        ));
                    }
                    out.push(cid);
                }
                BoxKind::Text(ref t) if t.trim().is_empty() => {}
                _ => stray.push(cid),
            }
        }
        if !stray.is_empty() {
            let row_children = self.fixup_row_children(parent_style, stray);
            out.push(self.alloc_anonymous(
                BoxKind::AnonymousTableRow,
                parent_style,
                row_children,
                false,
            ));
        }
        out
    }

    fn fixup_row_children(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        let mut out = Vec::new();
        let mut stray = Vec::new();
        for cid in children {
            match self.tree.get(cid).kind {
                BoxKind::TableCell { .. } | BoxKind::AnonymousTableCell => {
                    if !stray.is_empty() {
                        let cell_children =
                            self.fixup_block_children(parent_style, std::mem::take(&mut stray));
                        out.push(self.alloc_anonymous(
                            BoxKind::AnonymousTableCell,
                            parent_style,
                            cell_children,
                            true,
                        ));
                    }
                    out.push(cid);
                }
                BoxKind::Text(ref t) if t.trim().is_empty() => {}
                _ => stray.push(cid),
            }
        }
        if !stray.is_empty() {
            let cell_children = self.fixup_block_children(parent_style, stray);
            out.push(self.alloc_anonymous(
                BoxKind::AnonymousTableCell,
                parent_style,
                cell_children,
                true,
            ));
        }
        out
    }

    fn fixup_flex_grid_children(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        // Wrap direct non-whitespace text children of Flex/Grid containers into anonymous block items.
        let mut out = Vec::with_capacity(children.len());
        for cid in children {
            match &self.tree.get(cid).kind {
                BoxKind::Text(t) if t.trim().is_empty() => {}
                BoxKind::Text(_) => {
                    out.push(self.alloc_anonymous(
                        BoxKind::AnonymousBlock,
                        parent_style,
                        vec![cid],
                        true,
                    ));
                }
                _ => out.push(cid),
            }
        }
        out
    }

    fn fixup_block_children(
        &mut self,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
    ) -> Vec<BoxId> {
        let has_block = children
            .iter()
            .any(|&cid| self.tree.get(cid).is_block_level());
        if !has_block {
            return children;
        }
        let mut out = Vec::with_capacity(children.len());
        let mut inline_run = Vec::new();
        for cid in children {
            let child = self.tree.get(cid);
            if child.is_block_level() || child.is_out_of_flow || child.is_float {
                if !inline_run.is_empty() {
                    let has_visible = inline_run.iter().any(|&id| !self.is_whitespace_only(id));
                    let run = std::mem::take(&mut inline_run);
                    if has_visible {
                        out.push(self.alloc_anonymous(
                            BoxKind::AnonymousBlock,
                            parent_style,
                            run,
                            false,
                        ));
                    }
                }
                out.push(cid);
            } else {
                inline_run.push(cid);
            }
        }
        if !inline_run.is_empty() {
            let has_visible = inline_run.iter().any(|&id| !self.is_whitespace_only(id));
            if has_visible {
                out.push(self.alloc_anonymous(
                    BoxKind::AnonymousBlock,
                    parent_style,
                    inline_run,
                    false,
                ));
            }
        }
        out
    }

    fn is_whitespace_only(&self, id: BoxId) -> bool {
        matches!(&self.tree.get(id).kind, BoxKind::Text(t) if t.trim().is_empty())
    }

    fn alloc_anonymous(
        &mut self,
        kind: BoxKind,
        parent_style: &Arc<ComputedStyle>,
        children: Vec<BoxId>,
        establishes_bfc: bool,
    ) -> BoxId {
        let mut anon_style = ComputedStyle::inherit_from(parent_style);
        anon_style.display = match kind {
            BoxKind::AnonymousTable => DisplayMode::Table,
            BoxKind::AnonymousTableRow => DisplayMode::TableRow,
            BoxKind::AnonymousTableCell => DisplayMode::TableCell,
            _ => DisplayMode::Block,
        };
        let subtree_dirty = children.iter().any(|&c| self.tree.get(c).subtree_dirty);
        self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: None,
            kind,
            style: Arc::new(anon_style),
            children,
            is_out_of_flow: false,
            is_float: false,
            establishes_bfc,
            subtree_dirty,
            subtree_hash: 0,
        })
    }

    fn finalize_subtree_hashes(&mut self, id: BoxId) -> (bool, u64) {
        let child_ids = self.tree.get(id).children.clone();
        let mut dirty = self.tree.get(id).subtree_dirty;
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        if let Some(dom) = self.tree.get(id).dom_node {
            let style_ptr = Arc::as_ptr(&self.tree.get(id).style) as usize as u64;
            hash = fnv_mix(hash, style_ptr);
            hash = fnv_mix(hash, usize::from(dom) as u64);
        } else {
            let kind_tag: u64 = match self.tree.get(id).kind {
                BoxKind::AnonymousBlock => 1,
                BoxKind::AnonymousTable => 2,
                BoxKind::AnonymousTableRow => 3,
                BoxKind::AnonymousTableCell => 4,
                _ => 5,
            };
            hash = fnv_mix(hash, kind_tag);
        }
        match &self.tree.get(id).kind {
            BoxKind::Text(t) => {
                for &b in t.as_bytes() {
                    hash ^= u64::from(b);
                    hash = hash.wrapping_mul(0x0100_0000_01b3);
                }
            }
            BoxKind::TableCell { colspan, rowspan } => {
                hash = fnv_mix(hash, ((*colspan as u64) << 16) | (*rowspan as u64));
            }
            BoxKind::Replaced(ReplacedKind::Image {
                image_id,
                intrinsic_size,
                ..
            }) => {
                hash = fnv_mix(hash, image_id.map_or(0, |i| i.0));
                hash = fnv_mix(
                    hash,
                    (u64::from(intrinsic_size.width.to_bits()) << 32)
                        | u64::from(intrinsic_size.height.to_bits()),
                );
            }
            BoxKind::Replaced(ReplacedKind::ExternalSurface {
                surface_id,
                intrinsic_size,
            }) => {
                hash = fnv_mix(hash, surface_id.0);
                hash = fnv_mix(
                    hash,
                    (u64::from(intrinsic_size.width.to_bits()) << 32)
                        | u64::from(intrinsic_size.height.to_bits()),
                );
            }
            _ => {}
        }
        for cid in child_ids {
            let (child_dirty, child_hash) = self.finalize_subtree_hashes(cid);
            dirty |= child_dirty;
            hash = fnv_mix(hash, child_hash);
        }
        let b = self.tree.get_mut(id);
        b.subtree_dirty = dirty;
        b.subtree_hash = hash;
        (dirty, hash)
    }
}

#[inline]
fn fnv_mix(mut hash: u64, val: u64) -> u64 {
    hash ^= val;
    hash = hash.wrapping_mul(0x0100_0000_01b3);
    hash
}

fn classify_element_box_kind(
    tag: &str,
    elem: &ElementData,
    style: &ComputedStyle,
    is_float: bool,
    is_out_of_flow: bool,
) -> BoxKind {
    // HTML5 table row groups (`thead`, `tbody`, `tfoot`) act as table row groups when not overridden.
    if matches!(tag, "thead" | "tbody" | "tfoot")
        && matches!(style.display, DisplayMode::Inline | DisplayMode::Block)
    {
        return BoxKind::TableRowGroup;
    }

    // Floated or out-of-flow inline boxes blockify per CSS Display Level 3 §2.7.
    let effective_display = if (is_float || is_out_of_flow)
        && matches!(
            style.display,
            DisplayMode::Inline
                | DisplayMode::InlineBlock
                | DisplayMode::InlineFlex
                | DisplayMode::InlineGrid
                | DisplayMode::InlineTable
        ) {
        match style.display {
            DisplayMode::InlineFlex => DisplayMode::Flex,
            DisplayMode::InlineGrid => DisplayMode::Grid,
            DisplayMode::InlineTable => DisplayMode::Table,
            _ => DisplayMode::Block,
        }
    } else {
        style.display
    };

    match effective_display {
        DisplayMode::Flex => BoxKind::Block {
            formatting_context: FormattingContextKind::Flex,
        },
        DisplayMode::InlineFlex => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Flex,
        },
        DisplayMode::Grid => BoxKind::Block {
            formatting_context: FormattingContextKind::Grid,
        },
        DisplayMode::InlineGrid => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Grid,
        },
        DisplayMode::Table => BoxKind::Table,
        DisplayMode::InlineTable => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Table,
        },
        DisplayMode::TableRow => BoxKind::TableRow,
        DisplayMode::TableCell => {
            let colspan = elem
                .get_attribute("colspan")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000);
            let rowspan = elem
                .get_attribute("rowspan")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000);
            BoxKind::TableCell { colspan, rowspan }
        }
        DisplayMode::InlineBlock => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Block,
        },
        DisplayMode::Inline => BoxKind::Inline,
        DisplayMode::Block
        | DisplayMode::FlowRoot
        | DisplayMode::ListItem
        | DisplayMode::Contents
        | DisplayMode::None => BoxKind::Block {
            formatting_context: FormattingContextKind::Block,
        },
    }
}

fn box_establishes_bfc(
    kind: &BoxKind,
    style: &ComputedStyle,
    is_float: bool,
    is_out_of_flow: bool,
) -> bool {
    if is_float || is_out_of_flow {
        return true;
    }
    if style.display == DisplayMode::FlowRoot {
        return true;
    }
    if style.overflow.x != rix_css::OverflowMode::Visible
        || style.overflow.y != rix_css::OverflowMode::Visible
    {
        return true;
    }
    matches!(
        kind,
        BoxKind::InlineBlock { .. }
            | BoxKind::Table
            | BoxKind::AnonymousTable
            | BoxKind::TableCell { .. }
            | BoxKind::AnonymousTableCell
            | BoxKind::Block {
                formatting_context: FormattingContextKind::Flex
                    | FormattingContextKind::Grid
                    | FormattingContextKind::Table
            }
    )
}
