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

#[test]
fn css_color_l4_l5_wide_gamut_mix_relative_alpha_contrast_light_dark() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      #parent { color: red; background-color: color-mix(in srgb, currentColor 50%, green); }
      #child { color: green; background-color: inherit; }
      #dark { color-scheme: dark; color: light-dark(black, lime); }
      #light { color-scheme: light; color: light-dark(navy, lime); }
      #t { color: black; }
    </style>
    <div id="parent"><div id="child"></div></div>
    <div id="dark"></div>
    <div id="light"></div>
    <div id="t"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"(function() {
                const t = document.getElementById("t");
                if (getComputedStyle(document.getElementById("child")).backgroundColor !== "color(srgb 0 0.501961 0)") {
                    return "cc-inherit:" + getComputedStyle(document.getElementById("child")).backgroundColor;
                }
                if (getComputedStyle(document.getElementById("dark")).color !== "rgb(0, 255, 0)") return "dark";
                if (getComputedStyle(document.getElementById("light")).color !== "rgb(0, 0, 128)") return "light";
                t.style.color = "hwb(150 20% 10%)";
                if (getComputedStyle(t).color !== "rgb(51, 230, 140)") return "hwb:" + getComputedStyle(t).color;
                t.style.color = "hsl(from hsl(180 0 50%) h s l)";
                if (getComputedStyle(t).color !== "color(srgb 0.5 0.5 0.5)") return "powerless:" + getComputedStyle(t).color;
                t.style.color = "hwb(from hwb(120deg 20% 50% / .5) h w b / none)";
                if (getComputedStyle(t).color !== "hwb(120 20% 50% / none)") return "hwb-rel:" + getComputedStyle(t).color;
                t.style.color = "color-mix(in hsl longer hue, red, white)";
                if (getComputedStyle(t).color !== "color(srgb 0.625 0.875 0.875)") return "mix-long:" + getComputedStyle(t).color;
                t.style.color = "alpha(from currentcolor / calc(alpha * 0.5))";
                if (t.style.color !== "alpha(from currentcolor / calc(0.5 * alpha))") return "alpha:" + t.style.color;
                t.style.color = "contrast-color(navy)";
                if (getComputedStyle(t).color !== "rgb(255, 255, 255)") return "contrast:" + getComputedStyle(t).color;
                t.style.opacity = "clamp(50%, 80%, 70%)";
                if (t.style.opacity !== "calc(70%)") return "opacity:" + t.style.opacity;
                return "ok";
            })()"#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn css_values_l4_l5_math_units_and_sibling_functions() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html><style>
      :root { font-size: 10px; line-height: 20px; }
      #wrap { width: 100px; line-height: 20px; }
      #sibs > div { width: calc(10px * sibling-index()); z-index: sibling-count(); }
    </style>
    <div id="wrap"><div id="t" style="font-size: 10px;"></div></div>
    <div id="sibs"><div id="rm"></div><div></div><div id="s3"></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"(function() {
                const t = document.getElementById("t");
                const s3 = document.getElementById("s3");
                if (s3.offsetWidth !== 30) return "sib-w:" + s3.offsetWidth;
                if (getComputedStyle(s3).zIndex !== "3") return "sib-cnt:" + getComputedStyle(s3).zIndex;
                document.getElementById("rm").remove();
                if (s3.offsetWidth !== 20) return "sib-rm-w:" + s3.offsetWidth;
                if (getComputedStyle(s3).zIndex !== "2") return "sib-rm-cnt:" + getComputedStyle(s3).zIndex;
                t.style.marginLeft = "round(10%, 5px)";
                if (getComputedStyle(t).marginLeft !== "10px") return "round:" + getComputedStyle(t).marginLeft;
                t.style.scale = "calc(10em / 1rem)";
                if (getComputedStyle(t).scale !== "10") return "typed:" + getComputedStyle(t).scale;
                t.style.rotate = "atan2(10px, 10px)";
                if (getComputedStyle(t).rotate !== "45deg") return "atan2:" + getComputedStyle(t).rotate;
                t.style.scale = "progress(no-clamp 100px, 0px, 50px)";
                if (getComputedStyle(t).scale !== "2") return "prog-nc:" + getComputedStyle(t).scale;
                t.style.scale = "progress(100px, 0px, 50px)";
                if (getComputedStyle(t).scale !== "1") return "prog:" + getComputedStyle(t).scale;
                t.style.marginTop = "clamp(none, 33px, 30px)";
                if (t.style.marginTop !== "calc(30px)") return "clamp-none:" + t.style.marginTop;
                t.style.width = "calc(2lh + 1rlh)";
                if (getComputedStyle(t).width !== "60px") return "lh:" + getComputedStyle(t).width;
                t.style.lineHeight = "calc(10% / 1px)";
                if (getComputedStyle(t).lineHeight !== "10px") return "lh-typed:" + getComputedStyle(t).lineHeight;
                t.style.gridTemplateRows = "calc(1fr + 2fr)";
                if (getComputedStyle(t).gridTemplateRows !== "3fr") return "fr:" + getComputedStyle(t).gridTemplateRows;
                t.style.zIndex = "calc(10% / 20%)";
                if (t.style.zIndex !== "") return "z-pct:" + t.style.zIndex;
                t.style.zIndex = "calc(log(2, 1))";
                if (getComputedStyle(t).zIndex !== "0") return "log-nan:" + getComputedStyle(t).zIndex;
                return "ok";
            })()"#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn calc_size_interpolate_size_and_css_animations() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html><style>
      @keyframes grow { from { width: 0px; } to { width: auto; } }
      #box { width: calc-size(auto, size / 2 + 10px); interpolate-size: allow-keywords; }
      #anim { width: 100px; interpolate-size: allow-keywords; animation: grow 2s -1s linear paused; }
      #tr { width: 100px; interpolate-size: allow-keywords; }
      #wa { width: 40px; interpolate-size: allow-keywords; }
    </style>
    <div style="width: 200px;"><div id="box"></div><div id="anim"></div><div id="tr"></div><div id="wa"></div></div>"#;
    assert!(rt
        .load_document_with_external_scripts(html, &HashMap::new())
        .is_empty());
    let res = rt
        .eval_script(
            r#"(function() {
                const box = document.getElementById("box");
                if (box.offsetWidth !== 110) return "calc-size-w:" + box.offsetWidth;
                if (getComputedStyle(box).interpolateSize !== "allow-keywords") return "interp-size";
                const anim = document.getElementById("anim");
                if (getComputedStyle(anim).width !== "100px") return "anim-w:" + getComputedStyle(anim).width;
                const tr = document.getElementById("tr");
                getComputedStyle(tr).width;
                tr.style.transition = "width 2s -1s linear";
                tr.style.width = "auto";
                if (getComputedStyle(tr).width !== "150px") return "tr-w:" + getComputedStyle(tr).width;
                const wa = document.getElementById("wa");
                const a = wa.animate([{ width: "40px" }, { width: "auto" }], { duration: 1000, fill: "both" });
                a.pause(); a.currentTime = 500;
                if (getComputedStyle(wa).width !== "120px") return "wa-w:" + getComputedStyle(wa).width;
                return "ok";
            })()"#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}
