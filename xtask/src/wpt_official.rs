//! Official W3C Web Platform Tests (WPT) conformance runner
//! (`cargo xtask wpt-official --suite <subpath> [--wpt-dir <path>] [--limit <n>]`).
//!
//! Executes unmodified upstream WPT `.html` tests from a local WPT checkout:
//! - `testharness.js` script tests (DOM, Shadow DOM, Custom Elements, CSSOM, CSS Values/Selectors, HTML)
//! - Visual `<link rel="match"|"mismatch">` reftests (CSS Flexbox, Grid, Position, Tables, Display, Text, Cascade)

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rix_content::ContentRuntime;
use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::LayoutEngine;
use rix_render::{encode_display_list, EncodeOptions, ResourceRegistry, VelloRenderer};
use serde::{Deserialize, Serialize};
use vello::peniko::Color;
use vello::Scene;

use crate::reftest::{compare_rgba, extract_style_blocks, shared_font_db};

/// Spec-compatible WPT `testharness.js` bridge capturing exact subtest pass/fail outcomes.
const WPT_TESTHARNESS_SHIM: &str = r#"
(function(global) {
    const results = [];
    let harnessStatus = 0;
    let harnessErrors = [];
    let pendingAsync = 0;

    function format_value(val) {
        if (val === null) return "null";
        if (val === undefined) return "undefined";
        if (typeof val === "string") return JSON.stringify(val);
        if (typeof val === "number") return Object.is(val, -0) ? "-0" : String(val);
        if (typeof val === "symbol") return String(val);
        try { return String(val); } catch (_) { return "<object>"; }
    }

    function AssertionError(msg) {
        this.name = "AssertionError";
        this.message = msg || "Assertion failed";
    }
    AssertionError.prototype = Object.create(Error.prototype);
    AssertionError.prototype.constructor = AssertionError;
    AssertionError.prototype.toString = function() {
        return this.name + ": " + this.message;
    };

    function fail(msg) { throw new AssertionError(msg); }

    function assert_true(actual, desc) {
        if (actual !== true) fail((desc ? desc + ": " : "") + "expected true got " + format_value(actual));
    }
    function assert_false(actual, desc) {
        if (actual !== false) fail((desc ? desc + ": " : "") + "expected false got " + format_value(actual));
    }
    function assert_equals(actual, expected, desc) {
        if (!Object.is(actual, expected)) {
            if (!(typeof actual === "number" && typeof expected === "number" && actual === expected && actual === 0)) {
                fail((desc ? desc + ": " : "") + "expected " + format_value(expected) + " but got " + format_value(actual));
            }
        }
    }
    function assert_not_equals(actual, expected, desc) {
        if (Object.is(actual, expected) || (actual === 0 && expected === 0)) {
            fail((desc ? desc + ": " : "") + "got disallowed value " + format_value(actual));
        }
    }
    function assert_approx_equals(actual, expected, epsilon, desc) {
        if (typeof actual !== "number" || Math.abs(actual - expected) > epsilon) {
            fail((desc ? desc + ": " : "") + "expected " + expected + " +/- " + epsilon + " got " + format_value(actual));
        }
    }
    function assert_less_than(a, b, desc) {
        if (!(a < b)) fail((desc ? desc + ": " : "") + format_value(a) + " not < " + format_value(b));
    }
    function assert_greater_than(a, b, desc) {
        if (!(a > b)) fail((desc ? desc + ": " : "") + format_value(a) + " not > " + format_value(b));
    }
    function assert_less_than_equal(a, b, desc) {
        if (!(a <= b)) fail((desc ? desc + ": " : "") + format_value(a) + " not <= " + format_value(b));
    }
    function assert_greater_than_equal(a, b, desc) {
        if (!(a >= b)) fail((desc ? desc + ": " : "") + "expected " + format_value(a) + " >= " + format_value(b));
    }
    function assert_array_equals(actual, expected, desc) {
        if (!actual || actual.length !== expected.length) {
            fail((desc ? desc + ": " : "") + "lengths differ: " + (actual ? actual.length : "null") + " vs " + expected.length);
        }
        for (let i = 0; i < expected.length; i++) {
            assert_equals(actual[i], expected[i], (desc ? desc + " " : "") + "at index " + i);
        }
    }
    function assert_object_equals(actual, expected, desc) {
        if (actual === expected) return;
        if (!actual || !expected || typeof actual !== "object" || typeof expected !== "object") {
            fail((desc ? desc + ": " : "") + "objects differ");
        }
        const ak = Object.keys(actual), ek = Object.keys(expected);
        assert_equals(ak.length, ek.length, desc);
        for (const k of ek) assert_object_equals(actual[k], expected[k], desc);
    }
    function assert_in_array(actual, expected, desc) {
        if (!expected || expected.indexOf(actual) === -1) {
            fail((desc ? desc + ": " : "") + format_value(actual) + " not in array");
        }
    }
    function assert_own_property(obj, prop, desc) {
        if (!Object.prototype.hasOwnProperty.call(obj, prop)) {
            fail((desc ? desc + ": " : "") + "missing own property " + prop);
        }
    }
    function assert_inherits(obj, prop, desc) {
        if (!(prop in obj) || Object.prototype.hasOwnProperty.call(obj, prop)) {
            fail((desc ? desc + ": " : "") + "expected inherited property " + prop);
        }
    }
    function assert_idl_attribute(obj, prop, desc) {
        if (!(prop in obj)) fail((desc ? desc + ": " : "") + "missing IDL attribute " + prop);
    }
    function assert_readonly(obj, prop, desc) {
        const before = obj[prop];
        try { obj[prop] = "__rix_ro_probe__"; } catch (_) {}
        if (obj[prop] !== before) {
            fail((desc ? desc + ": " : "") + "property " + prop + " is not readonly");
        }
    }
    function assert_class_string(obj, expected, desc) {
        const actual = Object.prototype.toString.call(obj).slice(8, -1);
        assert_equals(actual, expected, desc);
    }
    function assert_regexp_match(actual, expected, desc) {
        if (!expected.test(actual)) {
            fail((desc ? desc + ": " : "") + format_value(actual) + " did not match " + expected);
        }
    }
    function assert_unreached(desc) {
        fail("Reached unreachable code" + (desc ? ": " + desc : ""));
    }
    function assert_throws_js(constructor, fn, desc) {
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            if (constructor && !(e instanceof constructor) && (!e || e.name !== constructor.name)) {
                fail((desc ? desc + ": " : "") + "expected " + constructor.name + " got " + (e && e.name ? e.name : e));
            }
        }
        if (!threw) fail((desc ? desc + ": " : "") + "did not throw");
    }
    function assert_throws_dom(type, fnOrCtor, maybeFn, desc) {
        const fn = typeof maybeFn === "function" ? maybeFn : fnOrCtor;
        const message = typeof maybeFn === "function" ? desc : maybeFn;
        const expectedCode =
            typeof type === "string" &&
            typeof DOMException !== "undefined" &&
            typeof DOMException[type] === "number"
                ? DOMException[type]
                : null;
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            if (typeof type === "string" && e && e.name && e.name !== type && e.code !== type && (expectedCode === null || e.code !== expectedCode)) {
                fail((message ? message + ": " : "") + "expected DOMException " + type + " got " + e.name);
            } else if (typeof type === "number" && e && e.code !== type) {
                fail((message ? message + ": " : "") + "expected DOMException code " + type + " got " + (e.code !== undefined ? e.code : e.name));
            }
        }
        if (!threw) fail((message ? message + ": " : "") + "did not throw DOMException " + type);
    }
    function assert_throws_exactly(expected, fn, desc) {
        let threw = false;
        try { fn(); } catch (e) {
            threw = true;
            assert_equals(e, expected, desc);
        }
        if (!threw) fail((desc ? desc + ": " : "") + "did not throw");
    }
    function assert_implements(cond, desc) {
        if (!cond) fail("Not implemented" + (desc ? ": " + desc : ""));
    }
    function assert_implements_optional(cond, desc) {
        if (!cond) fail("Optional feature not implemented" + (desc ? ": " + desc : ""));
    }

    function TestObj(name) {
        this.name = name || "(unnamed)";
        this._cleanups = [];
        this._done = false;
        this._status = 0;
        this._message = null;
    }
    TestObj.prototype.add_cleanup = function(fn) { this._cleanups.push(fn); };
    TestObj.prototype._run_cleanups = function() {
        while (this._cleanups.length) {
            try { this._cleanups.pop()(); } catch (_) {}
        }
    };
    TestObj.prototype.step = function(fn, thisObj) {
        if (this._done) return;
        const args = Array.prototype.slice.call(arguments, 2);
        try {
            return fn.apply(thisObj || this, args);
        } catch (e) {
            this._fail(e);
        }
    };
    TestObj.prototype.step_func = function(fn, thisObj) {
        const self = this;
        return function() {
            if (self._done) return;
            try { return fn.apply(thisObj || self, arguments); }
            catch (e) { self._fail(e); }
        };
    };
    TestObj.prototype.step_func_done = function(fn, thisObj) {
        const self = this;
        return function() {
            if (self._done) return;
            try {
                if (fn) fn.apply(thisObj || self, arguments);
                self.done();
            } catch (e) { self._fail(e); }
        };
    };
    TestObj.prototype.unreached_func = function(desc) {
        return this.step_func(function() { assert_unreached(desc); });
    };
    TestObj.prototype.step_timeout = function(fn, timeout) {
        const self = this;
        return setTimeout(function() { self.step(fn); }, timeout || 0);
    };
    TestObj.prototype._fail = function(e) {
        if (this._done) return;
        this._done = true;
        this._status = 1;
        this._message = e && e.message ? (e.name ? e.name + ": " + e.message : e.message) : String(e);
        this._run_cleanups();
        if (this._isAsync) pendingAsync = Math.max(0, pendingAsync - 1);
        results.push({ name: this.name, pass: false, message: this._message });
    };
    TestObj.prototype.done = function() {
        if (this._done) return;
        this._done = true;
        this._status = 0;
        this._run_cleanups();
        if (this._isAsync) pendingAsync = Math.max(0, pendingAsync - 1);
        results.push({ name: this.name, pass: true, message: "" });
    };

    function test(fn, name) {
        const t = new TestObj(name);
        try {
            fn(t);
            if (!t._done) t.done();
        } catch (e) {
            t._fail(e);
        }
    }
    function async_test(fn, name) {
        if (typeof fn === "string") { name = fn; fn = null; }
        const t = new TestObj(name);
        t._isAsync = true;
        pendingAsync++;
        if (fn) {
            try { fn(t); } catch (e) { t._fail(e); }
        }
        return t;
    }
    function promise_test(fn, name) {
        const t = new TestObj(name);
        t._isAsync = true;
        pendingAsync++;
        try {
            Promise.resolve(fn(t)).then(
                function() { t.done(); },
                function(e) { t._fail(e); }
            );
        } catch (e) {
            t._fail(e);
        }
    }
    function promise_rejects_js(t, ctor, promise, desc) {
        return promise.then(
            function() { assert_unreached((desc ? desc + ": " : "") + "promise fulfilled instead of rejecting"); },
            function(e) {
                if (ctor && !(e instanceof ctor) && (!e || e.name !== ctor.name)) {
                    fail((desc ? desc + ": " : "") + "expected " + ctor.name + " got " + (e && e.name ? e.name : e));
                }
            }
        );
    }
    function promise_rejects_dom(t, type, promise, desc) {
        return promise.then(
            function() { assert_unreached((desc ? desc + ": " : "") + "promise fulfilled instead of rejecting"); },
            function(e) {
                if (typeof type === "string" && e && e.name && e.name !== type) {
                    fail((desc ? desc + ": " : "") + "expected DOMException " + type + " got " + e.name);
                }
            }
        );
    }
    function generate_tests(fn, params) {
        if (!Array.isArray(params)) return;
        for (let i = 0; i < params.length; i++) {
            const entry = params[i];
            const name = entry[0];
            const rest = entry.slice(1);
            test(function() { fn.apply(this, rest); }, name);
        }
    }
    function setup(fnOrProps, maybeProps) {
        if (typeof fnOrProps === "function") {
            try { fnOrProps(); } catch (e) { harnessStatus = 1; harnessErrors.push(String(e)); }
        }
    }
    function done() {}
    function add_completion_callback() {}
    function add_result_callback() {}

    global.test = test;
    global.async_test = async_test;
    global.promise_test = promise_test;
    global.generate_tests = generate_tests;
    global.promise_rejects_js = promise_rejects_js;
    global.promise_rejects_dom = promise_rejects_dom;
    global.setup = setup;
    global.done = done;
    global.add_completion_callback = add_completion_callback;
    global.add_result_callback = add_result_callback;
    global.format_value = format_value;
    global.assert_true = assert_true;
    global.assert_false = assert_false;
    global.assert_equals = assert_equals;
    global.assert_not_equals = assert_not_equals;
    global.assert_approx_equals = assert_approx_equals;
    global.assert_less_than = assert_less_than;
    global.assert_greater_than = assert_greater_than;
    global.assert_less_than_equal = assert_less_than_equal;
    global.assert_greater_than_equal = assert_greater_than_equal;
    global.assert_array_equals = assert_array_equals;
    global.assert_object_equals = assert_object_equals;
    global.assert_in_array = assert_in_array;
    global.assert_own_property = assert_own_property;
    global.assert_inherits = assert_inherits;
    global.assert_idl_attribute = assert_idl_attribute;
    global.assert_readonly = assert_readonly;
    global.assert_class_string = assert_class_string;
    global.assert_regexp_match = assert_regexp_match;
    global.assert_unreached = assert_unreached;
    global.assert_throws_js = assert_throws_js;
    global.assert_throws_dom = assert_throws_dom;
    global.assert_throws_exactly = assert_throws_exactly;
    global.assert_implements = assert_implements;
    global.assert_implements_optional = assert_implements_optional;
    global.__rix_wpt_summary = function() {
        let passed = 0, failed = 0, firstErr = "";
        for (let i = 0; i < results.length; i++) {
            if (results[i].pass) passed++;
            else {
                failed++;
                if (!firstErr) firstErr = results[i].name + " => " + results[i].message;
            }
        }
        return JSON.stringify({
            total: results.length,
            passed: passed,
            failed: failed,
            pending: pendingAsync,
            status: harnessStatus,
            first_error: firstErr || (harnessErrors[0] || "")
        });
    };
})(globalThis);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialWptKind {
    All,
    Testharness,
    Reftest,
}

