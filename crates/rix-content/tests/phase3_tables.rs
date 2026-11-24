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
