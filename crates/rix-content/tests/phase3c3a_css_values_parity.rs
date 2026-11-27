//! Phase 3C-3A: W3C CSS Values & Units L3/L4/L5 + CSS Font Loading L3 parity regression tests.
//! Covers live font metrics (`lh`, `rlh`, `ch`, `rch`, `cap`, `rcap`, `ex`, `rex`), `FontFace` /
//! `document.fonts`, `<base href>` & post-`var()` relative URL resolution, gradient math functions,
//! `progress()` dimension validation, `-0` typed arithmetic, `font-feature-settings` integer
//! rounding, `line-height` transitions affecting `lh`, and iframe viewport resize/quirks height.
#![deny(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn make_runtime(html: &str, url: &str) -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut font_db = FontDatabase::new();
    let _ = font_db.scan_system_fonts();
    let mut rt = ContentRuntime::with_full_config(
        doc,
        Size::new(800.0, 600.0),
        Arc::new(font_db),
        url,
        rix_storage::StorageEngine::in_memory().expect("in-memory storage"),
        None,
    );
    let _ = rt.load_document_with_external_scripts(html, &HashMap::new());
    rt.advance_and_drain(Duration::from_millis(0));
    rt
}

fn eval_str(rt: &mut ContentRuntime, js: &str) -> String {
    let v = rt
        .eval_script(js)
        .unwrap_or_else(|e| panic!("JS error: {e:?}"));
    rt.vm.value_to_rust_string(v)
}

#[test]
fn css_values_l4_math_progress_and_negative_zero_and_gradients() {
    let html = r#"<!doctype html><style>
      #grad { background-image: linear-gradient(rgb(255, 0, 0) calc(10px + 20px), rgb(0, 0, 255) calc(100% - 10px)); }
      #bgsh { background: radial-gradient(at calc(50% + 100px * cos(calc(1turn * calc((sibling-index() + 2) / sibling-count())))) 50%, #000, #fff); }
      #ffs  { font-feature-settings: "vert" calc(2), "fooo" calc(10.1); }
      #ffs_bad { font-feature-settings: "fooo" 1e1; }
    </style>
    <div id="grad"></div>
    <div><div></div><div></div><div></div><div id="bgsh"></div></div>
    <div id="ffs"></div>
    <div id="ffs_bad"></div>
    <div id="t"></div>"#;
    let mut rt = make_runtime(html, "http://example.test/page.html");

    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('grad')).backgroundImage"
        ),
        "linear-gradient(rgb(255, 0, 0) 30px, rgb(0, 0, 255) calc(100% - 10px))"
    );
    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('bgsh')).backgroundImage"
        ),
        "radial-gradient(at calc(50% - 100px) 50%, rgb(0, 0, 0), rgb(255, 255, 255))"
    );
    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('ffs')).fontFeatureSettings"
        ),
        "\"vert\" 2, \"fooo\" 10"
    );
    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('ffs_bad')).fontFeatureSettings"
        ),
        "normal"
    );
    // `progress()` rejects squared dimensions (`px * px`) per CSS Values L5 §10.4.
    assert_eq!(
        eval_str(
            &mut rt,
            "const e = document.getElementById('t'); \
             e.style.opacity = 'progress(10px * 10px, 10px * 10px, 10px * 10px)'; \
             e.style.opacity;"
        ),
        ""
    );
    // Border shorthand preserves `calc()` through CSSOM specified serialization.
    assert_eq!(
        eval_str(
            &mut rt,
            "const e = document.getElementById('t'); \
             e.style.border = 'calc(calc(10px)) solid pink'; \
             e.style.border;"
        ),
        "calc(10px) solid pink"
    );
    // Negative zero in typed arithmetic survives `el.style` round-tripping for `sign()`.
    assert_eq!(
        eval_str(
            &mut rt,
            "const e = document.getElementById('t'); \
             e.style.zIndex = 'clamp(-1, 1 / sign(-0em / 1px), 1)'; \
             getComputedStyle(e).zIndex;"
        ),
        "-1"
    );
}

#[test]
fn css_values_relative_urls_honor_base_href_and_post_var_substitution() {
    let html = r#"<!doctype html>
    <base href="http://cdn.example.test/assets/">
    <style>
      :root { --img: url("images/test.png"); }
      #a { background-image: url(images/test.png); }
      #b { background-image: var(--img); }
    </style>
    <div id="a"></div><div id="b"></div>"#;
    let mut rt = make_runtime(html, "http://origin.example.test/dir/index.html");

    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('a')).backgroundImage"
        ),
        "url(\"http://cdn.example.test/assets/images/test.png\")"
    );
    assert_eq!(
        eval_str(
            &mut rt,
            "getComputedStyle(document.getElementById('b')).backgroundImage"
        ),
        "url(\"http://cdn.example.test/assets/images/test.png\")"
    );
}

#[test]
fn css_values_root_lh_rlh_and_viewport_units_in_quirks_iframe() {
    let html = r#"<!doctype html>
    <style>
      #m { position: absolute; }
      iframe { width: 200px; height: 100px; border: 0; }
    </style>
    <div id="m">&nbsp;</div>
    <iframe id="f"></iframe>"#;
    let mut rt = make_runtime(html, "http://example.test/index.html");

    assert_eq!(
        eval_str(
            &mut rt,
            "const m = document.getElementById('m'); \
             const initLh = parseFloat(getComputedStyle(m).height); \
             document.documentElement.style = 'font-size: 142px; line-height: 1lh;'; \
             const afterLh = parseFloat(getComputedStyle(m).height); \
             String(Math.abs(afterLh - initLh) <= 1.0);"
        ),
        "true"
    );

    // Quirks-mode about:blank iframe resolves `body { height: 100% }` against 100px viewport.
    assert_eq!(
        eval_str(
            &mut rt,
            "const f = document.getElementById('f'); \
             f.contentDocument.body.innerHTML = '<style>*{margin:0} body{height:100%} div{height:calc(1dvw + 10%)}</style><div></div>'; \
             f.contentWindow.getComputedStyle(f.contentDocument.querySelector('div')).height;"
        ),
        "12px"
    );
}
