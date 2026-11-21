#![deny(unsafe_code)]

pub mod font_loader;
pub mod font_match;
pub mod fontconfig;
pub mod layout;
pub mod shaping;
pub mod web_font;

pub use font_loader::{
    unicode_coverage_block, FontData, FontDatabase, FontDescriptor, FontId, FontMetrics, FontStyle,
    FontWeight,
};
pub use layout::{
    InlineObjectMetrics, LayoutLine, ParagraphItem, ParagraphLayout, ParagraphOptions,
    PositionedInlineObject, SpaceBreakBehavior, TextAlign, TextSourceRange, TextSpan,
    TrailingSpaceBehavior,
};
pub use shaping::{
    contains_rtl_runs, shared_shaping_cache, GlyphPosition, ShapedRun, TextShaper, TextShapingCache,
};
pub use web_font::{
    decode_web_font, detect_web_font_format, parse_unicode_range, FontFaceSet, UnicodeRange,
    WebFontError, WebFontFace, WebFontFormat,
};
