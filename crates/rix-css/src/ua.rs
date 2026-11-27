//! Built-in HTML5 User-Agent default stylesheet compiled once via `OnceLock`.

use std::sync::{Arc, OnceLock};

use crate::stylesheet::{CascadeOrigin, MediaContext, Stylesheet};

/// Standard HTML5 User-Agent stylesheet rules.
pub const UA_CSS: &str = r#"
head, title, meta, link, style, script, template, noscript, [hidden],
[popover]:not(:popover-open):not(dialog[open]), dialog:not([open]):not(:popover-open),
details:not([open]) > :not(summary) {
    display: none;
}
html, body, div, p, section, article, main, header, footer, nav, aside,
figure, figcaption, blockquote, pre, form, fieldset, legend, hr,
ul, ol, dl, dt, dd, h1, h2, h3, h4, h5, h6, address,
center, details, summary, dialog, hgroup, search, menu, dir {
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
    text-align: match-parent;
}
hr {
    color: gray;
    border-style: inset;
    border-width: 1px;
    margin-top: 0.5em;
    margin-bottom: 0.5em;
    margin-left: auto;
    margin-right: auto;
    overflow: hidden;
}
fieldset {
    border: 2px groove #e9e9ed;
    padding: 0.35em 0.75em 0.625em;
    margin-left: 2px;
    margin-right: 2px;
}
legend {
    padding-left: 2px;
    padding-right: 2px;
}
summary {
    display: list-item;
    list-style-type: disclosure-closed;
}
details[open] > summary {
    list-style-type: disclosure-open;
}
dialog {
    position: absolute;
    left: 0;
    right: 0;
    margin: auto;
    padding: 1em;
    border: 3px solid CanvasText;
    background-color: Canvas;
    color: CanvasText;
}
dialog:modal {
    position: fixed;
    top: 0;
    bottom: 0;
    max-width: calc(100% - 2em - 6px);
    max-height: calc(100% - 2em - 6px);
    overflow: auto;
    visibility: visible;
}
dialog::backdrop {
    background-color: rgba(0, 0, 0, 0.1);
}
[popover] {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    left: 0;
    width: fit-content;
    height: fit-content;
    margin: auto;
    border: 2px solid;
    padding: 0.25em;
    overflow: auto;
    color: CanvasText;
    background-color: Canvas;
}
table {
    display: table;
    border-collapse: separate;
    box-sizing: border-box;
    text-indent: initial;
    border-spacing: 2px;
}
thead {
    display: table-header-group;
    vertical-align: middle;
    border-color: inherit;
}
tbody {
    display: table-row-group;
    vertical-align: middle;
    border-color: inherit;
}
tfoot {
    display: table-footer-group;
    vertical-align: middle;
    border-color: inherit;
}
colgroup {
    display: table-column-group;
}
col {
    display: table-column;
}
caption {
    display: table-caption;
    text-align: center;
}
tr {
    display: table-row;
    vertical-align: inherit;
    border-color: inherit;
}
td, th {
    display: table-cell;
    vertical-align: inherit;
    padding: 1px;
}
th {
    text-align: center;
}
* {
    overlay: none !important;
}
:popover-open, dialog:modal {
    overlay: auto !important;
}
::backdrop {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    left: 0;
}
html {
    color: CanvasText;
    font-family: serif;
}
mark {
    background-color: Mark;
    color: MarkText;
}
body {
    margin: 8px;
}
p, ul, ol, dl, blockquote, pre, figure {
    margin-top: 1em;
    margin-bottom: 1em;
}
blockquote, figure {
    margin-left: 40px;
    margin-right: 40px;
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
b, strong {
    font-weight: bolder;
}
th {
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
ul ul, ul ol, ol ul, ol ol {
    margin-top: 0;
    margin-bottom: 0;
}
ul {
    list-style-type: disc;
}
ol {
    list-style-type: decimal;
}
a:link {
    color: LinkText;
    text-decoration: underline;
    cursor: pointer;
}
a:visited {
    color: VisitedText;
    text-decoration: underline;
    cursor: pointer;
}
input[type="hidden" i] {
    display: none;
}
label {
    cursor: default;
}
button, input, select, textarea {
    display: inline-block;
    font-family: sans-serif;
    font-size: 13.3333px;
    line-height: normal;
    letter-spacing: normal;
    word-spacing: normal;
    color: FieldText;
    border-radius: 0;
    appearance: auto;
}
button {
    box-sizing: border-box;
    background-color: ButtonFace;
    color: ButtonText;
    border: 2px outset ButtonBorder;
    padding: 1px 4px;
    text-align: center;
    overflow: visible;
    cursor: default;
}
input {
    box-sizing: content-box;
    background-color: Field;
    border: 2px inset ButtonBorder;
    padding: 1px 2px;
    text-align: start;
    overflow: clip;
    cursor: text;
}
input[type="search" i] {
    box-sizing: border-box;
}
input[type="submit" i], input[type="button" i], input[type="reset" i] {
    box-sizing: border-box;
    background-color: ButtonFace;
    color: ButtonText;
    border: 2px outset ButtonBorder;
    padding: 1px 4px;
    text-align: center;
    white-space: pre;
    cursor: default;
}
input[type="checkbox" i] {
    box-sizing: border-box;
    margin: 3px 3px 3px 4px;
    padding: 0;
    border: 0;
    background-color: transparent;
    overflow: visible;
    cursor: default;
}
input[type="radio" i] {
    box-sizing: border-box;
    margin: 3px 3px 0px 5px;
    padding: 0;
    border: 0;
    background-color: transparent;
    overflow: visible;
    cursor: default;
}
input[type="range" i] {
    box-sizing: content-box;
    margin: 2px;
    padding: 0;
    border: 0;
    background-color: Field;
    overflow: visible;
    cursor: default;
}
input[type="color" i] {
    box-sizing: border-box;
    background-color: ButtonFace;
    color: ButtonText;
    border: 2px outset ButtonBorder;
    padding: 4px;
    cursor: default;
}
input[type="file" i] {
    box-sizing: content-box;
    background-color: transparent;
    border: 0;
    padding: 0;
    white-space: nowrap;
    cursor: default;
    appearance: none;
}
input[type="date" i], input[type="time" i], input[type="datetime-local" i], input[type="month" i], input[type="week" i] {
    font-family: monospace;
    font-size: 13.3333px;
    cursor: default;
}
select {
    box-sizing: border-box;
    background-color: ButtonFace;
    border: 2px inset ButtonBorder;
    padding: 1px 4px;
    overflow: clip;
    white-space: nowrap;
    cursor: default;
}
select[multiple] {
    background-color: Field;
    padding: 1px 0;
    vertical-align: text-bottom;
    overflow-x: hidden;
    overflow-y: scroll;
}
optgroup {
    display: block;
    font-weight: 700;
    font-style: italic;
}
option {
    display: block;
    padding: 2px 0 2px 20px;
}
textarea {
    box-sizing: content-box;
    font-family: monospace;
    font-size: 12px;
    background-color: Field;
    border: 2px inset ButtonBorder;
    padding: 2px;
    vertical-align: text-bottom;
    white-space: pre-wrap;
    overflow: visible;
    cursor: text;
}
meter {
    display: inline-block;
    box-sizing: content-box;
    vertical-align: -0.2em;
    overflow: clip;
    appearance: auto;
}
progress {
    display: inline-block;
    box-sizing: content-box;
    background-color: #e6e6e6;
    border: 1px solid #817f84;
    vertical-align: -0.2em;
    overflow: clip;
    appearance: auto;
}
img, svg, canvas, video {
    display: inline;
}
img, video, canvas {
    overflow: clip;
}
svg:not(:root) {
    overflow: hidden;
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
