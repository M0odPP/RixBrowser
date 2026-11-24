//! W3C CSS Grid Layout Level 1 & CSS Box Alignment Level 3 spec regression tests.
//!
//! Exercises track expansion, auto-placement, §11 intrinsic & flexible track sizing,
//! legacy `grid-gap` aliases, `fit-content()`, §6.6 automatic minimum sizing,
//! §9 out-of-flow grid-area containing blocks, vertical writing modes, and
//! §7.2.6 `getComputedStyle().gridTemplateColumns/Rows`.

#![deny(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn run_script(html: &str, js: &str) -> String {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    let mut rt = ContentRuntime::new(doc, Size::new(800.0, 600.0), font_db);
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "unexpected load errors: {errs:?}");
    let val = rt.eval_script(js).expect("eval_script");
    rt.vm.value_to_rust_string(val)
}

#[test]
fn grid_track_sizing_fr_and_legacy_grid_gap() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g {
        display: grid;
        width: 260px;
        grid-template-columns: 40px 1fr 2fr;
        grid-template-rows: 30px 50px;
        grid-gap: 10px 20px;
      }
    </style>
    <div id="g">
      <div id="a"></div><div id="b"></div><div id="c"></div>
      <div id="d"></div><div id="e"></div><div id="f"></div>
    </div>"#;
    let res = run_script(
        html,
        r#"
        const g = document.getElementById('g');
        const cs = getComputedStyle(g);
        const c = document.getElementById('c');
        const d = document.getElementById('d');
        [cs.gridTemplateColumns, cs.gridTemplateRows, c.offsetLeft, c.offsetWidth, d.offsetTop].join('|')
        "#,
    );
    assert_eq!(res, "40px 60px 120px|30px 50px|140|120|40");
}

#[test]
fn grid_auto_fit_collapses_empty_tracks_and_abspos_grid_area() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g {
        position: relative;
        display: grid;
        width: 200px;
        grid: 20px / repeat(auto-fit, 30px);
      }
      #abs {
        position: absolute;
        top: 0; right: 0; bottom: 0; left: 0;
        grid-column: 1 / 5;
      }
    </style>
    <div id="g">
      <div id="abs"></div>
      <div style="grid-column: 1"></div>
    </div>"#;
    let res = run_script(
        html,
        r#"
        const a = document.getElementById('abs');
        const cs = getComputedStyle(document.getElementById('g'));
        [a.offsetWidth, a.offsetHeight, cs.gridTemplateColumns].join('|')
        "#,
    );
    assert_eq!(res, "30|20|30px 0px 0px 0px 0px 0px");
}

#[test]
fn grid_definite_min_contribution_not_clamped_by_minmax_auto_zero() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g {
        display: grid;
        width: 100px;
        grid-template-columns: minmax(auto, 0px);
        grid-template-rows: 20px 20px;
      }
      #i1 { width: 60px; min-width: 20px; }
    </style>
    <div id="g">
      <div id="i1"></div>
      <div id="i2"></div>
    </div>"#;
    let res = run_script(
        html,
        r#"
        const cs = getComputedStyle(document.getElementById('g'));
        const i2 = document.getElementById('i2');
        [cs.gridTemplateColumns, i2.offsetWidth].join('|')
        "#,
    );
    assert_eq!(res, "60px|60");
}

#[test]
fn grid_fit_content_clamps_growth_limit_and_respects_auto_min() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g1 { display: inline-grid; grid-template-columns: fit-content(50px); }
      #g2 { display: inline-grid; grid-template-columns: fit-content(50px); }
      .big { width: 80px; height: 10px; }
    </style>
    <div id="g1"><div style="min-width: 0px;"><div class="big"></div></div></div>
    <div id="g2"><div><div class="big"></div></div></div>"#;
    let res = run_script(
        html,
        r#"
        const c1 = getComputedStyle(document.getElementById('g1')).gridTemplateColumns;
        const c2 = getComputedStyle(document.getElementById('g2')).gridTemplateColumns;
        c1 + '|' + c2
        "#,
    );
    assert_eq!(res, "50px|80px");
}

#[test]
fn grid_vertical_writing_mode_inline_percentage_margins() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g {
        display: grid;
        writing-mode: vertical-lr;
        grid-template-columns: 100px;
        height: 400px;
        justify-items: start;
        position: relative;
      }
      #i1 { width: 10px; height: 10px; margin-left: 50%; }
      #i2 { width: 10px; height: 100%; }
    </style>
    <div id="g"><div id="i1"></div><div id="i2"></div></div>"#;
    let res = run_script(
        html,
        r#"
        const i1 = document.getElementById('i1');
        const i2 = document.getElementById('i2');
        [i1.offsetLeft, i2.offsetLeft, i2.offsetHeight].join('|')
        "#,
    );
    assert_eq!(res, "50|60|100");
}

#[test]
fn grid_template_areas_uses_auto_tracks_and_abspos_non_cb_ignores_grid_lines() {
    let html = r#"<!doctype html>
    <style>
      body { margin: 0; }
      #g1 {
        display: grid;
        grid-template-areas: "a b";
        grid-auto-columns: 15px;
        grid-auto-rows: 25px;
      }
      #wrap { position: relative; padding-left: 40px; }
      #g2 { display: grid; grid-template-columns: 50px 50px; }
      #abs { position: absolute; grid-column: 2; width: 20px; height: 20px; }
    </style>
    <div id="g1"><div id="c1"></div><div id="c2"></div></div>
    <div id="wrap"><div id="g2"><div id="abs"></div></div></div>"#;
    let res = run_script(
        html,
        r#"
        const c1 = document.getElementById('c1');
        const c2 = document.getElementById('c2');
        const abs = document.getElementById('abs');
        [c1.offsetWidth, c2.offsetLeft, c2.offsetWidth, abs.offsetLeft].join('|')
        "#,
    );
    assert_eq!(res, "15|15|15|40");
}
