//! Spec regression tests for Phase 3 Part 2 (CSS Flexbox Level 1 + Box Alignment Level 3):
//! - Native pure-Rust Flexbox layout across writing modes, RTL, multi-line `align-content`, and `order`
//! - §9.7 flexible lengths (`flex-grow` / `flex-shrink < 1`), §4.5 automatic minimum height/width
//! - §9.8 definite stretched & flexed heights (`percentage-heights`, `position: relative; top: 50%`)
//! - §4.1 abspos flex static positions (`safe`/`unsafe`, `baseline`/`last baseline`)
//! - §9.8 vertical writing-mode & reversed container baselines (`row-reverse`, `column-reverse`, `vertical-rl`)
//! - CSSOM `flex` / `flex-flow` shorthands, `fit-content()`, and `-webkit-*` legacy name aliases

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
fn flexbox_l1_core_sizing_alignment_and_definite_heights() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #wrap { display: flex; flex-wrap: wrap; width: 20px; height: 200px; }
      #wa { width: 20px; height: 10px; flex: none; }
      #wb { width: 20px; height: auto; flex: none; }
      #wb_in { width: 10px; height: 30px; }
      #shrinkBox { display: flex; width: 100px; height: 20px; }
      #sh1 { width: 200px; flex-shrink: 0.5; }
      #pctBox { display: flex; height: 200px; width: 100px; }
      #pctChild { height: 50%; }
      #relChild { position: relative; top: 50%; height: 20px; }
      #colOuter { display: inline-flex; flex-direction: column; border: 1px solid #000; }
      #colInner { flex-basis: 0px; display: flex; flex-direction: column; min-height: auto; border: 2px solid #0ff; }
      .ci { flex: 0 0 44px; border: 3px solid #f0f; }
      #absHost { display: flex; position: relative; width: 10px; height: 6px; border: 1px solid black; }
      #absItem { position: absolute; width: 2px; height: 2px; align-self: last baseline; }
    </style>
    <div id="wrap"><div id="wa"></div><div id="wb"><div id="wb_in"></div></div></div>
    <div id="shrinkBox"><div id="sh1"></div></div>
    <div id="pctBox"><div id="pctItem"><div id="pctChild"></div><div id="relChild"></div></div></div>
    <div id="colOuter"><div id="colInner"><div class="ci"></div><div class="ci"></div></div></div>
    <div id="absHost"><div id="absItem"></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                // Multi-line stretch distributes leftover 160px equally across 2 lines (+80px each).
                if (wb.offsetHeight !== 110 || wb.offsetTop !== 90) return "wrap:" + wb.offsetHeight + "@" + wb.offsetTop;
                // Fractional flex-shrink 0.5 shrinks 200px in 100px container by 50px -> 150px.
                if (sh1.offsetWidth !== 150) return "shrink:" + sh1.offsetWidth;
                // Stretched flex item has definite height 200px -> child 50% = 100px, rel top:50% = 100px.
                if (pctChild.offsetHeight !== 100) return "pct:" + pctChild.offsetHeight;
                if (relChild.offsetTop - pctItem.offsetTop - pctChild.offsetHeight !== 100) return "rel:" + relChild.offsetTop;
                // Column flex item with flex-basis: 0px + min-height: auto grows to content height (100px + 4px borders).
                if (colOuter.clientHeight !== 104 || colInner.clientHeight !== 100) return "col:" + colOuter.clientHeight + "/" + colInner.clientHeight;
                // Abspos flex child with `align-self: last baseline` falls back to writing-mode end edge (6 - 2 = 4).
                if (absItem.offsetTop !== 4) return "abs:" + absItem.offsetTop;
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn flexbox_l1_vertical_writing_mode_and_cssom_shorthands() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #vwrap { display: flex; position: relative; writing-mode: vertical-rl; flex-wrap: wrap; align-content: flex-start; height: 20px; }
      .vi { flex: 0 0 10px; width: 10px; }
      .vw { flex: 0 0 10px; width: 30px; }
      #target { font-size: 20px; }
    </style>
    <div id="vwrap"><div class="vi"></div><div class="vw"></div><div class="vi"></div><div class="vw"></div></div>
    <div id="target"></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                // Vertical-rl multi-line flex shrinks auto width to 2 lines * 30px = 60px.
                if (vwrap.offsetWidth !== 60 || vwrap.offsetHeight !== 20) return "vwrap:" + vwrap.offsetWidth + "x" + vwrap.offsetHeight;
                // CSSOM flex shorthand, fit-content(), and -webkit-* legacy aliases.
                target.style.flex = "1 2 0";
                if (target.style.flex !== "1 2 0px") return "flex0:" + target.style.flex;
                target.style.flex = "fit-content(10%) 2 3";
                if (target.style.flexBasis !== "fit-content(10%)") return "fitc:" + target.style.flexBasis;
                target.style.setProperty("-webkit-align-content", "safe center");
                if (target.style.getPropertyValue("align-content") !== "safe center") return "wk:" + target.style.getPropertyValue("align-content");
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}
