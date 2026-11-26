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
    ComputedStyle, DisplayMode, FloatMode, PositionMode, RixPseudoElement, StyleContext,
};
use rix_dom::{DirtyFlags, Document, ElementData, NodeData, NodeId, SvgViewBox};
use rix_ipc::{ImageId, SurfaceId};

/// Deterministic GPU `ExternalSurface` identifier derived from a DOM `NodeId`
/// (zero pixel arrays or graphics state stored inside DOM nodes!).
#[inline]
#[must_use]
pub fn surface_id_for_node(node: NodeId) -> SurfaceId {
    SurfaceId((usize::from(node) as u64).wrapping_add(1))
}

pub use crate::assets::SvgPathEntry;
use crate::assets::{parse_best_srcset_url, parse_html_dimension_attr, AssetRegistry};

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

/// Table row-group section classification per CSS Tables Level 3 §3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableSectionKind {
    Header,
    Body,
    Footer,
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
    TableCaption,
    TableColumnGroup {
        span: usize,
    },
    TableColumn {
        span: usize,
    },
    TableRowGroup {
        section: TableSectionKind,
    },
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
    pub lang: Option<rix_core::InternedStr>,
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
        match self.kind {
            BoxKind::Block { .. }
            | BoxKind::Table
            | BoxKind::AnonymousBlock
            | BoxKind::AnonymousTable
            | BoxKind::Replaced(ReplacedKind::HorizontalRule) => true,
            // CSS Tables L3 §2.1: replaced elements with `display: table` behave as `block`,
            // while other table display values behave as `inline`.
            BoxKind::Replaced(
                ReplacedKind::Image { .. }
                | ReplacedKind::Svg { .. }
                | ReplacedKind::FormControl { .. }
                | ReplacedKind::ExternalSurface { .. },
            ) => matches!(
                self.style.display,
                DisplayMode::Block
                    | DisplayMode::FlowRoot
                    | DisplayMode::ListItem
                    | DisplayMode::FlowRootListItem
                    | DisplayMode::Flex
                    | DisplayMode::Grid
                    | DisplayMode::GridLanes
                    | DisplayMode::Table
                    | DisplayMode::RunInTable
                    | DisplayMode::BlockMath
                    | DisplayMode::BlockRuby
            ),
            _ => false,
        }
    }

    /// Returns `true` if this box participates as an inline-level box in flow.
    #[must_use]
    pub fn is_inline_level(&self) -> bool {
        if self.is_out_of_flow || self.is_float || self.is_block_level() {
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

    /// Returns `true` for proper direct children of a `table-root` box (CSS Tables L3 §2.2).
    #[must_use]
    pub fn is_proper_table_child(&self) -> bool {
        matches!(
            self.kind,
            BoxKind::TableCaption
                | BoxKind::TableColumnGroup { .. }
                | BoxKind::TableColumn { .. }
                | BoxKind::TableRowGroup { .. }
                | BoxKind::TableRow
                | BoxKind::AnonymousTableRow
        )
    }

    /// Returns `true` for table-non-root boxes (CSS Tables L3 §2.1).
    #[must_use]
    pub fn is_table_internal(&self) -> bool {
        matches!(
            self.kind,
            BoxKind::TableCaption
                | BoxKind::TableColumnGroup { .. }
                | BoxKind::TableColumn { .. }
                | BoxKind::TableRowGroup { .. }
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
    /// Document quirks mode (`0` = NoQuirks, `1` = LimitedQuirks, `2` = Quirks).
    pub quirks_mode: u8,
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
            tree: Self {
                quirks_mode: doc.quirks_mode(),
                ..Self::default()
            },
            doc,
            styles,
            assets,
        };
        if let Some(doc_elem) = doc.document_element() {
            let roots = builder.build_dom_node(doc_elem, None, None);
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
        current_lang: Option<&rix_core::InternedStr>,
    ) -> Vec<BoxId> {
        let Some(data) = self.doc.get(node) else {
            return Vec::new();
        };
        match data {
            NodeData::Text(text) | NodeData::CDataSection(text) => {
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
                    lang: current_lang.cloned(),
                    children: Vec::new(),
                    is_out_of_flow: false,
                    is_float: false,
                    establishes_bfc: false,
                    subtree_dirty: false,
                    subtree_hash: 0,
                });
                vec![id]
            }
            NodeData::Element(elem) => self.build_element(node, elem, parent_style, current_lang),
            NodeData::ShadowRoot { .. } | NodeData::DocumentFragment => {
                let mut out = Vec::new();
                for child in self.doc.children(node) {
                    out.extend(self.build_dom_node(child, parent_style, current_lang));
                }
                out
            }
            NodeData::Document
            | NodeData::DocumentType { .. }
            | NodeData::Comment(_)
            | NodeData::ProcessingInstruction { .. } => Vec::new(),
        }
    }

    fn build_element(
        &mut self,
        node: NodeId,
        elem: &ElementData,
        parent_style: Option<&Arc<ComputedStyle>>,
        parent_lang: Option<&rix_core::InternedStr>,
    ) -> Vec<BoxId> {
        let tag = elem.name.local.as_str();
        if matches!(
            tag,
            "head" | "script" | "style" | "meta" | "link" | "title" | "noscript" | "template"
        ) {
            return Vec::new();
        }

        // HTML Living Standard §3.2.6.2: an explicit empty `lang=""` resets the
        // element's language to unknown rather than inheriting from ancestors.
        let effective_lang = if let Some(raw_lang) = elem
            .get_attribute("lang")
            .or_else(|| elem.get_attribute("xml:lang"))
        {
            let trimmed = raw_lang.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(rix_core::intern_str(trimmed))
            }
        } else {
            parent_lang.cloned()
        };

        let style = self.styles.style_for(node).cloned().unwrap_or_else(|| {
            parent_style.map_or_else(
                || Arc::new(ComputedStyle::default()),
                |p| Arc::new(ComputedStyle::inherit_from(p)),
            )
        });

        if style.display == DisplayMode::None {
            return Vec::new();
        }

        // WHATWG Shadow DOM flat-tree slotting: an HTML `<slot>` inside a `ShadowRoot`
        // lays out its assigned light-DOM slottables (or its own fallback children if none
        // are assigned) directly as `display: contents` when not given an explicit box display.
        if tag == "slot" && &*elem.name.ns == "http://www.w3.org/1999/xhtml" {
            if let Some(sr_id) = self.doc.shadow_root_containing(node) {
                if let Some(NodeData::ShadowRoot {
                    host: shadow_host,
                    slot_assignment,
                    ..
                }) = self.doc.get(sr_id)
                {
                    if slot_assignment == "named" {
                        let slot_name = elem.get_attribute("name").unwrap_or("");
                        // Only the FIRST `<slot>` with `slot_name` in tree order claims slottables.
                        let is_first = self.doc.descendants(sr_id).find(|&id| {
                            matches!(
                                self.doc.get(id),
                                Some(NodeData::Element(se))
                                    if &*se.name.ns == "http://www.w3.org/1999/xhtml"
                                        && &*se.name.local == "slot"
                                        && se.get_attribute("name").unwrap_or("") == slot_name
                            )
                        }) == Some(node);
                        let mut assigned = Vec::new();
                        if is_first {
                            for host_child in self.doc.children(*shadow_host) {
                                let matches_slot = match self.doc.get(host_child) {
                                    Some(NodeData::Element(ce)) => {
                                        ce.get_attribute("slot").unwrap_or("") == slot_name
                                    }
                                    Some(NodeData::Text(_)) => slot_name.is_empty(),
                                    _ => false,
                                };
                                if matches_slot {
                                    assigned.push(host_child);
                                }
                            }
                        }
                        let sources: Vec<NodeId> = if assigned.is_empty() {
                            self.doc.children(node).collect()
                        } else {
                            assigned
                        };
                        let mut flattened = Vec::new();
                        for child in sources {
                            flattened.extend(self.build_dom_node(
                                child,
                                parent_style,
                                effective_lang.as_ref(),
                            ));
                        }
                        return flattened;
                    }
                }
            }
        }

        // `display: contents` generates no box of its own; its children participate directly.
        // Per CSS Display L3 Appendix B, replaced elements with `display: contents` render as `none`.
        if style.display == DisplayMode::Contents {
            if matches!(
                tag,
                "img"
                    | "br"
                    | "wbr"
                    | "hr"
                    | "canvas"
                    | "iframe"
                    | "video"
                    | "audio"
                    | "input"
                    | "textarea"
                    | "select"
                    | "svg"
                    | "object"
                    | "embed"
            ) {
                return Vec::new();
            }
            let mut flattened = Vec::new();
            if let Some(before_box) =
                self.build_pseudo_box(node, RixPseudoElement::Before, effective_lang.as_ref())
            {
                flattened.push(before_box);
            }
            let child_source = elem.shadow_root.unwrap_or(node);
            let children: Vec<NodeId> = self.doc.children(child_source).collect();
            for child in children {
                flattened.extend(self.build_dom_node(child, Some(&style), effective_lang.as_ref()));
            }
            if let Some(after_box) =
                self.build_pseudo_box(node, RixPseudoElement::After, effective_lang.as_ref())
            {
                flattened.push(after_box);
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
                lang: effective_lang,
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
        // CSS 2.2 §9.4.1: The root element of the document establishes a block formatting context.
        let establishes_bfc =
            parent_style.is_none() || box_establishes_bfc(&kind, &style, is_float, is_out_of_flow);

        let mut raw_children = Vec::new();

        // CSS Tables L3 §2.2 Rule 1.1: Children of a `table-column` are treated as `display: none`.
        if !matches!(kind, BoxKind::TableColumn { .. }) {
            // Emit `::before` generated pseudo-box if `content` is specified.
            if !matches!(kind, BoxKind::TableColumnGroup { .. }) {
                if let Some(before_box) =
                    self.build_pseudo_box(node, RixPseudoElement::Before, effective_lang.as_ref())
                {
                    raw_children.push(before_box);
                }
            }

            // If element hosts a Shadow DOM, render its shadow root children; otherwise light children.
            let child_source = elem.shadow_root.unwrap_or(node);
            let dom_children: Vec<NodeId> = self.doc.children(child_source).collect();
            if matches!(kind, BoxKind::TableColumnGroup { .. }) {
                // CSS Tables L3 §2.2 Rule 1.2: Children of a `table-column-group` that are not
                // `table-column` are treated as if they had `display: none`.
                for child in dom_children {
                    let is_table_col = self.styles.style_for(child).is_some_and(|st| {
                        st.display == DisplayMode::TableColumn
                            && !matches!(st.position, PositionMode::Absolute | PositionMode::Fixed)
                            && st.float_mode == FloatMode::None
                    });
                    if is_table_col {
                        raw_children.extend(self.build_dom_node(
                            child,
                            Some(&style),
                            effective_lang.as_ref(),
                        ));
                    }
                }
            } else {
                for child in dom_children {
                    raw_children.extend(self.build_dom_node(
                        child,
                        Some(&style),
                        effective_lang.as_ref(),
                    ));
                }
            }

            // Emit `::after` generated pseudo-box if `content` is specified.
            if !matches!(kind, BoxKind::TableColumnGroup { .. }) {
                if let Some(after_box) =
                    self.build_pseudo_box(node, RixPseudoElement::After, effective_lang.as_ref())
                {
                    raw_children.push(after_box);
                }
            }
        }

        let fixed_children = self.fixup_children(&kind, &style, raw_children);

        let id = self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: Some(node),
            kind,
            style,
            lang: effective_lang,
            children: fixed_children,
            is_out_of_flow,
            is_float,
            establishes_bfc,
            subtree_dirty: is_dirty,
            subtree_hash: 0,
        });
        vec![id]
    }

    fn build_pseudo_box(
        &mut self,
        host: NodeId,
        pseudo: RixPseudoElement,
        effective_lang: Option<&rix_core::InternedStr>,
    ) -> Option<BoxId> {
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
            lang: effective_lang.cloned(),
            children: Vec::new(),
            is_out_of_flow: false,
            is_float: false,
            establishes_bfc: false,
            subtree_dirty: false,
            subtree_hash: 0,
        });
        if pseudo_style.display == DisplayMode::Contents {
            return Some(text_id);
        }
        let is_out_of_flow = matches!(
            pseudo_style.position,
            PositionMode::Absolute | PositionMode::Fixed
        );
        let is_float = !is_out_of_flow && pseudo_style.float_mode != FloatMode::None;
        let kind = match pseudo_style.display {
            DisplayMode::Block
            | DisplayMode::FlowRoot
            | DisplayMode::ListItem
            | DisplayMode::FlowRootListItem
            | DisplayMode::BlockMath
            | DisplayMode::BlockRuby => BoxKind::Block {
                formatting_context: FormattingContextKind::Block,
            },
            DisplayMode::TableCaption => BoxKind::TableCaption,
            DisplayMode::TableHeaderGroup => BoxKind::TableRowGroup {
                section: TableSectionKind::Header,
            },
            DisplayMode::TableRowGroup => BoxKind::TableRowGroup {
                section: TableSectionKind::Body,
            },
            DisplayMode::TableFooterGroup => BoxKind::TableRowGroup {
                section: TableSectionKind::Footer,
            },
            DisplayMode::TableRow => BoxKind::TableRow,
            DisplayMode::TableCell => BoxKind::TableCell {
                colspan: 1,
                rowspan: 1,
            },
            DisplayMode::TableColumnGroup => BoxKind::TableColumnGroup { span: 1 },
            DisplayMode::TableColumn => BoxKind::TableColumn { span: 1 },
            DisplayMode::InlineBlock | DisplayMode::InlineFlowRootListItem => {
                BoxKind::InlineBlock {
                    formatting_context: FormattingContextKind::Block,
                }
            }
            DisplayMode::Flex => BoxKind::Block {
                formatting_context: FormattingContextKind::Flex,
            },
            DisplayMode::InlineFlex => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Flex,
            },
            DisplayMode::Grid | DisplayMode::GridLanes => BoxKind::Block {
                formatting_context: FormattingContextKind::Grid,
            },
            DisplayMode::InlineGrid
            | DisplayMode::InlineGridLanes
            | DisplayMode::InlineGridLanesMulti => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Grid,
            },
            DisplayMode::Table => BoxKind::Table,
            DisplayMode::InlineTable => BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Table,
            },
            _ => BoxKind::Inline,
        };
        let establishes_bfc = box_establishes_bfc(&kind, &pseudo_style, is_float, is_out_of_flow);
        Some(self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: None,
            kind,
            style: pseudo_style,
            lang: effective_lang.cloned(),
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
            "canvas" | "iframe" | "video" => {
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
                let raw_src = elem.get_attribute("src").unwrap_or_default().trim();
                let srcset_pick = elem.get_attribute("srcset").and_then(parse_best_srcset_url);
                let src = if !raw_src.is_empty() {
                    raw_src.to_string()
                } else {
                    srcset_pick.clone().unwrap_or_default()
                };
                let attr_w = elem
                    .get_attribute("width")
                    .and_then(parse_html_dimension_attr);
                let attr_h = elem
                    .get_attribute("height")
                    .and_then(parse_html_dimension_attr);

                // Pure-Rust SVG-in-`<img>` support (`.svg` and `data:image/svg+xml`).
                let svg_asset = self.assets.get_svg_by_src(&src).or_else(|| {
                    srcset_pick
                        .as_deref()
                        .and_then(|s| self.assets.get_svg_by_src(s))
                });
                if let Some(svg) = svg_asset {
                    let nat_w = svg.intrinsic_size.width.max(1.0);
                    let nat_h = svg.intrinsic_size.height.max(1.0);
                    let (iw, ih) = match (attr_w, attr_h) {
                        (Some(w), Some(h)) => (w, h),
                        (Some(w), None) => (w, (w * (nat_h / nat_w)).max(1.0)),
                        (None, Some(h)) => ((h * (nat_w / nat_h)).max(1.0), h),
                        (None, None) => (nat_w, nat_h),
                    };
                    return Some(ReplacedKind::Svg {
                        view_box: svg.view_box.or(Some(rix_dom::SvgViewBox {
                            min_x: 0.0,
                            min_y: 0.0,
                            width: nat_w,
                            height: nat_h,
                        })),
                        intrinsic_size: Size::new(iw, ih),
                        paths: svg.paths.clone(),
                    });
                }
                if let Some((nat_w, nat_h)) = parse_inline_svg_data_uri_size(&src) {
                    let (iw, ih) = match (attr_w, attr_h) {
                        (Some(w), Some(h)) => (w, h),
                        (Some(w), None) => (w, (w * (nat_h / nat_w.max(1.0))).max(1.0)),
                        (None, Some(h)) => ((h * (nat_w / nat_h.max(1.0))).max(1.0), h),
                        (None, None) => (nat_w, nat_h),
                    };
                    return Some(ReplacedKind::Image {
                        src,
                        image_id: None,
                        intrinsic_size: Size::new(iw, ih),
                    });
                }

                let decoded = self.assets.get_by_src(&src).or_else(|| {
                    srcset_pick
                        .as_deref()
                        .and_then(|s| self.assets.get_by_src(s))
                });
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
                let view_box = elem
                    .get_attribute("viewBox")
                    .or_else(|| elem.get_attribute("viewbox"))
                    .and_then(SvgViewBox::parse);
                let attr_w = elem
                    .get_attribute("width")
                    .and_then(parse_html_dimension_attr);
                let attr_h = elem
                    .get_attribute("height")
                    .and_then(parse_html_dimension_attr);
                let (iw, ih) = match (attr_w, attr_h, view_box) {
                    (Some(w), Some(h), _) => (w, h),
                    (Some(w), None, Some(vb)) if vb.width > 0.0 && vb.height > 0.0 => {
                        (w, w * (vb.height / vb.width))
                    }
                    (None, Some(h), Some(vb)) if vb.width > 0.0 && vb.height > 0.0 => {
                        (h * (vb.width / vb.height), h)
                    }
                    (w_opt, h_opt, vb_opt) => (
                        w_opt.or_else(|| vb_opt.map(|vb| vb.width)).unwrap_or(16.0),
                        h_opt.or_else(|| vb_opt.map(|vb| vb.height)).unwrap_or(16.0),
                    ),
                };
                let paths = crate::assets::extract_svg_from_node(self.doc, node, style.text.color)
                    .map(|d| d.paths)
                    .unwrap_or_default();
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
                            (value.len().max(4) as f32 * style.text.font_size_px * 0.56).max(48.0);
                        (
                            FormControlKind::Button,
                            Size::new(label_w + 14.0, (style.text.font_size_px * 1.3).max(18.0)),
                        )
                    }
                    _ => {
                        let size_chars = elem
                            .get_attribute("size")
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|v| v.is_finite() && *v > 0.0)
                            .unwrap_or(20.0);
                        (
                            FormControlKind::TextInput,
                            Size::new(
                                (size_chars * style.text.font_size_px * 0.56).max(40.0),
                                (style.text.font_size_px * 1.25).max(18.0),
                            ),
                        )
                    }
                };
                Some(ReplacedKind::FormControl {
                    kind,
                    text: value,
                    placeholder,
                    intrinsic_size: size,
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
            BoxKind::Table
            | BoxKind::AnonymousTable
            | BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Table,
            } => self.fixup_table_children(parent_style, children),
            BoxKind::TableRowGroup { .. } => self.fixup_row_group_children(parent_style, children),
            BoxKind::TableRow | BoxKind::AnonymousTableRow => {
                self.fixup_row_children(parent_style, children)
            }
            BoxKind::TableColumnGroup { .. } | BoxKind::TableColumn { .. } => children,
            BoxKind::Block {
                formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
            }
            | BoxKind::InlineBlock {
                formatting_context: FormattingContextKind::Flex | FormattingContextKind::Grid,
            } => self.fixup_flex_grid_children(parent_style, children),
            BoxKind::Block { .. }
            | BoxKind::InlineBlock { .. }
            | BoxKind::TableCaption
            | BoxKind::TableCell { .. }
            | BoxKind::AnonymousBlock
            | BoxKind::AnonymousTableCell => {
                // First wrap any stray table-internal children inside an anonymous table.
                let children = self.wrap_stray_table_internals(parent_style, children);
                self.fixup_block_children(parent_style, children)
            }
            BoxKind::Inline => self.wrap_stray_table_internals(parent_style, children),
            _ => children,
        }
    }

    /// Removes ignorable whitespace-only text boxes inside table-root, row-group, and row boxes
    /// per CSS Tables Level 3 §2.2 Rules 1.3 and 1.4.
    fn strip_table_whitespace(&self, children: Vec<BoxId>) -> Vec<BoxId> {
        let len = children.len();
        if len == 0 {
            return children;
        }
        // Rule 1.3: discard leading and trailing whitespace-only text boxes.
        let mut start = 0usize;
        while start < len && self.is_whitespace_only(children[start]) {
            start += 1;
        }
        let mut end = len;
        while end > start && self.is_whitespace_only(children[end - 1]) {
            end -= 1;
        }
        let slice = &children[start..end];
        // Rule 1.4: discard whitespace-only text boxes whose immediate previous AND next
        // siblings are both table-non-root boxes (or table-internal boxes).
        let mut out = Vec::with_capacity(slice.len());
        for (i, &cid) in slice.iter().enumerate() {
            if self.is_whitespace_only(cid) {
                let prev_is_table = i > 0 && self.tree.get(slice[i - 1]).is_table_internal();
                let next_is_table =
                    i + 1 < slice.len() && self.tree.get(slice[i + 1]).is_table_internal();
                if prev_is_table && next_is_table {
                    continue;
                }
            }
            out.push(cid);
        }
        out
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
        let children = self.strip_table_whitespace(children);
        let mut out = Vec::new();
        let mut stray_cells = Vec::new();
        for cid in children {
            if self.tree.get(cid).is_proper_table_child() {
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
            } else {
                stray_cells.push(cid);
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
        let children = self.strip_table_whitespace(children);
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
        let children = self.strip_table_whitespace(children);
        let mut out = Vec::new();
        let mut stray = Vec::new();
        for cid in children {
            match self.tree.get(cid).kind {
                BoxKind::TableCell { .. } | BoxKind::AnonymousTableCell => {
                    if !stray.is_empty() {
                        let wrapped = self
                            .wrap_stray_table_internals(parent_style, std::mem::take(&mut stray));
                        let cell_children = self.fixup_block_children(parent_style, wrapped);
                        out.push(self.alloc_anonymous(
                            BoxKind::AnonymousTableCell,
                            parent_style,
                            cell_children,
                            true,
                        ));
                    }
                    out.push(cid);
                }
                _ => stray.push(cid),
            }
        }
        if !stray.is_empty() {
            let wrapped = self.wrap_stray_table_internals(parent_style, stray);
            let cell_children = self.fixup_block_children(parent_style, wrapped);
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
        // CSS 2.1 §9.2.1.1: Split any `BoxKind::Inline` box containing block-level descendants
        // (e.g. `<a href="..."><div>Card</div></a>` or `<span><table>...</table></span>`) so
        // block/table/flex/grid boxes participate directly in the enclosing BFC instead of being
        // dropped by IFC!
        let children = self.split_inline_containing_blocks(children);

        let has_block = children.iter().any(|&cid| {
            let c = self.tree.get(cid);
            c.is_block_level() && !c.is_float
        });
        if !has_block {
            // CSS 2.1 §9.2.2.1 / §9.5.1: If a block container holds only floats, out-of-flow
            // boxes, and collapsible whitespace text nodes (no visible inline content), strip
            // whitespace text nodes so the container runs a clean BFC without laying out
            // whitespace `BoxKind::Text` nodes as styled block boxes.
            let only_floats_oof_ws = !children.is_empty()
                && children.iter().all(|&cid| {
                    let c = self.tree.get(cid);
                    c.is_float || c.is_out_of_flow || self.is_whitespace_only(cid)
                });
            if only_floats_oof_ws {
                return children
                    .into_iter()
                    .filter(|&cid| !self.is_whitespace_only(cid))
                    .collect();
            }
            return children;
        }
        let mut out = Vec::with_capacity(children.len());
        let mut inline_run = Vec::new();
        for cid in children {
            let child = self.tree.get(cid);
            if (child.is_block_level() && !child.is_float) || child.is_out_of_flow {
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
            } else if child.is_float {
                // Keep a float inside an active inline run only when visible inline content
                // precedes it on the same line; otherwise emit it directly as a BFC float.
                let has_visible = inline_run.iter().any(|&id| !self.is_whitespace_only(id));
                if has_visible {
                    inline_run.push(cid);
                } else {
                    inline_run.clear();
                    out.push(cid);
                }
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

    fn inline_contains_block_descendant(&self, id: BoxId) -> bool {
        let b = self.tree.get(id);
        if b.is_block_level() {
            return true;
        }
        if !matches!(b.kind, BoxKind::Inline) {
            return false;
        }
        b.children
            .iter()
            .any(|&cid| self.inline_contains_block_descendant(cid))
    }

    fn split_inline_containing_blocks(&mut self, children: Vec<BoxId>) -> Vec<BoxId> {
        let needs_split = children.iter().any(|&cid| {
            matches!(self.tree.get(cid).kind, BoxKind::Inline)
                && self.inline_contains_block_descendant(cid)
        });
        if !needs_split {
            return children;
        }
        let mut out = Vec::with_capacity(children.len() + 2);
        for cid in children {
            if matches!(self.tree.get(cid).kind, BoxKind::Inline)
                && self.inline_contains_block_descendant(cid)
            {
                let (dom_node, style, lang, subtree_dirty, raw_kids) = {
                    let b = self.tree.get(cid);
                    (
                        b.dom_node,
                        Arc::clone(&b.style),
                        b.lang.clone(),
                        b.subtree_dirty,
                        b.children.clone(),
                    )
                };
                let flattened = self.split_inline_containing_blocks(raw_kids);
                let mut inline_seg = Vec::new();
                for sub in flattened {
                    let is_blk = {
                        let sb = self.tree.get(sub);
                        sb.is_block_level() || sb.is_out_of_flow || sb.is_float
                    };
                    if is_blk {
                        if !inline_seg.is_empty() {
                            let seg = std::mem::take(&mut inline_seg);
                            out.push(self.alloc_box(LayoutBox {
                                id: BoxId(0),
                                dom_node,
                                kind: BoxKind::Inline,
                                style: Arc::clone(&style),
                                lang: lang.clone(),
                                children: seg,
                                is_out_of_flow: false,
                                is_float: false,
                                establishes_bfc: false,
                                subtree_dirty,
                                subtree_hash: 0,
                            }));
                        }
                        out.push(sub);
                    } else {
                        inline_seg.push(sub);
                    }
                }
                if !inline_seg.is_empty() {
                    out.push(self.alloc_box(LayoutBox {
                        id: BoxId(0),
                        dom_node,
                        kind: BoxKind::Inline,
                        style,
                        lang,
                        children: inline_seg,
                        is_out_of_flow: false,
                        is_float: false,
                        establishes_bfc: false,
                        subtree_dirty,
                        subtree_hash: 0,
                    }));
                }
            } else {
                out.push(cid);
            }
        }
        out
    }

    fn is_whitespace_only(&self, id: BoxId) -> bool {
        matches!(
            &self.tree.get(id).kind,
            BoxKind::Text(t)
                if t.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C))
        )
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
        let lang = children
            .first()
            .and_then(|&c| self.tree.get(c).lang.clone());
        self.alloc_box(LayoutBox {
            id: BoxId(0),
            dom_node: None,
            kind,
            style: Arc::new(anon_style),
            lang,
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
            BoxKind::TableColumn { span } | BoxKind::TableColumnGroup { span } => {
                hash = fnv_mix(hash, (*span as u64) ^ 0x5555_0000);
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

fn parse_html_non_negative_int(raw: &str) -> Option<usize> {
    let s = raw.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let s = s.strip_prefix('+').unwrap_or(s);
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    s[..end].parse::<usize>().ok()
}

fn classify_element_box_kind(
    tag: &str,
    elem: &ElementData,
    style: &ComputedStyle,
    is_float: bool,
    is_out_of_flow: bool,
) -> BoxKind {
    // Floated or out-of-flow boxes blockify per CSS Display Level 3 §2.7.
    let effective_display = if is_float || is_out_of_flow {
        style.display.blockify()
    } else {
        style.display
    };

    match effective_display {
        DisplayMode::Flex | DisplayMode::RunInFlex => BoxKind::Block {
            formatting_context: FormattingContextKind::Flex,
        },
        DisplayMode::InlineFlex => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Flex,
        },
        DisplayMode::Grid | DisplayMode::RunInGrid | DisplayMode::GridLanes => BoxKind::Block {
            formatting_context: FormattingContextKind::Grid,
        },
        DisplayMode::InlineGrid
        | DisplayMode::InlineGridLanes
        | DisplayMode::InlineGridLanesMulti => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Grid,
        },
        DisplayMode::Table | DisplayMode::RunInTable => BoxKind::Table,
        DisplayMode::InlineTable => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Table,
        },
        DisplayMode::TableCaption => BoxKind::TableCaption,
        DisplayMode::TableColumnGroup => {
            let span = if tag.eq_ignore_ascii_case("colgroup") {
                elem.get_attribute("span")
                    .and_then(parse_html_non_negative_int)
                    .filter(|&v| v > 0)
                    .unwrap_or(1)
                    .min(1000)
            } else {
                1
            };
            BoxKind::TableColumnGroup { span }
        }
        DisplayMode::TableColumn => {
            let span = if tag.eq_ignore_ascii_case("col") {
                elem.get_attribute("span")
                    .and_then(parse_html_non_negative_int)
                    .filter(|&v| v > 0)
                    .unwrap_or(1)
                    .min(1000)
            } else {
                1
            };
            BoxKind::TableColumn { span }
        }
        DisplayMode::TableHeaderGroup => BoxKind::TableRowGroup {
            section: TableSectionKind::Header,
        },
        DisplayMode::TableRowGroup => BoxKind::TableRowGroup {
            section: TableSectionKind::Body,
        },
        DisplayMode::TableFooterGroup => BoxKind::TableRowGroup {
            section: TableSectionKind::Footer,
        },
        DisplayMode::TableRow => BoxKind::TableRow,
        DisplayMode::TableCell => {
            let is_html_cell = tag.eq_ignore_ascii_case("td") || tag.eq_ignore_ascii_case("th");
            let colspan = if is_html_cell {
                elem.get_attribute("colspan")
                    .and_then(parse_html_non_negative_int)
                    .filter(|&v| v > 0)
                    .unwrap_or(1)
                    .min(1000)
            } else {
                1
            };
            // HTML Standard §4.9.11: `rowspan="0"` means span all remaining rows in the row group.
            let rowspan = if is_html_cell {
                elem.get_attribute("rowspan")
                    .and_then(parse_html_non_negative_int)
                    .unwrap_or(1)
                    .min(65_534)
            } else {
                1
            };
            BoxKind::TableCell { colspan, rowspan }
        }
        DisplayMode::InlineBlock | DisplayMode::InlineFlowRootListItem => BoxKind::InlineBlock {
            formatting_context: FormattingContextKind::Block,
        },
        DisplayMode::Inline
        | DisplayMode::InlineListItem
        | DisplayMode::Math
        | DisplayMode::Ruby
        | DisplayMode::RubyBase
        | DisplayMode::RubyText
        | DisplayMode::RubyBaseContainer
        | DisplayMode::RubyTextContainer
        | DisplayMode::RunIn
        | DisplayMode::RunInListItem
        | DisplayMode::RunInRuby => BoxKind::Inline,
        DisplayMode::Block
        | DisplayMode::FlowRoot
        | DisplayMode::ListItem
        | DisplayMode::FlowRootListItem
        | DisplayMode::RunInFlowRoot
        | DisplayMode::RunInFlowRootListItem
        | DisplayMode::BlockMath
        | DisplayMode::BlockRuby
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
    if matches!(
        style.display,
        DisplayMode::FlowRoot | DisplayMode::FlowRootListItem | DisplayMode::TableCaption
    ) {
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

/// Parses intrinsic `(width, height)` from an inline `data:image/svg+xml,...` URL when
/// not pre-loaded in `AssetRegistry` (e.g. dynamically assigned via `img.src = ...`).
fn parse_inline_svg_data_uri_size(src: &str) -> Option<(f32, f32)> {
    let lower = src.to_ascii_lowercase();
    if !lower.starts_with("data:image/svg+xml") {
        return None;
    }
    let comma = src.find(',')?;
    let meta = &lower[..comma];
    let payload = &src[comma + 1..];
    if meta.contains(";base64") {
        return None;
    }
    // Percent-decode payload into UTF-8 string.
    let mut bytes = Vec::with_capacity(payload.len());
    let pb = payload.as_bytes();
    let mut i = 0;
    while i < pb.len() {
        if pb[i] == b'%' && i + 2 < pb.len() {
            if let Ok(hex) = std::str::from_utf8(&pb[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    bytes.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        bytes.push(pb[i]);
        i += 1;
    }
    let svg = String::from_utf8(bytes).ok()?;
    let tag_start = svg.to_ascii_lowercase().find("<svg")?;
    let tag_end = svg[tag_start..].find('>')? + tag_start;
    let tag = &svg[tag_start..tag_end];
    let extract_attr = |name: &str| -> Option<String> {
        let low = tag.to_ascii_lowercase();
        let needle = format!("{name}=");
        let pos = low.find(&needle)? + needle.len();
        let rest = tag[pos..].trim_start();
        let quote = rest.bytes().next()?;
        if quote == b'"' || quote == b'\'' {
            let inner = &rest[1..];
            let end = inner.find(quote as char)?;
            Some(inner[..end].to_string())
        } else {
            None
        }
    };
    let w = extract_attr("width").and_then(|s| parse_html_dimension_attr(&s));
    let h = extract_attr("height").and_then(|s| parse_html_dimension_attr(&s));
    if let (Some(wv), Some(hv)) = (w, h) {
        return Some((wv, hv));
    }
    if let Some(vb) = extract_attr("viewbox").and_then(|s| SvgViewBox::parse(&s)) {
        return Some((w.unwrap_or(vb.width), h.unwrap_or(vb.height)));
    }
    None
}
