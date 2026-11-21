#![deny(unsafe_code)]

pub mod parser;
pub mod preload;
pub mod sink;

pub use markup5ever::QualName;
pub use parser::{
    attach_declarative_shadow_roots, is_blocking_classic_script, parse_html, parse_html_fragment,
    HtmlParserStream, ParserStreamStep,
};
pub use preload::{PreloadHint, PreloadKind, PreloadPriority, PreloadScanner};
pub use sink::DomTreeSink;
