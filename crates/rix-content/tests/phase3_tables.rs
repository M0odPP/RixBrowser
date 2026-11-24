//! Phase 3 Part 4 specification regression tests for W3C CSS Tables Level 3:
//! - Inherited table properties (`border-collapse`, `border-spacing`, `empty-cells`, `caption-side`)
//! - Negative `border-spacing` parse rejection
//! - HTML §15.3.9 table presentational hint mapping (`border`, `rules`, `frame`, `cellpadding`)
//! - CSS Tables L3 §2.2 Box Tree Fixup (`table-column`/`table-column-group` child suppression,
//!   replaced element display suppression, whitespace preservation inside anonymous cells)

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
fn table_properties_inherit_and_reject_negative_border_spacing() {
    let html = r#"<!doctype html>
    <html><body>
      <div id="p" style="border-collapse: collapse; border-spacing: 10px 20px; empty-cells: hide; caption-side: bottom; table-layout: fixed;">
        <div id="c"></div>
      </div>
      <div id="neg" style="border-spacing: -20px;"></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const cs = getComputedStyle(document.getElementById('c'));
          const ns = getComputedStyle(document.getElementById('neg'));
          return [cs.borderCollapse, cs.borderSpacing, cs.emptyCells, cs.captionSide, cs.tableLayout, ns.borderSpacing].join('|');
        })()"#,
    );
    assert_eq!(res, "collapse|10px 20px|hide|bottom|auto|0px");
}

#[test]
fn html_table_border_rules_frame_mapping_matches_spec() {
    let html = r#"<!doctype html>
    <html><body>
      <table id="t1" border="0" rules="groups" style="border-collapse: separate"><tr><td></td></tr></table>
      <table id="t2" border="0" rules="groups" frame="box" style="border-collapse: separate"><tr><td></td></tr></table>
      <table id="t3" border="10" rules="groups" frame="box" style="border-collapse: separate"><tr><td id="td3"></td></tr></table>
      <table id="t4" border="yes" rules="all" style="border-collapse: separate"><tr><td id="td4"></td></tr></table>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const fmt = id => {
            const s = getComputedStyle(document.getElementById(id));
            return `${s.borderLeftWidth}/${s.borderLeftStyle}/${s.borderLeftColor}`;
          };
          return [fmt('t1'), fmt('t2'), fmt('t3'), fmt('td3'), fmt('t4'), fmt('td4')].join(' | ');
        })()"#,
    );
    assert_eq!(
        res,
        "0px/hidden/rgb(128, 128, 128) | 1px/outset/rgb(0, 0, 0) | 10px/outset/rgb(128, 128, 128) | 1px/inset/rgb(128, 128, 128) | 1px/outset/rgb(128, 128, 128) | 1px/solid/rgb(128, 128, 128)"
    );
}

#[test]
fn table_box_tree_fixup_suppresses_col_children_and_replaced_table_display() {
    let html = r#"<!doctype html>
    <html><body>
      <div style="display:table-column"><div id="c1" style="width:100px;height:100px;"></div></div>
      <div style="display:table-column-group"><div id="c2" style="width:100px;height:100px;"></div></div>
      <div><input id="i1" style="display:table;width:100px" /><input id="i2" style="display:table;width:100px" /></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const w1 = document.getElementById('c1').offsetWidth;
          const w2 = document.getElementById('c2').offsetWidth;
          const r1 = document.getElementById('i1').getBoundingClientRect();
          const r2 = document.getElementById('i2').getBoundingClientRect();
          return `${w1}|${w2}|${r2.top > r1.top}`;
        })()"#,
    );
    assert_eq!(res, "0|0|true");
}

#[test]
fn table_auto_and_fixed_column_width_distribution_matches_css_tables_l3() {
    let html = r#"<!doctype html>
    <html><head><style>
      table { border-collapse: collapse; }
      td { padding: 0; }
    </style></head><body>
      <table id="t-auto" style="width:400px">
        <tr>
          <td id="a1">&nbsp;</td>
          <td id="a2" style="width:50px; min-width:150px">&nbsp;</td>
        </tr>
      </table>
      <table id="t-fixed" style="table-layout:fixed; width:300px">
        <tr>
          <td id="f1" style="width:20px"></td>
          <td id="f2" style="width:10px"></td>
          <td id="f3" style="width:10%"></td>
        </tr>
      </table>
      <table id="t-calc" style="width:200px">
        <col style="width:calc(20% + 80px)">
        <tr><td id="c1"></td><td id="c2"></td></tr>
      </table>
      <table id="t-merge" style="width:400px; border-collapse:separate; border:10px solid gray; border-spacing:20px;">
        <tr><td id="m1" colspan="10" style="width:50px"></td><td id="m2" style="width:50px"></td></tr>
      </table>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const w = id => document.getElementById(id).offsetWidth;
          return [w('a1'), w('a2'), w('f1'), w('f2'), w('f3'), w('c1'), w('c2'), w('m1'), w('m2')].join('|');
        })()"#,
    );
    assert_eq!(res, "250|150|180|90|30|100|100|270|50");
}

