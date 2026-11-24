#![deny(unsafe_code)]
//! CSS Box Tree, Fragment Tree, BFC/Floats, IFC, Table, retained Taffy
//! Flex/Grid, and CSS 2.1 Appendix E DisplayList painter for RixBrowser.

pub mod assets;
pub mod bfc;
pub mod box_tree;
pub mod engine;
pub mod flex;
pub mod flex_grid;
pub mod fragment;
pub mod grid;
pub mod grid_lanes;
pub mod grid_tracks;
pub mod ifc;
pub mod paint;
pub mod position;
pub mod table;

pub use assets::{
    collect_svg_paths_from_dom, decode_raster_batch_parallel, decode_single_raster,
    extract_svg_from_document, parse_best_srcset_url, parse_html_dimension_attr, parse_svg_color,
    AssetError, AssetRegistry, DecodedImage, DecodedRasterBytes, DecodedSvg,
};
pub use bfc::{collapse_margins, layout_bfc_root, layout_block_box, FloatBand, FloatContext};
pub use box_tree::{
    surface_id_for_node, BoxId, BoxKind, BoxTree, FormControlKind, FormattingContextKind,
    LayoutBox, ReplacedKind, SvgPathEntry,
};
pub use engine::{hit_test, LayoutEngine, LayoutFrame, LayoutMetrics};
pub use flex::{layout_flex_container, measure_flex_intrinsic_widths, FlexAbsposHint, FlexAxes};
pub use flex_grid::layout_flex_grid_container;
pub use fragment::{
    layout_box, layout_replaced, layout_tree, layout_tree_with_scroll, measure_intrinsic_widths,
    BoxMetrics, ConstraintSpace, EdgeInsets, Fragment, FragmentKind, FragmentTree, LayoutCache,
    LayoutContext, PositionedGlyph, ResolvedGridTracks, ShapedTextFragment,
};
pub use grid::{layout_grid_container, measure_grid_intrinsic_widths, GridAbsposHint, GridAxes};
pub use ifc::{
    layout_ifc, measure_ifc_min_max_content, resolve_font_for_style, resolve_line_height_px,
};
pub use paint::{paint_fragment_tree, paint_fragment_tree_with_keyframes};
pub use position::{
    apply_sticky_offsets, establishes_abs_cb, establishes_fixed_cb, is_scroll_axis,
    layout_out_of_flow_box, offset_pending_oof, resolve_oof_for_container, ContainingBlockInfo,
    PendingOof,
};
pub use table::{layout_table, measure_table_intrinsic_widths};
