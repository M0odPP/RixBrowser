//! Phase 3 Part 5A specification regression tests for W3C CSS Text L3 / L4 / L5:
//! - `white-space` L4 shorthand (`white-space-collapse`, `text-wrap-mode`, `white-space-trim`)
//!   preserving `text-wrap-style`
//! - `text-wrap` and `text-spacing` shorthands and strict grammar rejection
//! - `word-wrap` legacy name alias for `overflow-wrap` (including `var()` values)
//! - `text-align: match-parent` resolution under LTR and RTL
//! - `text-indent` (`hanging` / `each-line`), `letter-spacing`, `word-spacing`, `text-justify`
//! - Inherited vs non-inherited CSS Text L4/L5 properties and animation interpolation

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
fn white_space_l4_shorthand_preserves_text_wrap_style() {
    let html = r#"<!doctype html>
    <html><body>
      <div id="a" style="text-wrap-style: balance; white-space: pre-wrap;"></div>
      <div id="b" style="white-space: preserve nowrap discard-after discard-before;"></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const ca = getComputedStyle(document.getElementById('a'));
          const cb = getComputedStyle(document.getElementById('b'));
          return [
            ca.whiteSpace, ca.whiteSpaceCollapse, ca.textWrapMode, ca.textWrapStyle, ca.whiteSpaceTrim,
            cb.whiteSpace, cb.whiteSpaceCollapse, cb.textWrapMode, cb.whiteSpaceTrim
          ].join('|');
        })()"#,
    );
    assert_eq!(
        res,
        "pre-wrap|preserve|wrap|balance|none|preserve nowrap discard-before discard-after|preserve|nowrap|discard-before discard-after"
    );
}

#[test]
fn text_wrap_text_spacing_and_word_wrap_alias_conform() {
    let html = r#"<!doctype html>
    <html><body>
      <div id="t"></div>
      <div id="w" style="word-wrap: break-word; overflow-wrap: break-word;"></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const t = document.getElementById('t');
          t.style.textWrap = 'pretty nowrap';
          const tw = t.style.textWrap + ',' + t.style.textWrapMode + ',' + t.style.textWrapStyle;
          t.style.textSpacing = 'auto auto';
          const badTs = t.style.textSpacing;
          t.style.textSpacing = 'no-autospace space-all';
          const goodTs = t.style.textSpacing + ',' + t.style.textSpacingTrim + ',' + t.style.textAutospace;
          const w = document.getElementById('w');
          const cssText = w.style.cssText;
          w.style.wordWrap = 'var(--v)';
          const aliasVar = w.style.overflowWrap;
          return [tw, badTs, goodTs, cssText, aliasVar].join('|');
        })()"#,
    );
    assert_eq!(
        res,
        "nowrap pretty,nowrap,pretty||none,space-all,no-autospace|overflow-wrap: break-word;|var(--v)"
    );
}

#[test]
fn text_align_match_parent_and_spacing_computed_values() {
    let html = r#"<!doctype html>
    <html><body>
      <div style="direction: rtl; text-align: start;">
        <div id="rtlChild" style="text-align: match-parent;"></div>
      </div>
      <div id="sp" style="font-size: 40px; letter-spacing: 0px; word-spacing: normal; text-indent: each-line hanging calc(10px + 0.5em); text-justify: distribute;"></div>
      <div id="pct" style="font-size: 40px; letter-spacing: calc(10px - (5% + 10%)); word-spacing: 110%;"></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const r = getComputedStyle(document.getElementById('rtlChild'));
          const s = getComputedStyle(document.getElementById('sp'));
          const p = getComputedStyle(document.getElementById('pct'));
          return [
            r.textAlign, s.letterSpacing, s.wordSpacing, s.textIndent, s.textJustify,
            p.letterSpacing, p.wordSpacing
          ].join('|');
        })()"#,
    );
    assert_eq!(
        res,
        "right|normal|0px|30px hanging each-line|inter-character|calc(-15% + 10px)|110%"
    );
}

