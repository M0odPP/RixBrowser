//! Phase 3A regression tests: `getComputedStyle` spec parity with Mozilla Firefox 161.0.
//!
//! Covers:
//! - CSS Sizing L3 §4.1 (`min-width` / `min-height` initial `auto` across all boxes)
//! - CSS Box Alignment L3 §8 (`row-gap` / `column-gap` initial `normal` & `gap: normal` reset)
//! - CSS Color 4 §14.1 (wide-gamut `oklch()` / `lab()` / `light-dark()` preservation through
//!   custom property `var()` substitution, `border` shorthands, logical borders, and `currentcolor`)
//! - CSSOM §6.7.2 (`top`/`right`/`bottom`/`left` resolved values for static/relative/absolute/fixed)
//! - HTML5 §15.3.4 + CSS Sizing L4 §5.1 (`img[width][height]` `aspect-ratio: auto w / h` hint &
//!   `auto || <ratio>` canonical serialization)
//! - CSS Backgrounds L3 §3.6 & §3.9 (multi-layer `background-size` / `background-position`)
//! - Gecko UA stylesheet parity (`table` border-color `currentcolor`, `label` `cursor: default`)

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn load_rt(html: &str) -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    let mut rt = ContentRuntime::new(doc, Size::new(800.0, 600.0), font_db);
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected load errors: {errs:?}");
    rt
}

fn eval_str(rt: &mut ContentRuntime, js: &str) -> String {
    let val = rt
        .eval_script(js)
        .unwrap_or_else(|e| panic!("JS error: {e:?}"));
    rt.vm.value_to_rust_string(val)
}

#[test]
fn min_size_auto_and_gap_normal_computed_values() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            #flex { display: flex; align-items: center; }
            #zeroGap { display: flex; gap: 0px; }
            #resetGap { display: grid; gap: 12px; row-gap: normal; }
            #inheritGap { display: flex; gap: inherit; }
        </style></head><body>
            <div id="blk"></div>
            <div id="flex"><div id="item"></div></div>
            <div id="zeroGap"></div>
            <div id="resetGap"><div id="inheritGap"></div></div>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const cs = id => getComputedStyle(document.getElementById(id));
        JSON.stringify({
            blkMinW: cs("blk").minWidth,
            blkMinH: cs("blk").minHeight,
            itemMinW: cs("item").minWidth,
            itemMinH: cs("item").minHeight,
            flexRowGap: cs("flex").rowGap,
            flexColGap: cs("flex").columnGap,
            zeroRowGap: cs("zeroGap").rowGap,
            zeroColGap: cs("zeroGap").columnGap,
            resetRowGap: cs("resetGap").rowGap,
            resetColGap: cs("resetGap").columnGap,
            inhRowGap: cs("inheritGap").rowGap,
            inhColGap: cs("inheritGap").columnGap,
        });
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["blkMinW"], "0px");
    assert_eq!(v["blkMinH"], "0px");
    assert_eq!(v["itemMinW"], "auto");
    assert_eq!(v["itemMinH"], "auto");
    assert_eq!(v["flexRowGap"], "normal");
    assert_eq!(v["flexColGap"], "normal");
    assert_eq!(v["zeroRowGap"], "0px");
    assert_eq!(v["zeroColGap"], "0px");
    assert_eq!(v["resetRowGap"], "normal");
    assert_eq!(v["resetColGap"], "12px");
    assert_eq!(v["inhRowGap"], "normal");
    assert_eq!(v["inhColGap"], "12px");
}

#[test]
fn wide_gamut_custom_properties_and_currentcolor_borders() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            :root {
                --brand: oklch(0.62 0.21 255);
                --surface: lab(90 -5 10);
            }
            #box1 {
                color: var(--brand);
                background-color: var(--surface);
            }
            #box2 {
                border: 1px solid var(--brand);
                border-block-end: 2px solid var(--surface);
                outline: 1px solid var(--brand);
            }
            table { color: #828282; }
        </style></head><body>
            <div id="box1"></div>
            <div id="box2"></div>
            <table id="tbl"><tr><td>cell</td></tr></table>
            <label id="lbl">Name</label>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const cs = id => getComputedStyle(document.getElementById(id));
        JSON.stringify({
            b1Color: cs("box1").color,
            b1Bg: cs("box1").backgroundColor,
            b1BorderTop: cs("box1").borderTopColor,
            b1Outline: cs("box1").outlineColor,
            b1TextDec: cs("box1").textDecorationColor,
            b2BorderTop: cs("box2").borderTopColor,
            b2BorderBottom: cs("box2").borderBottomColor,
            b2Outline: cs("box2").outlineColor,
            tblBorderTop: cs("tbl").borderTopColor,
            tblColor: cs("tbl").color,
            lblCursor: cs("lbl").cursor,
        });
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["b1Color"], "oklch(0.62 0.21 255)");
    assert_eq!(v["b1Bg"], "lab(90 -5 10)");
    assert_eq!(v["b1BorderTop"], "oklch(0.62 0.21 255)");
    assert_eq!(v["b1Outline"], "oklch(0.62 0.21 255)");
    assert_eq!(v["b1TextDec"], "oklch(0.62 0.21 255)");
    assert_eq!(v["b2BorderTop"], "oklch(0.62 0.21 255)");
    assert_eq!(v["b2BorderBottom"], "lab(90 -5 10)");
    assert_eq!(v["b2Outline"], "oklch(0.62 0.21 255)");
    assert_eq!(v["tblColor"], "rgb(130, 130, 130)");
    assert_eq!(v["tblBorderTop"], "rgb(130, 130, 130)");
    assert_eq!(v["lblCursor"], "default");
}

