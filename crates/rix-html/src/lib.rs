#![deny(unsafe_code)]

pub mod parser;
pub mod preload;
pub mod sink;

pub use markup5ever::QualName;
pub use parser::{
    attach_declarative_shadow_roots, decode_html_bytes, detect_html_charset,
    is_blocking_classic_script, parse_html, parse_html_fragment, parse_html_fragment_with_options,
    parse_html_fragment_with_scripting, parse_html_with_options, parse_html_with_scripting,
    HtmlParserStream, ParserStreamStep,
};
pub use preload::{PreloadHint, PreloadKind, PreloadPriority, PreloadScanner};
pub use sink::DomTreeSink;