#[test]
fn css_text_l4_inheritance_and_non_inheritance_rules() {
    let html = r#"<!doctype html>
    <html><body>
      <div id="p" style="white-space-trim: discard-before; wrap-inside: avoid; text-group-align: center; hyphens: auto; line-break: strict; hyphenate-limit-chars: 6 3; text-autospace: ideograph-alpha; text-autospace-size: 0.25; hanging-punctuation: allow-end first; text-fit: grow per-line 150%; word-space-transform: space auto-phrase;">
        <div id="c"></div>
      </div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const c = getComputedStyle(document.getElementById('c'));
          return [
            c.whiteSpaceTrim, c.wrapInside, c.textGroupAlign,
            c.hyphens, c.lineBreak, c.hyphenateLimitChars,
            c.textAutospace, c.textAutospaceSize, c.hangingPunctuation,
            c.textFit, c.wordSpaceTransform
          ].join('|');
        })()"#,
    );
    assert_eq!(
        res,
        "none|auto|none|auto|strict|6 3|ideograph-alpha|0.25|first allow-end|grow per-line 150%|space auto-phrase"
    );
}

#[test]
fn selection_and_inner_text_apply_math_auto_kana_german_georgian_and_capitalize() {
    let html = r#"<!doctype html>
    <html><body>
      <span id="m" style="text-transform: math-auto;">hA</span>
      <span id="k" style="text-transform: full-size-kana;">ぁ&#x1B167;&#xFF6F;</span>
      <span id="de" lang="de" style="text-transform: uppercase;">&#x00DF;</span>
      <div id="ka1" lang="ka" style="text-transform: uppercase;">ა</div>
      <div id="ka2" lang="ka" style="text-transform: capitalize;">ა ბ</div>
      <div id="cap1" style="text-transform: capitalize;">foo-bar john's foo_bar</div>
      <div>a<span id="cap2" style="text-transform: capitalize;">b</span>c</div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const sel = window.getSelection();
          const m = document.getElementById('m');
          sel.setBaseAndExtent(m.firstChild, 0, m.firstChild, 2);
          const sm = sel.toString();
          const k = document.getElementById('k');
          sel.setBaseAndExtent(k.firstChild, 0, k.firstChild, k.firstChild.data.length);
          const sk = sel.toString();
          const de = document.getElementById('de');
          sel.setBaseAndExtent(de, 0, de, 1);
          const sde = sel.toString();
          return [
            sm === '\u210E\u{1D434}',
            sk === '\u3042\u30F3\uFF82',
            sde,
            document.getElementById('ka1').innerText === 'Ა',
            document.getElementById('ka2').innerText === 'ა ბ',
            document.getElementById('cap1').innerText,
            document.getElementById('cap2').innerText
          ].join('|');
        })()"#,
    );
    assert_eq!(res, "true|true|SS|true|true|Foo-Bar John's Foo_bar|b");
}

#[test]
fn uax14_line_break_strictness_and_atomic_nowrap_min_content() {
    let html = r#"<!doctype html>
    <html lang="en"><body>
      <div id="strict" lang="" style="font-size: 30px; width: 105px; line-height: 1em; line-break: strict;">文文文文文文&#x3005;字<span id="sStrict">字</span></div>
      <div id="loose" lang="" style="font-size: 30px; width: 105px; line-height: 1em; line-break: loose;">文文文文文文&#x3005;字<span id="sLoose">字</span></div>
      <div id="danda" style="font-family: monospace; font-size: 20px; width: 30ch; line-height: 24px;">aaaaaaaaaaaaaaaaaaaaaaaaa&#x0964;bbbbbbbbbbbbbbbbbb</div>
      <div style="width: 0;"><span id="atom" style="display: inline-block; text-wrap-mode: nowrap; width: min-content;"><span style="display:inline-block;width:50px;height:10px"></span><span style="display:inline-block;width:50px;height:10px"></span></span></div>
    </body></html>"#;
    let res = run_script(
        html,
        r#"(() => {
          const xs = document.getElementById('sStrict').getBoundingClientRect().left;
          const xl = document.getElementById('sLoose').getBoundingClientRect().left;
          const dh = document.getElementById('danda').offsetHeight;
          const aw = document.getElementById('atom').clientWidth;
          return [xs > xl + 10, dh > 30, aw].join('|');
        })()"#,
    );
    assert_eq!(res, "true|true|100");
}