#[test]
fn aspect_ratio_hints_backgrounds_and_resolved_insets() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            #ar1 { aspect-ratio: 16 / 9 auto; }
            #bg1 {
                background: linear-gradient(red, blue) 98% 0 / 250% 100%;
            }
            #bg2 {
                background-image: linear-gradient(red, blue), linear-gradient(green, yellow);
                background-size: 10px, cover;
                background-position: 12px 13px, 50% 6px;
            }
            #rel { position: relative; }
            #absWrap { position: relative; width: 200px; height: 100px; }
            #abs { position: absolute; left: 20px; top: 10px; width: 50px; height: 30px; }
        </style></head><body>
            <img id="pic" width="2308" height="1022">
            <div id="ar1"></div>
            <div id="bg1"></div>
            <div id="bg2"></div>
            <div id="stat"></div>
            <div id="rel"></div>
            <div id="absWrap"><div id="abs"></div></div>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const cs = id => getComputedStyle(document.getElementById(id));
        JSON.stringify({
            picAr: cs("pic").aspectRatio,
            ar1: cs("ar1").aspectRatio,
            bg1Pos: cs("bg1").backgroundPosition,
            bg1Size: cs("bg1").backgroundSize,
            bg2Pos: cs("bg2").backgroundPosition,
            bg2Size: cs("bg2").backgroundSize,
            statTop: cs("stat").top,
            relTop: cs("rel").top,
            relLeft: cs("rel").left,
            absLeft: cs("abs").left,
            absRight: cs("abs").right,
            absTop: cs("abs").top,
            absBottom: cs("abs").bottom,
        });
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["picAr"], "auto 2308 / 1022");
    assert_eq!(v["ar1"], "auto 16 / 9");
    assert_eq!(v["bg1Pos"], "98% 0px");
    assert_eq!(v["bg1Size"], "250% 100%");
    assert_eq!(v["bg2Pos"], "12px 13px, 50% 6px");
    assert_eq!(v["bg2Size"], "10px auto, cover");
    assert_eq!(v["statTop"], "auto");
    assert_eq!(v["relTop"], "0px");
    assert_eq!(v["relLeft"], "0px");
    assert_eq!(v["absLeft"], "20px");
    assert_eq!(v["absRight"], "130px");
    assert_eq!(v["absTop"], "10px");
    assert_eq!(v["absBottom"], "60px");
}

#[test]
fn overflow_pairing_bolder_weight_background_repeat_and_utf8_safety() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            .utf8 { content: "hi — world"; width: 100px; }
            #ov1 { overflow-x: hidden; }
            #ov2 { overflow-x: clip; overflow-y: hidden; }
            #bg3 { background-repeat: no-repeat, space round; background-position: center bottom -50px; }
            #parentTxt { letter-spacing: 3px; line-height: 28px; color: rgb(200, 10, 10); }
        </style></head><body>
            <div class="utf8" id="u"></div>
            <div id="ov1"></div>
            <div id="ov2"></div>
            <div id="bg3"></div>
            <strong><b id="nestedBold">hi</b></strong>
            <img id="bimg" border="0">
            <div id="parentTxt"><button id="btn">Click</button></div>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const cs = id => getComputedStyle(document.getElementById(id));
        JSON.stringify({
            uWidth: cs("u").width,
            ov1x: cs("ov1").overflowX,
            ov1y: cs("ov1").overflowY,
            ov2x: cs("ov2").overflowX,
            ov2y: cs("ov2").overflowY,
            bg3Rep: cs("bg3").backgroundRepeat,
            bg3Pos: cs("bg3").backgroundPosition,
            nestedBoldW: cs("nestedBold").fontWeight,
            bimgStyle: cs("bimg").borderTopStyle,
            bimgWidth: cs("bimg").borderTopWidth,
            btnLs: cs("btn").letterSpacing,
            btnLh: cs("btn").lineHeight,
            btnColor: cs("btn").color,
        });
        "#,
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["uWidth"], "100px");
    assert_eq!(v["ov1x"], "hidden");
    assert_eq!(v["ov1y"], "auto");
    assert_eq!(v["ov2x"], "clip");
    assert_eq!(v["ov2y"], "hidden");
    assert_eq!(v["bg3Rep"], "no-repeat, space round");
    assert_eq!(v["bg3Pos"], "50% calc(100% + 50px)");
    assert_eq!(v["nestedBoldW"], "900");
    assert_eq!(v["bimgStyle"], "solid");
    assert_eq!(v["bimgWidth"], "0px");
    assert_eq!(v["btnLs"], "normal");
    assert_eq!(v["btnLh"], "normal");
    assert_eq!(v["btnColor"], "rgb(0, 0, 0)");
}
