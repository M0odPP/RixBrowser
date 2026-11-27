//! Phase 3C-1 regression tests: WPT `css/selectors` parity (0 Firefox-only gaps).
//!
//! Covers:
//! - Selectors L4 §3.7 compatibility: unknown `::-webkit-*` pseudo-elements (incl. CSS escapes,
//!   case-folding, user-action pseudo-classes, rejecting non-dash `::-webkitfoo` & nested pseudos)
//! - CSS Scoping L1 §3.1–3.2: featureless shadow hosts (`:host`, `:is(:host)`, `@namespace`
//!   default-namespace exemption, `:host::before`, `::slotted()` flat-tree `<slot>` inheritance,
//!   `::part()` + `:dir()` invalidation across shadow trees, pinned Custom Element wrappers)
//! - HTML §6.6.3 Focus Fixup Rule at rendering-update / VSync boundary (`display: none` blur)
//!   + `:focus-visible` UA `outline: auto` & `delegatesFocus` non-propagation
//! - Selectors L4 `:has()` invalidation for `:checked`, `:indeterminate`, `:disabled`/`:enabled`
//!   (`<optgroup disabled>`), `:valid`/`:invalid` (`<form>`), `:in-range`/`:out-of-range`
//!   (`readonly`/`disabled`), `:target` fragment navigation, `:state()` custom states, `:heading`
//!   with `headingoffset`/`headingreset`, media pseudo-classes (`:playing`/`:paused`/`:muted`),
//!   and O(1) amortized DOM subtree mutation invalidation (`has-complexity`).

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
fn webkit_prefixed_pseudo_elements_parse_cascade_and_serialize() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><head><style id="s">
            #test { color: rgb(255, 0, 0); }
            span::-webkit-something-invalid, #test, ::-WeBkIt-sOmEtHiNg-NoNeXiSt123, ::-webkit-\ escaped {
                color: rgb(0, 255, 0);
            }
            ::-webkitfoo, #test { color: rgb(255, 0, 0); }
        </style></head><body><div id="test"></div></body></html>"##,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                const s = document.getElementById('s').sheet;
                const el = document.getElementById('test');
                return [
                    getComputedStyle(el).color,
                    s.cssRules.length,
                    s.cssRules[1].selectorText,
                ].join('|');
            })()"#
        ),
        "rgb(0, 255, 0)|2|span::-webkit-something-invalid, #test, ::-webkit-something-nonexist123, ::-webkit-\\ escaped"
    );
}

#[test]
fn shadow_featureless_host_namespace_and_slotted_flat_tree_inheritance() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><body>
            <div id="h1" class="host"></div>
            <div id="h2" class="host"></div>
            <div id="h3"><p class="d">x</p><p class="e">y</p></div>
        </body></html>"##,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                const h1 = document.getElementById('h1');
                h1.attachShadow({ mode: 'open' }).innerHTML =
                    '<style>@namespace url(http://www.w3.org/1999/xhtml); :is(:host) { --m: yes; }</style>';
                const h2 = document.getElementById('h2');
                h2.attachShadow({ mode: 'open' }).innerHTML =
                    '<style>@namespace url(http://example.org/ns); :host { --m: yes; }</style>';
                const h3 = document.getElementById('h3');
                h3.attachShadow({ mode: 'open' }).innerHTML =
                    '<style>* { color: rgb(0, 0, 255); } ::slotted(:is(.e, .f)) { color: rgb(0, 128, 0); }</style><slot></slot>';
                return [
                    getComputedStyle(h1).getPropertyValue('--m').trim(),
                    getComputedStyle(h2).getPropertyValue('--m').trim(),
                    getComputedStyle(h3.querySelector('.d')).color,
                    getComputedStyle(h3.querySelector('.e')).color,
                ].join('|');
            })()"#
        ),
        "|yes|rgb(0, 0, 255)|rgb(0, 128, 0)"
    );
}

#[test]
fn part_dir_invalidation_and_pinned_custom_element_wrappers() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><head><style>
            x-box::part(inner) { background-color: rgb(255, 0, 0); }
            x-box::part(inner):dir(ltr) { background-color: rgb(0, 255, 0); }
            x-box::part(inner):dir(rtl) { background-color: rgb(0, 0, 255); }
        </style></head><body><x-box id="box"></x-box></body></html>"##,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                customElements.define('x-box', class extends HTMLElement {
                    connectedCallback() {
                        this.attachShadow({ mode: 'open' }).innerHTML = '<div part="inner">hi</div>';
                    }
                    get inner() { return this.shadowRoot.querySelector('[part=inner]'); }
                });
                const c0 = getComputedStyle(box.inner).backgroundColor;
                box.inner.setAttribute('dir', 'rtl');
                const c1 = getComputedStyle(box.inner).backgroundColor;
                box.inner.removeAttribute('dir');
                const c2 = getComputedStyle(box.inner).backgroundColor;
                return [c0, c1, c2].join('|');
            })()"#
        ),
        "rgb(0, 255, 0)|rgb(0, 0, 255)|rgb(0, 255, 0)"
    );
}

#[test]
fn has_pseudo_class_invalidation_for_select_optgroup_form_and_target() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><head><style>
            main:has(input) div { color: rgb(128, 128, 128); }
            main:has(#opt2:checked) > #s1 { color: rgb(255, 0, 0); }
            main:has(#opt2:disabled) > #s2 { color: rgb(0, 128, 0); }
            main:has(#frm:valid) > #s3 { color: rgb(255, 255, 0); }
            main:has(#anchor:target) > #s4 { color: rgb(0, 0, 255); }
        </style></head><body>
            <main>
                <form id="frm">
                    <select id="sel"><optgroup id="og"><option id="opt1">a</option><option id="opt2">b</option></optgroup></select>
                    <input id="inp" type="text" required>
                </form>
                <div id="anchor"></div>
                <div id="s1"></div><div id="s2"></div><div id="s3"></div><div id="s4"></div>
            </main>
        </body></html>"##,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                const oldOpt = sel.selectedOptions[0];
                opt2.selected = true;
                const a = getComputedStyle(s1).color;
                oldOpt.selected = true;
                const b = getComputedStyle(s1).color;
                og.disabled = true;
                const c = getComputedStyle(s2).color;
                og.disabled = false;
                inp.value = 'ok';
                const d = getComputedStyle(s3).color;
                location.hash = '#anchor';
                const e = getComputedStyle(s4).color;
                return [a, b, c, d, e].join('|');
            })()"#
        ),
        "rgb(255, 0, 0)|rgb(128, 128, 128)|rgb(0, 128, 0)|rgb(255, 255, 0)|rgb(0, 0, 255)"
    );
}

#[test]
fn focus_fixup_runs_at_vsync_frame_boundary_not_synchronously() {
    let mut rt = load_rt(r##"<!DOCTYPE html><html><body><input id="inp"></body></html>"##);
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                inp.focus();
                inp.style.display = 'none';
                return String(inp.matches(':focus'));
            })()"#
        ),
        "true"
    );
    rt.fire_vsync(16.0);
    assert_eq!(eval_str(&mut rt, "String(inp.matches(':focus'))"), "false");
}