#[test]
fn table_section_ordering_rowspan_zero_and_tbody_percentage_height() {
    let html = r#"<!doctype html>
    <html><head><style>
      table { border-collapse: collapse; }
      td { padding: 0; }
    </style></head><body>
      <table id="t-order">
        <tfoot><tr id="rf"><td><div style="height:20px"></div></td></tr></tfoot>
        <tbody><tr id="rb"><td><div style="height:30px"></div></td></tr></tbody>
        <thead><tr id="rh"><td><div style="height:10px"></div></td></tr></thead>
      </table>
      <table id="t-pct" style="height:100px; width:100px">
        <tbody id="b1" style="height:40%"><tr><td>x</td></tr></tbody>
        <tbody id="b2"><tr><td>x</td></tr></tbody>
      </table>
      <table id="t-rs0">
        <tbody>
          <tr><td id="rs0" rowspan="0">span</td><td><div style="height:25px"></div></td></tr>
          <tr><td><div style="height:35px"></div></td></tr>
          <tr><td><div style="height:40px"></div></td></tr>
        </tbody>
      </table>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const top = id => Math.round(document.getElementById(id).getBoundingClientRect().top);
          const h = id => document.getElementById(id).offsetHeight;
          const ordered = top('rh') < top('rb') && top('rb') < top('rf');
          return `${ordered}|${h('b1')}|${h('b2')}|${h('rs0')}`;
        })()"#,
    );
    assert_eq!(res, "true|40|60|100");
}

#[test]
fn table_visibility_collapse_preserves_orthogonal_dimension() {
    let html = r#"<!doctype html>
    <html><head><style>
      table { border-collapse: collapse; }
      td { padding: 0; }
    </style></head><body>
      <table id="t-ref">
        <tr><td><div style="width:80px;height:20px"></div></td><td><div style="width:60px;height:20px"></div></td></tr>
        <tr><td><div style="width:80px;height:30px"></div></td><td><div style="width:60px;height:30px"></div></td></tr>
      </table>
      <table id="t-row-col">
        <tr><td><div style="width:80px;height:20px"></div></td><td><div style="width:60px;height:20px"></div></td></tr>
        <tr id="r-col" style="visibility:collapse"><td><div style="width:80px;height:30px"></div></td><td><div style="width:60px;height:30px"></div></td></tr>
      </table>
      <table id="t-col-col">
        <col><col style="visibility:collapse">
        <tr><td><div style="width:80px;height:20px"></div></td><td><div style="width:60px;height:20px"></div></td></tr>
        <tr><td><div style="width:80px;height:30px"></div></td><td><div style="width:60px;height:30px"></div></td></tr>
      </table>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const ref = document.getElementById('t-ref');
          const trc = document.getElementById('t-row-col');
          const tcc = document.getElementById('t-col-col');
          const rcol = document.getElementById('r-col');
          return [
            trc.offsetWidth === ref.offsetWidth,
            trc.offsetHeight,
            rcol.offsetHeight,
            tcc.offsetHeight === ref.offsetHeight,
            tcc.offsetWidth,
          ].join('|');
        })()"#,
    );
    assert_eq!(res, "true|20|0|true|80");
}

#[test]
fn table_multiple_captions_and_vertical_align_baseline_mapping() {
    let html = r#"<!doctype html>
    <html><head><style>
      table { border-collapse: collapse; position: relative; }
      td { padding: 0; }
    </style></head><body>
      <table id="t-cap">
        <caption id="c-top1" style="line-height:20px">top1</caption>
        <caption id="c-top2" style="line-height:20px">top2</caption>
        <caption id="c-bot1" style="caption-side:bottom;line-height:20px">bot1</caption>
        <tr><td id="cell" style="line-height:30px">body</td></tr>
      </table>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const y = id => document.getElementById(id).offsetTop;
          return [y('c-top1'), y('c-top2'), y('cell'), y('c-bot1')].join('|');
        })()"#,
    );
    assert_eq!(res, "0|20|40|70");
}
