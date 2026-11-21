#![deny(unsafe_code)]
//! CSS Box Tree, Fragment Tree, BFC/Floats, and IFC layout engine for RixBrowser.

pub mod assets;
pub mod bfc;
pub mod box_tree;
pub mod fragment;
pub mod ifc;

pub use assets::{
    parse_html_dimension_attr, parse_svg_color, AssetError, AssetRegistry, DecodedImage,
};
pub use bfc::{collapse_margins, layout_bfc_root, layout_block_box, FloatBand, FloatContext};
pub use box_tree::{
    BoxId, BoxKind, BoxTree, FormControlKind, FormattingContextKind, LayoutBox, ReplacedKind,
    SvgPathEntry,
};
pub use fragment::{
    layout_box, layout_replaced, layout_tree, measure_intrinsic_widths, BoxMetrics,
    ConstraintSpace, EdgeInsets, Fragment, FragmentKind, FragmentTree, LayoutCache, LayoutContext,
    PositionedGlyph, ShapedTextFragment,
};
pub use ifc::{
    layout_ifc, measure_ifc_min_max_content, resolve_font_for_style, resolve_line_height_px,
};
