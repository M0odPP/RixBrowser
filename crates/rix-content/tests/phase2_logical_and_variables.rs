//! Spec regression tests for CSS Logical Properties L1 CSSOM, live
//! `getComputedStyle(el)` layout-resolved values, CSS Variables L1/L2, and `@property`.

#![deny(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

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
fn live_computed_style_logical_properties_and_layout_resolution() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      #wrap { width: 200px; }
      #box { font-size: 40px; }
      #child { width: 60px; height: 80px; }
      #flexParent { display: flex; }
      #flexItem { min-block-size: auto; }
    </style>
    <div id="wrap"><div id="box"><div id="child"></div></div></div>
    <div id="flexParent"><div id="flexItem"></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                const cs = getComputedStyle(box);
                // Layout-resolved auto block-size & min-content inline-size
                if (cs.getPropertyValue("block-size") !== "80px") return "block-size:" + cs.getPropertyValue("block-size");
                box.style.inlineSize = "min-content";
                if (cs.getPropertyValue("inline-size") !== "60px") return "min-content:" + cs.getPropertyValue("inline-size");
                // Live update with deferred var(--wm) writing-mode
                box.style.cssText = "--wm: vertical-rl; writing-mode: var(--wm); margin-block-start: 11px; margin-inline-start: 7px;";
                if (cs.getPropertyValue("margin-right") !== "11px") return "wm-mright:" + cs.getPropertyValue("margin-right");
                if (cs.getPropertyValue("margin-top") !== "7px") return "wm-mtop:" + cs.getPropertyValue("margin-top");
                // Non-negative calc clamp & calc(% + px) preservation
                box.style.maxBlockSize = "calc(10px - 0.5em)";
                if (cs.getPropertyValue("max-block-size") !== "0px") return "clamp:" + cs.getPropertyValue("max-block-size");
                box.style.minBlockSize = "calc(20% + 10px)";
                if (cs.getPropertyValue("min-block-size") !== "calc(20% + 10px)") return "calc-pct:" + cs.getPropertyValue("min-block-size");
                // min-block-size: auto -> 0px in block vs auto in flex
                box.style.minBlockSize = "auto";
                if (cs.getPropertyValue("min-block-size") !== "0px") return "min-block:" + cs.getPropertyValue("min-block-size");
                if (getComputedStyle(flexItem).minBlockSize !== "auto") return "min-flex:" + getComputedStyle(flexItem).minBlockSize;
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn cssom_declaration_validation_shorthands_and_pending_substitution() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style id="s">#r { --var1: red; }</style>
    <div id="d1" style="background: var(--prop); background: green;"></div>
    <div id="d2" style="margin: var(--prop); margin-top: 10px"></div>
    <div id="d3" style="margin: var(--prop) !important; margin-top: 10px"></div>
    <div id="d4"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                if (d1.style.cssText !== "background: green;") return "d1:" + d1.style.cssText;
                if (d2.style.cssText !== "margin-right: ; margin-bottom: ; margin-left: ; margin-top: 10px;") return "d2:" + d2.style.cssText;
                if (d2.style.getPropertyValue("margin") !== "") return "d2-m:" + d2.style.getPropertyValue("margin");
                if (d3.style.getPropertyValue("margin") !== "var(--prop)") return "d3-m:" + d3.style.getPropertyValue("margin");

                // Logical validation: reject unitless non-zero, padding auto; canonical border & 0px
                const rule = document.styleSheets[0].cssRules[0];
                if (rule.style.cssText !== "--var1: red;") return "rule-init:" + rule.style.cssText;
                rule.style = "block-size: 1";
                if (rule.style.length !== 0) return "quirk-len:" + rule.style.length;

                d4.style.setProperty("padding-block-end", "auto");
                if (d4.style.getPropertyValue("padding-block-end") !== "") return "pad-auto";
                d4.style.setProperty("padding-block-start", "0");
                if (d4.style.getPropertyValue("padding-block-start") !== "0px") return "pad-zero:" + d4.style.getPropertyValue("padding-block-start");
                d4.style.setProperty("border-block-start", "green");
                if (d4.style.getPropertyValue("border-block-start") !== "green") return "border-green:" + d4.style.getPropertyValue("border-block-start");
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn at_property_dynamic_var_names_and_token_boundaries() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      @property --reg-no-inherit {
        syntax: '<color>';
        initial-value: lightgreen;
        inherits: false;
      }
      #parent {
        --reg-no-inherit: tomato;
        --target-w: 42px;
        --indirect: --target-w;
      }
      #child {
        background-color: var(--reg-no-inherit);
        width: var(var(--indirect));
      }
      #tok {
        --n: 20;
        text-indent: var(--n)%;
        --a: 11px;
        --b: 19px;
        border-spacing: var(--a)var(--b);
      }
    </style>
    <div id="parent"><div id="child"></div></div>
    <div id="tok"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                const cc = getComputedStyle(child);
                if (cc.backgroundColor !== "rgb(144, 238, 144)") return "at-prop:" + cc.backgroundColor;
                if (cc.width !== "42px") return "dyn-var:" + cc.width;
                const tc = getComputedStyle(tok);
                if (tc.getPropertyValue("text-indent") !== "0px") return "tok-pct:" + tc.getPropertyValue("text-indent");
                if (tc.getPropertyValue("border-spacing") !== "11px 19px") return "tok-bs:" + tc.getPropertyValue("border-spacing");
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn cascade_l5_layers_scope_revert_rule_and_nesting_cssom() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style id="s">
      @layer a.b, a;
      @layer a { #t { color: green; background-color: navy !important; } }
      @layer a.b { #t { color: red; background-color: lime !important; } }
      @scope (.outer) to (.limit) { z-index: 1; .item { z-index: 2; } }
      @scope (.inner) { .item { z-index: 3; } }
      #rr { color: green; }
      #rr { color: revert-rule; }
    </style>
    <div id="t"></div>
    <div class="outer"><div class="inner"><div id="inItem" class="item"></div></div>
      <div class="limit"><div id="limItem" class="item"></div></div></div>
    <div id="rr"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"(function() {
                if (getComputedStyle(t).color !== "rgb(0, 128, 0)") return "layer-norm";
                if (getComputedStyle(t).backgroundColor !== "rgb(0, 255, 0)") return "layer-imp";
                if (getComputedStyle(document.querySelector(".outer")).zIndex !== "1") return "scope-decl";
                if (getComputedStyle(inItem).zIndex !== "3") return "scope-prox";
                if (getComputedStyle(limItem).zIndex !== "auto") return "scope-limit";
                if (getComputedStyle(rr).color !== "rgb(0, 128, 0)") return "revert-rule";
                if (CSSStyleRule.__proto__ !== CSSGroupingRule) return "proto";
                const sheet = document.getElementById("s").sheet;
                const idx = sheet.insertRule(".a { color: red; }", sheet.cssRules.length);
                const parent = sheet.cssRules[idx];
                parent.insertRule("& .b { color: hotpink; }", 0);
                if (parent.cssRules[0].parentRule !== parent) return "parentRule";
                if (parent.cssRules[0].cssText !== "& .b { color: hotpink; }") return "color:" + parent.cssRules[0].cssText;
                let caught = "";
                try { parent.insertRule("@font-face {}", 0); } catch (e) { caught = e.name; }
                if (caught !== "HierarchyRequestError") return "hier:" + caught;
                return "ok";
            })()"#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}
