#![deny(unsafe_code)]
//! CSS Box Tree construction and decoded image/SVG asset registry for RixBrowser.

pub mod assets;
pub mod box_tree;

pub use assets::{
    parse_html_dimension_attr, parse_svg_color, AssetError, AssetRegistry, DecodedImage,
};
pub use box_tree::{
    BoxId, BoxKind, BoxTree, FormControlKind, FormattingContextKind, LayoutBox, ReplacedKind,
    SvgPathEntry,
};