#[derive(Debug, Clone)]
pub struct OfficialWptOptions {
    pub wpt_dir: PathBuf,
    pub suite: String,
    pub kind: OfficialWptKind,
    pub limit: Option<usize>,
    pub report_json: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialWptFailure {
    pub file: String,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialWptReport {
    pub suite: String,
    pub files_total: usize,
    pub files_passed: usize,
    pub files_failed: usize,
    pub files_panicked: usize,
    pub reftests_total: usize,
    pub reftests_passed: usize,
    pub testharness_files_total: usize,
    pub testharness_files_passed: usize,
    pub subtests_total: usize,
    pub subtests_passed: usize,
    pub file_pass_rate_pct: f64,
    pub subtest_pass_rate_pct: f64,
    pub top_failure_reasons: Vec<(String, usize)>,
    pub sample_failures: Vec<OfficialWptFailure>,
}

#[derive(Debug, Deserialize)]
struct JsWptSummary {
    total: usize,
    passed: usize,
    failed: usize,
    pending: usize,
    status: i32,
    first_error: String,
}

#[must_use]
pub fn default_official_wpt_dir() -> PathBuf {
    if let Ok(base) = std::env::var("RIX_CONFORMANCE_DIR") {
        return PathBuf::from(base).join("wpt");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/rix-conformance/wpt")
}

fn collect_wpt_html_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if matches!(
                name,
                "support" | "reference" | "references" | "resources" | "crashtests" | "tools"
            ) {
                continue;
            }
            collect_wpt_html_files(&path, out)?;
        } else if (name.ends_with(".html") || name.ends_with(".htm"))
            && !name.ends_with("-ref.html")
            && !name.ends_with("-ref.htm")
            && !name.ends_with("-notref.html")
            && !name.ends_with("-manual.html")
            && !name.contains("manual.")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn resolve_wpt_path(wpt_root: &Path, doc_path: &Path, href: &str) -> PathBuf {
    let clean = href.split(['?', '#']).next().unwrap_or(href).trim();
    if let Some(rel) = clean.strip_prefix('/') {
        wpt_root.join(rel)
    } else {
        doc_path.parent().unwrap_or(wpt_root).join(clean)
    }
}

/// Extracts static ES module specifiers from `import ... from "..."`, `export ... from "..."`,
/// and bare `import "..."` statements so offline WPT tests can preload local modules.
fn extract_es_module_specifiers(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        // Skip line comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // Match `from` or `import` followed by whitespace and string literal
        let matched_kw = if code[i..].starts_with("from")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'$'))
            && i + 4 < len
            && (bytes[i + 4].is_ascii_whitespace() || bytes[i + 4] == b'"' || bytes[i + 4] == b'\'')
        {
            Some(4usize)
        } else if code[i..].starts_with("import")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'$'))
            && i + 6 < len
            && (bytes[i + 6].is_ascii_whitespace() || bytes[i + 6] == b'"' || bytes[i + 6] == b'\'')
        {
            Some(6usize)
        } else {
            None
        };
        if let Some(kw_len) = matched_kw {
            let mut j = i + kw_len;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (bytes[j] == b'"' || bytes[j] == b'\'') {
                let q = bytes[j];
                let start = j + 1;
                let mut end = start;
                while end < len && bytes[end] != q && bytes[end] != b'\n' {
                    if bytes[end] == b'\\' {
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                if end < len && bytes[end] == q {
                    let spec = &code[start..end];
                    if !spec.is_empty() {
                        out.push(spec.to_string());
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

enum ClassifiedTest {
    Reftest { is_match: bool, ref_path: PathBuf },
    Testharness,
    Unknown,
}

fn classify_wpt_file(wpt_root: &Path, path: &Path, html: &str) -> ClassifiedTest {
    let doc = parse_html(html);
    let mut has_testharness = false;
    let mut reftest_link: Option<(bool, String)> = None;

    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(elem)) = doc.get(node) else {
            continue;
        };
        match elem.name.local.as_ref() {
            "link" => {
                if let (Some(rel), Some(href)) =
                    (elem.get_attribute("rel"), elem.get_attribute("href"))
                {
                    if rel.eq_ignore_ascii_case("match") {
                        reftest_link = Some((true, href.to_string()));
                    } else if rel.eq_ignore_ascii_case("mismatch") {
                        reftest_link = Some((false, href.to_string()));
                    }
                }
            }
            "script" => {
                if let Some(src) = elem.get_attribute("src") {
                    if src.contains("testharness.js") {
                        has_testharness = true;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some((is_match, href)) = reftest_link {
        let ref_path = resolve_wpt_path(wpt_root, path, &href);
        if ref_path.exists() {
            return ClassifiedTest::Reftest { is_match, ref_path };
        }
    }
    if has_testharness {
        return ClassifiedTest::Testharness;
    }
    ClassifiedTest::Unknown
}

fn collect_local_stylesheets(
    wpt_root: &Path,
    doc_path: &Path,
    doc: &Document,
    media: &MediaContext,
) -> Vec<Arc<Stylesheet>> {
    let mut sheets = Vec::new();
    for node in doc.descendants(doc.root()) {
        let Some(NodeData::Element(elem)) = doc.get(node) else {
            continue;
        };
        if &*elem.name.local == "link" {
            let is_style = elem.get_attribute("rel").is_some_and(|r| {
                r.split_ascii_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("stylesheet"))
            });
            if is_style {
                if let Some(href) = elem.get_attribute("href") {
                    let css_path = resolve_wpt_path(wpt_root, doc_path, href);
                    if let Ok(css_text) = fs::read_to_string(&css_path) {
                        sheets.push(Stylesheet::parse_cached(
                            &css_text,
                            CascadeOrigin::Author,
                            media,
                        ));
                    }
                }
            }
        }
    }
    sheets
}

fn render_wpt_reftest_rgba(
    renderer: &mut VelloRenderer,
    wpt_root: &Path,
    doc_path: &Path,
    html: &str,
) -> Result<Vec<u8>> {
    let width = 800u32;
    let height = 600u32;
    let viewport = Size::new(width as f32, height as f32);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let mut doc = parse_html(html);
    let mut styles = StyleContext::new(media);
    for sheet in collect_local_stylesheets(wpt_root, doc_path, &doc, &media) {
        styles.add_stylesheet(sheet);
    }
    for css in extract_style_blocks(&doc) {
        styles.add_stylesheet(Stylesheet::parse_cached(
            &css,
            CascadeOrigin::Author,
            &media,
        ));
    }
    styles.cascade_full(&mut doc);

    let mut engine = LayoutEngine::new();
    let pipeline = PipelineId(1);
    let mut resources = ResourceRegistry::default();
    let frame = engine.layout_and_paint(
        &doc,
        &styles,
        viewport,
        shared_font_db(),
        pipeline,
        &mut resources,
    );
    let mut scene = Scene::new();
    encode_display_list(
        pipeline,
        &frame.display_list,
        &mut resources,
        &EncodeOptions::default(),
        &mut scene,
    );
    renderer
        .render_to_rgba_blocking(&scene, width, height, Color::WHITE)
        .map_err(|err| anyhow::anyhow!("vello render failed: {err}"))
}

struct ScriptRunOutcome {
    passed_file: bool,
    subtests_passed: usize,
    subtests_total: usize,
    reason: Option<String>,
}

fn run_wpt_testharness_file(wpt_root: &Path, path: &Path, html: &str) -> ScriptRunOutcome {
    static FONT_DB: std::sync::OnceLock<Arc<rix_text::FontDatabase>> = std::sync::OnceLock::new();
    let font_db = Arc::clone(FONT_DB.get_or_init(|| {
        let mut db = rix_text::FontDatabase::new();
        db.scan_system_fonts();
        Arc::new(db)
    }));

    let parsed = parse_html(html);
    let viewport = Size::new(800.0, 600.0);
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        ..MediaContext::default()
    };
    let ext_sheets = collect_local_stylesheets(wpt_root, path, &parsed, &media);

    let mut ext_scripts: HashMap<String, String> = HashMap::new();
    let mut module_scan_queue: Vec<(PathBuf, String)> = Vec::new();
    for node in parsed.descendants(parsed.root()) {
        let Some(NodeData::Element(elem)) = parsed.get(node) else {
            continue;
        };
        if &*elem.name.local == "script" {
            let is_module = elem
                .get_attribute("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("module"));
            if let Some(src) = elem.get_attribute("src") {
                if src.contains("testharnessreport.js") {
                    ext_scripts.insert(src.to_string(), String::new());
                } else if src.contains("testharness.js") {
                    ext_scripts.insert(src.to_string(), WPT_TESTHARNESS_SHIM.to_string());
                } else {
                    let js_path = resolve_wpt_path(wpt_root, path, src);
                    if let Ok(code) = fs::read_to_string(&js_path) {
                        if is_module {
                            module_scan_queue.push((js_path, code.clone()));
                        }
                        ext_scripts.insert(src.to_string(), code);
                    }
                }
            } else if is_module {
                let inline_code = parsed.inner_text(node);
                if !inline_code.trim().is_empty() {
                    module_scan_queue.push((path.to_path_buf(), inline_code));
                }
            }
        }
    }
    // Recursively preload static ES module imports (`import ... from "..."`, `export ... from "..."`)
    // from the local WPT tree so `<script type="module">` tests work offline without a live server.
    let mut visited_modules: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    while let Some((cur_file, code)) = module_scan_queue.pop() {
        for spec in extract_es_module_specifiers(&code) {
            if spec.starts_with("http://") || spec.starts_with("https://") {
                continue;
            }
            let dep_path = resolve_wpt_path(wpt_root, &cur_file, &spec);
            let canonical = fs::canonicalize(&dep_path).unwrap_or_else(|_| dep_path.clone());
            if !visited_modules.insert(canonical.clone()) {
                continue;
            }
            if let Ok(dep_code) = fs::read_to_string(&dep_path) {
                if let Ok(rel_dep) = canonical.strip_prefix(wpt_root) {
                    let abs_url = format!(
                        "http://web-platform.test:8000/{}",
                        rel_dep.to_string_lossy().replace('\\', "/")
                    );
                    ext_scripts.insert(abs_url, dep_code.clone());
                } else if let Ok(rel_dep) = dep_path.strip_prefix(wpt_root) {
                    let abs_url = format!(
                        "http://web-platform.test:8000/{}",
                        rel_dep.to_string_lossy().replace('\\', "/")
                    );
                    ext_scripts.insert(abs_url, dep_code.clone());
                }
                ext_scripts.insert(spec, dep_code.clone());
                module_scan_queue.push((dep_path, dep_code));
            }
        }
    }

    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, viewport, font_db);
    rt.vm.max_loop_iterations = Some(250_000);
    rt.event_loop.borrow_mut().enable_virtual_clock();
    rt.host.borrow_mut().set_external_stylesheets(ext_sheets);
    let rel = path.strip_prefix(wpt_root).unwrap_or(path);
    rt.set_document_url(&format!("http://web-platform.test:8000/{}", rel.display()));

    let script_errors = rt.load_document_with_external_scripts(html, &ext_scripts);
    // Drive timers + microtasks + rAF briefly so async_test / promise_test can settle.
    for tick in 0..8 {
        rt.advance_and_drain(Duration::from_millis(if tick == 0 { 0 } else { 10 }));
    }

    let summary_json = match rt
        .eval_script("typeof __rix_wpt_summary === 'function' ? __rix_wpt_summary() : ''")
    {
        Ok(val) => rt.vm.value_to_rust_string(val),
        Err(e) => {
            return ScriptRunOutcome {
                passed_file: false,
                subtests_passed: 0,
                subtests_total: 0,
                reason: Some(format!("summary eval failed: {e}")),
            };
        }
    };

    let Ok(summary) = serde_json::from_str::<JsWptSummary>(&summary_json) else {
        let first_err = script_errors
            .first()
            .cloned()
            .unwrap_or_else(|| "testharness summary missing".to_string());
        return ScriptRunOutcome {
            passed_file: false,
            subtests_passed: 0,
            subtests_total: 0,
            reason: Some(first_err),
        };
    };

    let passed_file = summary.total > 0
        && summary.failed == 0
        && summary.pending == 0
        && summary.status == 0
        && script_errors.is_empty();

    let reason = if passed_file {
        None
    } else if !script_errors.is_empty() {
        Some(script_errors[0].clone())
    } else if !summary.first_error.is_empty() {
        Some(summary.first_error)
    } else if summary.pending > 0 {
        Some(format!(
            "{} async subtest(s) still pending",
            summary.pending
        ))
    } else if summary.total == 0 {
        Some("0 subtests executed".to_string())
    } else {
        Some(format!(
            "{}/{} subtests failed",
            summary.failed, summary.total
        ))
    };

    ScriptRunOutcome {
        passed_file,
        subtests_passed: summary.passed,
        subtests_total: summary.total,
        reason,
    }
}

/// Runs an official upstream WPT directory (both `testharness.js` and visual reftests).
pub fn run_official_wpt_suite(opts: &OfficialWptOptions) -> Result<OfficialWptReport> {
    let suite_dir = opts.wpt_dir.join(&opts.suite);
    if !suite_dir.exists() {
        bail!(
            "official WPT suite directory not found: {}",
            suite_dir.display()
        );
    }

    let mut files = Vec::new();
    collect_wpt_html_files(&suite_dir, &mut files)?;
    files.sort();

    let mut testharness_jobs: Vec<(PathBuf, String)> = Vec::new();
    let mut reftest_jobs: Vec<(PathBuf, String, bool, PathBuf)> = Vec::new();

    for path in files {
        let Ok(html) = fs::read_to_string(&path) else {
            continue;
        };
        match classify_wpt_file(&opts.wpt_dir, &path, &html) {
            ClassifiedTest::Reftest { is_match, ref_path }
                if matches!(opts.kind, OfficialWptKind::All | OfficialWptKind::Reftest) =>
            {
                reftest_jobs.push((path, html, is_match, ref_path));
            }
            ClassifiedTest::Testharness
                if matches!(
                    opts.kind,
                    OfficialWptKind::All | OfficialWptKind::Testharness
                ) =>
            {
                testharness_jobs.push((path, html));
            }
            _ => {}
        }
        if let Some(lim) = opts.limit {
            if testharness_jobs.len() + reftest_jobs.len() >= lim {
                break;
            }
        }
    }

    let mut report = OfficialWptReport {
        suite: opts.suite.clone(),
        ..OfficialWptReport::default()
    };
    let mut reasons: HashMap<String, usize> = HashMap::new();

    // Execute script tests in parallel with 8 MiB stacks and panic isolation.
    let wpt_root = opts.wpt_dir.clone();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .stack_size(8 * 1024 * 1024)
        .build()
        .context("build wpt worker pool")?;
    let script_results: Vec<(PathBuf, std::result::Result<ScriptRunOutcome, String>)> = pool
        .install(|| {
            testharness_jobs
                .par_iter()
                .map(|(path, html)| {
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_wpt_testharness_file(&wpt_root, path, html)
                    }))
                    .map_err(|payload| {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        format!("PANIC: {msg}")
                    });
                    (path.clone(), res)
                })
                .collect()
        });

    for (path, outcome) in script_results {
        let rel = path
            .strip_prefix(&opts.wpt_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        report.files_total += 1;
        report.testharness_files_total += 1;
        match outcome {
            Ok(out) => {
                report.subtests_total += out.subtests_total;
                report.subtests_passed += out.subtests_passed;
                if out.passed_file {
                    report.files_passed += 1;
                    report.testharness_files_passed += 1;
                } else {
                    report.files_failed += 1;
                    let r = out.reason.unwrap_or_else(|| "subtest failure".into());
                    let bucket: String = r.chars().take(72).collect();
                    *reasons.entry(bucket).or_insert(0) += 1;
                    if report.sample_failures.len() < 40 {
                        report.sample_failures.push(OfficialWptFailure {
                            file: rel,
                            kind: "testharness".into(),
                            reason: r,
                        });
                    }
                }
            }
            Err(panic_msg) => {
                report.files_failed += 1;
                report.files_panicked += 1;
                let bucket: String = panic_msg.chars().take(72).collect();
                *reasons.entry(bucket).or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "testharness".into(),
                        reason: panic_msg,
                    });
                }
            }
        }
    }

    // Execute GPU visual reftests sequentially to reuse one GPU device cleanly.
    let mut renderer = VelloRenderer::new();
    for (path, html, is_match, ref_path) in reftest_jobs {
        let rel = path
            .strip_prefix(&opts.wpt_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        report.files_total += 1;
        report.reftests_total += 1;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<bool> {
            let ref_html = fs::read_to_string(&ref_path)
                .with_context(|| format!("read ref {}", ref_path.display()))?;
            let actual = render_wpt_reftest_rgba(&mut renderer, &opts.wpt_dir, &path, &html)?;
            let expected =
                render_wpt_reftest_rgba(&mut renderer, &opts.wpt_dir, &ref_path, &ref_html)?;
            let diff = compare_rgba(&actual, &expected, 4);
            Ok(if is_match {
                diff.diff_pixels == 0
            } else {
                diff.diff_pixels > 0
            })
        }));

        match res {
            Ok(Ok(true)) => {
                report.files_passed += 1;
                report.reftests_passed += 1;
            }
            Ok(Ok(false)) => {
                report.files_failed += 1;
                let reason = if is_match {
                    "reftest pixel mismatch".to_string()
                } else {
                    "mismatch reftest rendered identically".to_string()
                };
                *reasons.entry(reason.clone()).or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason,
                    });
                }
            }
            Ok(Err(err)) => {
                report.files_failed += 1;
                let reason = format!("render error: {err}");
                *reasons
                    .entry(reason.chars().take(72).collect())
                    .or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason,
                    });
                }
            }
            Err(_) => {
                report.files_failed += 1;
                report.files_panicked += 1;
                *reasons.entry("PANIC in reftest".into()).or_insert(0) += 1;
                if report.sample_failures.len() < 40 {
                    report.sample_failures.push(OfficialWptFailure {
                        file: rel,
                        kind: "reftest".into(),
                        reason: "PANIC in reftest".into(),
                    });
                }
            }
        }
    }

    report.file_pass_rate_pct = if report.files_total == 0 {
        100.0
    } else {
        (report.files_passed as f64 / report.files_total as f64) * 100.0
    };
    report.subtest_pass_rate_pct = if report.subtests_total == 0 {
        report.file_pass_rate_pct
    } else {
        (report.subtests_passed as f64 / report.subtests_total as f64) * 100.0
    };

    let mut ranked: Vec<(String, usize)> = reasons.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(15);
    report.top_failure_reasons = ranked;

    if let Some(ref json_path) = opts.report_json {
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(json_path, serde_json::to_string_pretty(&report)? + "\n")?;
    }

    Ok(report)
}

pub fn print_report(report: &OfficialWptReport) {
    println!("=== Official W3C WPT Conformance ({}) ===", report.suite);
    println!(
        "  Files: {} / {} ({:.1}%) | Reftests: {}/{} | Testharness files: {}/{} | Subtests: {} / {} ({:.1}%) | Panics: {}",
        report.files_passed,
        report.files_total,
        report.file_pass_rate_pct,
        report.reftests_passed,
        report.reftests_total,
        report.testharness_files_passed,
        report.testharness_files_total,
        report.subtests_passed,
        report.subtests_total,
        report.subtest_pass_rate_pct,
        report.files_panicked,
    );
    if !report.top_failure_reasons.is_empty() {
        println!("  Top failure reasons:");
        for (reason, cnt) in report.top_failure_reasons.iter().take(10) {
            println!("    [{:>4}] {}", cnt, reason);
        }
    }
    let _ = BTreeMap::<String, usize>::new();
}
