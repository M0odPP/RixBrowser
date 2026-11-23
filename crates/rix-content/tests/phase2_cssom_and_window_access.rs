use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn make_runtime() -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    ContentRuntime::new(doc, Size::new(800.0, 600.0), font_db)
}

#[test]
fn css_var_exponential_expansion_and_cycles_do_not_oom_or_leak_debug() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      #boom {
        --a: x x;
        --b: var(--a) var(--a);
        --c: var(--b) var(--b);
        --d: var(--c) var(--c);
        --e: var(--d) var(--d);
        --f: var(--e) var(--e);
        --g: var(--f) var(--f);
        --h: var(--g) var(--g);
        --i: var(--h) var(--h);
        --j: var(--i) var(--i);
        --k: var(--j) var(--j);
        --l: var(--k) var(--k);
        --m: var(--l) var(--l);
        --n: var(--m) var(--m);
        --o: var(--n) var(--n);
        --p: var(--o) var(--o);
        --q: var(--p) var(--p);
        --r: var(--q) var(--q);
        --s: var(--r) var(--r);
        --t: var(--s) var(--s);
        color: var(--t, rgb(0, 128, 0));
      }
      #cyc {
        --x: var(--y);
        --y: var(--x);
        --z: var(--x, rgb(0, 0, 255));
        --fn: calc(10px + 20px);
        color: var(--z);
      }
    </style>
    <div id="boom"></div>
    <div id="cyc"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let boom_color = rt.eval_script("getComputedStyle(boom).color").unwrap();
    assert_eq!(rt.vm.value_to_rust_string(boom_color), "rgb(0, 128, 0)");

    let cyc_color = rt.eval_script("getComputedStyle(cyc).color").unwrap();
    assert_eq!(rt.vm.value_to_rust_string(cyc_color), "rgb(0, 0, 255)");

    let custom_fn = rt
        .eval_script("getComputedStyle(cyc).getPropertyValue('--fn')")
        .unwrap();
    let custom_fn_str = rt.vm.value_to_rust_string(custom_fn);
    assert!(
        !custom_fn_str.contains("Function {"),
        "custom property leaked Debug format: {custom_fn_str}"
    );
    assert!(custom_fn_str.contains("calc("));
}

#[test]
fn window_named_element_access_and_servo_selectors_and_cssom() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style id="s1">
      #box > span.hit { color: rgb(10, 20, 30); }
      #box::before { content: "hi"; color: rgb(1, 2, 3); }
    </style>
    <div id="box"><span class="hit">ok</span></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r##"
            (function() {
                if (box !== document.getElementById("box")) return "named-access-fail";
                if (!("box" in window)) return "in-window-fail";
                if (!("background-color" in getComputedStyle(box))) return "has-computed-fail";
                if (getComputedStyle(box, "::before").color !== "rgb(1, 2, 3)") return "pseudo-fail";
                if (CSS.supports("--", "initial") !== false) return "reserved-dash-fail";
                if (CSS.supports("display", "grid") !== true) return "supports-prop-fail";
                if (CSS.supports("display", "not-a-display") !== false) return "supports-invalid-fail";
                if (CSS.supports("selector(:has(> span.hit))") !== true) return "supports-has-fail";
                if (document.querySelector("div:has(> span.hit)") !== box) return "qs-has-fail";
                if (DOMException.SYNTAX_ERR !== 12) return "dom-exc-code-const-fail";

                let syntaxCode = 0;
                try { document.querySelector("div[=]"); } catch (e) { syntaxCode = e.code; }
                if (syntaxCode !== 12) return "qs-syntax-err-fail:" + syntaxCode;

                const st = document.createElement("style");
                document.head.appendChild(st);
                const { sheet } = st;
                document.head.removeChild(st);
                const { cssRules } = sheet;
                if (cssRules.length !== 0) return "empty-sheet-fail";
                sheet.insertRule(".a > .b { color: red; }");
                if (cssRules.length !== 1) return "live-rules-fail";
                if (cssRules[0].selectorText !== ".a > .b") return "sel-text-fail:" + cssRules[0].selectorText;
                sheet.deleteRule(0);
                if (cssRules.length !== 0) return "delete-rule-fail";

                const adopted = new CSSStyleSheet();
                adopted.replaceSync("#box { opacity: 0.5; }");
                document.adoptedStyleSheets = [adopted];
                if (getComputedStyle(box).opacity !== "0.5") return "adopted-fail:" + getComputedStyle(box).opacity;

                box.style.width = "not-a-length";
                if (box.style.getPropertyValue("width") !== "") return "invalid-inline-style-fail";
                box.style.width = "42px";
                if (box.style.getPropertyValue("width") !== "42px") return "valid-inline-style-fail";
                return "ok";
            })()
            "##,
        )
        .unwrap();
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn fetch_without_tokio_runtime_rejects_cleanly_without_panicking() {
    let mut rt = make_runtime();
    let _ = rt.eval_script(
        r#"
        var rejected = false;
        fetch("http://example.invalid/").catch(function(e) {
            rejected = e instanceof TypeError;
        });
        "#,
    );
    rt.advance_and_drain(Duration::from_millis(0));
    let val = rt.eval_script("rejected").unwrap();
    assert_eq!(rt.vm.value_to_rust_string(val), "true");
}
