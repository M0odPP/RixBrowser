//! Phase 2C regression tests: Quirks/Limited-Quirks/No-Quirks mode rules, HTML5 §15.3
//! presentational hints, HTML5 §4.16.3 form pseudo-classes, CSS `appearance` & `accent-color`,
//! Firefox 161.0 Gecko UA stylesheet & intrinsic form sizing, `<dialog>`, Popover API, and
//! `<details>`/`<summary>` exclusive accordions.

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
fn quirks_mode_selectors_hashless_hex_and_unitless_lengths() {
    let mut quirks = load_rt(
        r#"<html><head><style>
            .MyBox { width: 120; color: 123; background-color: abc; }
            #HeroCard { margin-top: 15; border-spacing: calc(1); outline-width: 5; }
            @supports (background-color: 00ff00) { #sup { background-color: 00ff00; } }
        </style></head><body>
            <div class="mybox" id="herocard"></div>
            <div id="sup"></div>
            <div id="sci" style="background-color: 1e1"></div>
        </body></html>"#,
    );
    let out = eval_str(
        &mut quirks,
        r#"
        const el = document.getElementById("herocard");
        const cs = getComputedStyle(el);
        const sup = getComputedStyle(document.getElementById("sup"));
        const sci = getComputedStyle(document.getElementById("sci"));
        JSON.stringify({
            compat: document.compatMode,
            width: cs.width,
            color: cs.color,
            bg: cs.backgroundColor,
            mt: cs.marginTop,
            bs: cs.borderSpacing,
            ow: cs.outlineWidth,
            supBg: sup.backgroundColor,
            cssSup: CSS.supports("background-color", "00ff00"),
            sciBg: sci.backgroundColor
        })
        "#,
    );
    assert!(out.contains(r#""compat":"BackCompat""#), "got {out}");
    assert!(out.contains(r#""width":"120px""#), "got {out}");
    assert!(out.contains(r#""color":"rgb(0, 1, 35)""#), "got {out}");
    assert!(out.contains(r#""bg":"rgb(170, 187, 204)""#), "got {out}");
    assert!(out.contains(r#""mt":"15px""#), "got {out}");
    assert!(
        out.contains(r#""bs":"0px""#),
        "calc(1) must be rejected: {out}"
    );
    assert!(out.contains(r#""supBg":"rgb(0, 255, 0)""#), "got {out}");
    assert!(out.contains(r#""cssSup":false"#), "got {out}");
    assert!(
        out.contains(r#""sciBg":"rgba(0, 0, 0, 0)""#),
        "1e1 must be rejected: {out}"
    );

    let mut std_rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            .MyBox { color: red; }
            #box { width: 120; background-color: abc; border-spacing: 1; }
        </style></head><body><div class="mybox" id="box"></div></body></html>"#,
    );
    let std_out = eval_str(
        &mut std_rt,
        r#"
        const cs = getComputedStyle(document.getElementById("box"));
        JSON.stringify({ compat: document.compatMode, color: cs.color, bg: cs.backgroundColor, bs: cs.borderSpacing })
        "#,
    );
    assert!(
        std_out.contains(r#""compat":"CSS1Compat""#),
        "got {std_out}"
    );
    assert!(
        std_out.contains(r#""color":"rgb(0, 0, 0)""#),
        "got {std_out}"
    );
    assert!(
        std_out.contains(r#""bg":"rgba(0, 0, 0, 0)""#),
        "got {std_out}"
    );
    assert!(std_out.contains(r#""bs":"0px""#), "got {std_out}");
}

#[test]
fn html5_presentational_hints_and_form_pseudo_classes() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><head><style>
            :disabled { opacity: 0.5; }
            :indeterminate { opacity: 0.25; }
            :in-range { color: rgb(0, 128, 0); }
            :out-of-range { color: rgb(255, 0, 0); }
            :placeholder-shown { letter-spacing: 2px; }
        </style></head>
        <body marginwidth="20" marginheight="10">
            <img id="pic" hspace="8" vspace="6" align="right" border="2">
            <font id="fnt" color="#112233" face="monospace" size="5">hi</font>
            <fieldset disabled>
                <legend><input id="in_legend" type="text" value="ok"></legend>
                <input id="in_body" type="text" value="blocked">
            </fieldset>
            <input id="chk" type="checkbox">
            <input id="num_ok" type="number" min="1" max="10" value="5">
            <input id="num_hi" type="number" min="1" max="10" value="99">
            <input id="ph" type="text" placeholder="Search...">
        </body></html>"##,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const chk = document.getElementById("chk");
        chk.indeterminate = true;
        const pic = getComputedStyle(document.getElementById("pic"));
        const body = getComputedStyle(document.body);
        const fnt = getComputedStyle(document.getElementById("fnt"));
        JSON.stringify({
            bodyMl: body.marginLeft,
            bodyMt: body.marginTop,
            picMl: pic.marginLeft,
            picMt: pic.marginTop,
            picFloat: pic.float,
            picBw: pic.borderTopWidth,
            fntColor: fnt.color,
            legendDisabled: document.getElementById("in_legend").matches(":disabled"),
            bodyDisabled: document.getElementById("in_body").matches(":disabled"),
            chkInd: chk.indeterminate && chk.matches(":indeterminate"),
            numOk: document.getElementById("num_ok").matches(":in-range"),
            numHi: document.getElementById("num_hi").matches(":out-of-range"),
            phShown: document.getElementById("ph").matches(":placeholder-shown")
        })
        "#,
    );
    assert!(out.contains(r#""bodyMl":"20px""#), "got {out}");
    assert!(out.contains(r#""bodyMt":"10px""#), "got {out}");
    assert!(out.contains(r#""picMl":"8px""#), "got {out}");
    assert!(out.contains(r#""picMt":"6px""#), "got {out}");
    assert!(out.contains(r#""picFloat":"right""#), "got {out}");
    assert!(out.contains(r#""picBw":"2px""#), "got {out}");
    assert!(out.contains(r#""fntColor":"rgb(17, 34, 51)""#), "got {out}");
    assert!(
        out.contains(r#""legendDisabled":false"#),
        "first legend exempt: {out}"
    );
    assert!(out.contains(r#""bodyDisabled":true"#), "got {out}");
    assert!(out.contains(r#""chkInd":true"#), "got {out}");
    assert!(out.contains(r#""numOk":true"#), "got {out}");
    assert!(out.contains(r#""numHi":true"#), "got {out}");
    assert!(out.contains(r#""phShown":true"#), "got {out}");
}

#[test]
fn appearance_accent_color_and_firefox_form_intrinsic_sizes() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            #wrap { accent-color: rgb(255, 0, 128); }
            #btn { -webkit-appearance: none; }
        </style></head><body>
            <div id="wrap">
                <input id="chk" type="checkbox">
                <input id="rng" type="range">
                <meter id="mtr" value="0.5"></meter>
                <progress id="prg" value="0.5"></progress>
                <button id="btn">Go</button>
            </div>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const chk = document.getElementById("chk");
        const rng = document.getElementById("rng");
        const mtr = document.getElementById("mtr");
        const prg = document.getElementById("prg");
        const btn = document.getElementById("btn");
        JSON.stringify({
            chkAccent: getComputedStyle(chk).accentColor,
            chkApp: getComputedStyle(chk).appearance,
            btnApp: getComputedStyle(btn).appearance,
            chkW: Math.round(chk.getBoundingClientRect().width),
            chkH: Math.round(chk.getBoundingClientRect().height),
            rngW: Math.round(rng.getBoundingClientRect().width),
            mtrW: Math.round(mtr.getBoundingClientRect().width),
            prgW: Math.round(prg.getBoundingClientRect().width)
        })
        "#,
    );
    assert!(
        out.contains(r#""chkAccent":"rgb(255, 0, 128)""#),
        "got {out}"
    );
    assert!(out.contains(r#""chkApp":"auto""#), "got {out}");
    assert!(out.contains(r#""btnApp":"none""#), "got {out}");
    assert!(
        out.contains(r#""chkW":14"#) && out.contains(r#""chkH":14"#),
        "got {out}"
    );
    assert!(out.contains(r#""rngW":160"#), "got {out}");
    assert!(out.contains(r#""mtrW":80"#), "got {out}");
    assert!(out.contains(r#""prgW":162"#), "got {out}");
}

#[test]
fn popover_dialog_and_details_exclusive_accordion_semantics() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><body>
            <button id="inv" popovertarget="p1">Open</button>
            <div id="p1" popover>One</div>
            <div id="p2" popover="auto">Two</div>
            <div id="pm" popover="manual">Manual</div>
            <dialog id="dlg"><button id="dlg_btn">OK</button></dialog>
            <details id="d1" name="acc" open><summary id="s1">One</summary>Body 1</details>
            <details id="d2" name="acc"><summary id="s2">Two</summary>Body 2</details>
        </body></html>"#,
    );
    let out = eval_str(
        &mut rt,
        r#"
        const p1 = document.getElementById("p1");
        const p2 = document.getElementById("p2");
        const pm = document.getElementById("pm");
        const dlg = document.getElementById("dlg");
        const d1 = document.getElementById("d1");
        const d2 = document.getElementById("d2");
        const beforeLog = [];
        p1.addEventListener("beforetoggle", (e) => {
            beforeLog.push(e.oldState + "->" + e.newState + ":" + (e.source ? e.source.id : "null"));
        });
        document.getElementById("inv").click();
        const p1OpenAfterInv = p1.matches(":popover-open");
        pm.showPopover();
        p2.showPopover();
        const p1DismissedByP2 = !p1.matches(":popover-open");
        const pmKeptOpen = pm.matches(":popover-open");
        dlg.showModal();
        const p2DismissedByModal = !p2.matches(":popover-open");
        const dlgFocusedBtn = document.activeElement && document.activeElement.id === "dlg_btn";
        dlg.close("done");
        const rv = dlg.returnValue;
        dlg.close("ignored");
        const rvPreserved = dlg.returnValue === "done";
        document.getElementById("s2").click();
        JSON.stringify({
            p1Attr: p1.popover,
            beforeLog,
            p1OpenAfterInv,
            p1DismissedByP2,
            pmKeptOpen,
            p2DismissedByModal,
            dlgFocusedBtn,
            rv,
            rvPreserved,
            d1Open: d1.open,
            d2Open: d2.open
        })
        "#,
    );
    assert!(out.contains(r#""p1Attr":"auto""#), "got {out}");
    assert!(
        out.contains(r#""beforeLog":["closed->open:inv","open->closed:null"]"#),
        "got {out}"
    );
    assert!(out.contains(r#""p1OpenAfterInv":true"#), "got {out}");
    assert!(out.contains(r#""p1DismissedByP2":true"#), "got {out}");
    assert!(out.contains(r#""pmKeptOpen":true"#), "got {out}");
    assert!(out.contains(r#""p2DismissedByModal":true"#), "got {out}");
    assert!(out.contains(r#""dlgFocusedBtn":true"#), "got {out}");
    assert!(out.contains(r#""rv":"done""#), "got {out}");
    assert!(out.contains(r#""rvPreserved":true"#), "got {out}");
    assert!(
        out.contains(r#""d1Open":false"#) && out.contains(r#""d2Open":true"#),
        "got {out}"
    );
}
