//! Spec regression tests for Phase 3 Part 1:
//! - CSS Display Level 3/4 multi-keyword parsing, serialization & blockification
//! - CSS Positioned Layout Level 3 (true containing blocks, IMCB, auto margins, `position: sticky`)
//! - CSS Positioned Layout Level 4 (`overlay`, top-layer `dialog:modal` position)
//! - W3C CSSOM View geometry (`offsetParent`, `offset*`, `client*`, `scroll*`, `getBoundingClientRect`)

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
fn display_l3_keywords_blockification_and_top_layer_overlay() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      #runin { display: run-in flow-root; }
      #bad { display: content; }
      #grid { display: grid; }
      #wrap { display: contents; }
      #dlg { position: sticky; }
      #p::first-line { display: flex; }
    </style>
    <div id="runin"></div>
    <div id="bad"></div>
    <div id="grid"><div id="wrap"><span id="item"></span></div></div>
    <dialog id="dlg"></dialog>
    <p id="p">hello</p>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                if (getComputedStyle(runin).display !== "run-in flow-root") return "runin:" + getComputedStyle(runin).display;
                if (getComputedStyle(bad).display !== "block") return "bad:" + getComputedStyle(bad).display;
                // Blockification across display: contents inside grid container
                if (getComputedStyle(item).display !== "block") return "item:" + getComputedStyle(item).display;
                // Dynamic de-blockification when container changes from grid -> block
                grid.style.display = "block";
                if (getComputedStyle(item).display !== "inline") return "deblock:" + getComputedStyle(item).display;
                // ::first-line ignores display
                if (getComputedStyle(p, "::first-line").display !== "inline") return "first-line:" + getComputedStyle(p, "::first-line").display;
                // Top-layer dialog: position sticky -> absolute, overlay none -> auto
                if (getComputedStyle(dlg).position !== "sticky") return "dlg-pre-pos:" + getComputedStyle(dlg).position;
                if (getComputedStyle(dlg).overlay !== "none") return "dlg-pre-ov:" + getComputedStyle(dlg).overlay;
                dlg.showModal();
                if (getComputedStyle(dlg).position !== "absolute") return "dlg-pos:" + getComputedStyle(dlg).position;
                if (getComputedStyle(dlg).overlay !== "auto") return "dlg-ov:" + getComputedStyle(dlg).overlay;
                dlg.close();
                if (getComputedStyle(dlg).position !== "sticky") return "dlg-closed:" + getComputedStyle(dlg).position;
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn positioned_layout_l3_containing_blocks_imcb_and_cssom_view() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #cb { position: relative; width: 200px; height: 100px; border: 10px solid black; padding: 5px; }
      #staticWrap { width: 50px; height: 20px; }
      #abs { position: absolute; left: 10px; right: 10px; top: 0; bottom: 0; margin: auto; width: 40px; height: 20px; }
      #fixedHost { contain: paint; width: 120px; height: 80px; }
      #fixedChild { position: fixed; width: 100%; height: 10px; }
      #oofStretch { position: absolute; top: 0; bottom: 0; left: 0; right: 0; }
      #relChild { position: relative; top: 50%; width: 10px; height: 10px; }
    </style>
    <div id="cb">
      <div id="staticWrap"><div id="abs"></div></div>
      <div id="oofStretch"><div id="relChild"></div></div>
    </div>
    <div id="fixedHost"><div id="fixedChild"></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                // cb padding box is 210x110 (content 200x100 + padding 5px each side).
                if (cb.clientWidth !== 210 || cb.clientHeight !== 110) return "cb-client:" + cb.clientWidth + "x" + cb.clientHeight;
                if (cb.clientLeft !== 10 || cb.clientTop !== 10) return "cb-border:" + cb.clientLeft + "," + cb.clientTop;
                // abs resolves against #cb (skipping #staticWrap): IMCB is x=[10..200] (190px), y=[0..110] (110px).
                // Auto margins center 40x20 inside 190x110 -> offsetLeft = 10 + (190-40)/2 = 85, offsetTop = (110-20)/2 = 45.
                if (abs.offsetParent !== cb) return "op";
                if (abs.offsetLeft !== 85 || abs.offsetTop !== 45) return "abs:" + abs.offsetLeft + "," + abs.offsetTop;
                // Percentage top:50% inside stretched OOF (110px tall) -> 55px.
                if (relChild.offsetTop !== 55) return "rel-pct:" + relChild.offsetTop;
                // contain: paint establishes fixed CB (120px wide), removing it reverts to viewport (800px).
                if (fixedChild.getBoundingClientRect().width !== 120) return "fixed-pre:" + fixedChild.getBoundingClientRect().width;
                fixedHost.style.contain = "none";
                if (fixedChild.getBoundingClientRect().width !== 800) return "fixed-post:" + fixedChild.getBoundingClientRect().width;
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}

#[test]
fn sticky_positioning_overconstrained_and_single_axis_scrollers() {
    let mut rt = make_runtime();
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #scroller { position: relative; height: 100px; overflow: auto; }
      .pad { height: 200px; }
      #over { position: sticky; top: 20px; bottom: 0; height: 200px; }
      #outerY { overflow-y: scroll; overflow-x: clip; width: 100px; height: 100px; }
      #innerX { overflow-x: scroll; overflow-y: clip; width: 100px; height: 200px; }
      #dual { position: sticky; top: 10px; left: 20px; width: 10px; height: 10px; }
      #spacer { width: 400px; height: 400px; }
    </style>
    <div id="scroller">
      <div class="pad"></div>
      <div id="over"></div>
      <div class="pad"></div>
    </div>
    <div id="outerY"><div id="innerX"><div id="dual"></div><div id="spacer"></div></div></div>"#;
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");

    let res = rt
        .eval_script(
            r#"
            (function() {
                scroller.scrollTop = 0;
                if (over.offsetTop !== 20) return "s0:" + over.offsetTop;
                scroller.scrollTop = 160;
                if (over.offsetTop !== 180) return "s160:" + over.offsetTop;
                scroller.scrollTop = 500;
                if (over.offsetTop !== 400) return "s500:" + over.offsetTop;

                // Independent single-axis scrollers (overflow-x: scroll + overflow-y: clip)
                innerX.scrollLeft = 50;
                outerY.scrollTop = 60;
                const ro = outerY.getBoundingClientRect();
                const rd = dual.getBoundingClientRect();
                if (Math.round(rd.left - ro.left) !== 20) return "dx:" + (rd.left - ro.left);
                if (Math.round(rd.top - ro.top) !== 10) return "dy:" + (rd.top - ro.top);
                return "ok";
            })()
            "#,
        )
        .expect("script should succeed");
    assert_eq!(rt.vm.value_to_rust_string(res), "ok");
}
