//! Built-in HTML5 User-Agent default stylesheet compiled once via `OnceLock`.

use std::sync::{Arc, OnceLock};

use crate::stylesheet::{CascadeOrigin, MediaContext, Stylesheet};

/// Standard HTML5 User-Agent stylesheet rules.
pub const UA_CSS: &str = r#"
head, title, meta, link, style, script, template, noscript, [hidden] {
    display: none;
}
html, body, div, p, section, article, main, header, footer, nav, aside,
figure, figcaption, blockquote, pre, form, fieldset, legend, hr,
ul, ol, dl, dt, dd, h1, h2, h3, h4, h5, h6, address,
center, details, summary, dialog, hgroup, search, menu, dir, caption {
    display: block;
}
center {
    text-align: center;
}
center > table {
    margin-left: auto;
    margin-right: auto;
}
li {
    display: list-item;
}
table {
    display: table;
    border-collapse: separate;
    box-sizing: border-box;
    text-align: left;
}
tr {
    display: table-row;
}
td, th {
    display: table-cell;
}
body {
    margin: 8px;
}
p, ul, ol, dl, blockquote, pre, figure {
    margin-top: 1em;
    margin-bottom: 1em;
}
h1 {
    font-size: 2em;
    font-weight: 700;
    margin-top: 0.67em;
    margin-bottom: 0.67em;
}
h2 {
    font-size: 1.5em;
    font-weight: 700;
    margin-top: 0.83em;
    margin-bottom: 0.83em;
}
h3 {
    font-size: 1.17em;
    font-weight: 700;
    margin-top: 1em;
    margin-bottom: 1em;
}
h4 {
    font-weight: 700;
    margin-top: 1.33em;
    margin-bottom: 1.33em;
}
h5 {
    font-size: 0.83em;
    font-weight: 700;
    margin-top: 1.67em;
    margin-bottom: 1.67em;
}
h6 {
    font-size: 0.67em;
    font-weight: 700;
    margin-top: 2.33em;
    margin-bottom: 2.33em;
}
b, strong, th {
    font-weight: 700;
}
i, em, cite, dfn, var, address {
    font-style: italic;
}
code, kbd, samp, pre {
    font-family: monospace;
}
pre {
    white-space: pre;
}
ul, ol {
    padding-left: 40px;
}
a:link {
    color: #0000ee;
    text-decoration: underline;
    cursor: pointer;
}
a:visited {
    color: #551a8b;
    text-decoration: underline;
    cursor: pointer;
}
input[type="hidden"] {
    display: none;
}
button, input, select, textarea {
    display: inline-block;
    box-sizing: border-box;
    border: 1px solid #a0a6b2;
    border-radius: 4px;
}
input, textarea, select {
    background-color: #ffffff;
}
button, input[type="submit"], input[type="button"], input[type="reset"] {
    background-color: #eceef2;
}
img, svg, canvas, video {
    display: inline-block;
}
[dir="rtl" i] {
    direction: rtl;
}
[dir="ltr" i] {
    direction: ltr;
}
[dir="auto" i]:dir(rtl) {
    direction: rtl;
}
[dir="auto" i]:dir(ltr) {
    direction: ltr;
}
input[type="tel" i]:dir(ltr) {
    direction: ltr;
}
"#;

/// Returns the cached compiled HTML5 User-Agent [`Stylesheet`].
#[must_use]
pub fn default_ua_stylesheet() -> Arc<Stylesheet> {
    static UA_SHEET: OnceLock<Arc<Stylesheet>> = OnceLock::new();
    Arc::clone(UA_SHEET.get_or_init(|| {
        Arc::new(Stylesheet::parse(
            UA_CSS,
            CascadeOrigin::UserAgent,
            &MediaContext::default(),
        ))
    }))
}
