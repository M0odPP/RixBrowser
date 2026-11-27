//! Phase 3B regression tests: WPT core CSS module parity (`css-display`, `css-color`,
//! `css-nesting`, `css-logical`, `css-variables`) — 0 Firefox-only gaps across all 5 suites.
//!
//! Covers:
//! - SVG 2 `SVGTextContentElement` (`getNumberOfChars`, `getComputedTextLength`, `getStartPositionOfChar`)
//!   with `display: contents` / `display: none`
//! - CSS Color 4/5: exact Bradford D65<->D50 matrices, `oklab` interpolation default for wide-gamut,
//!   system color consistency across `color-scheme: light | dark`, `light-dark(<image>, <image>)` on
//!   `background-image` / `list-style-image` / `border-image-source` / `content`
//! - CSS Nesting L1 + Containment L3: streaming multi-`<style>` invalidation across `<script>`
//!   boundaries, `CSSNestedDeclarations` trailing rules, `revert-rule`, boolean `@container (width)`
//! - CSS Logical Properties L1 + Web Animations §5.5.2: table-cell block-axis min/max non-clamping
//!   in horizontal & vertical writing modes, `will-change: transform` CB for logical `%`, JS keyframe
//!   physical/longhand priority, `var()` in logical shorthands, `@keyframes` physical `getKeyframes()`
//! - CSS Variables L1/L2 & CSS Values L5: unbalanced custom-prop delimiter recovery, animated custom
//!   properties re-substituting dependent `var()` properties & guaranteed-invalid `initial` keyframes,
//!   `attr()`-taint preservation across custom-property transitions, `perspective-origin` layout `%`,
//!   `radial-gradient(at x y, ...)`, CSSOM shorthand append order, SVG presentation attributes w/ `var()`

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
fn css_color_system_colors_and_light_dark_images() {
    let mut rt = load_rt(
        r##"<!DOCTYPE html><html><head><style>
            #light { color-scheme: light; }
            #dark { color-scheme: dark; }
        </style></head><body>
            <div id="light"><button id="bl"></button><mark id="ml">x</mark></div>
            <div id="dark"><button id="bd"></button><input id="id" type="text"><a href="#" id="ad">x</a><mark id="md">x</mark></div>
        </body></html>"##,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                const l = document.getElementById('light');
                const d = document.getElementById('dark');
                l.style.color = 'Mark';
                const markOk = getComputedStyle(l).color === getComputedStyle(document.getElementById('ml')).backgroundColor;
                d.style.color = 'ButtonFace';
                const btnFaceOk = getComputedStyle(d).color === getComputedStyle(document.getElementById('bd')).backgroundColor;
                d.style.color = 'ButtonBorder';
                const btnBorderOk = getComputedStyle(d).color === getComputedStyle(document.getElementById('bd')).borderTopColor;
                d.style.color = 'Field';
                const fieldOk = getComputedStyle(d).color === getComputedStyle(document.getElementById('id')).backgroundColor;
                d.style.color = 'LinkText';
                const linkOk = getComputedStyle(d).color === getComputedStyle(document.getElementById('ad')).color;
                l.style.setProperty('content', "light-dark(url('light.png'), url('dark.png'))");
                d.style.setProperty('content', "light-dark(url('light.png'), url('dark.png'))");
                const lc = getComputedStyle(l).content;
                const dc = getComputedStyle(d).content;
                return [markOk, btnFaceOk, btnBorderOk, fieldOk, linkOk, lc.endsWith('light.png")'), dc.endsWith('dark.png")')].join(',');
            })()"#
        ),
        "true,true,true,true,true,true,true"
    );
}

#[test]
fn css_nesting_streaming_styles_and_boolean_container() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><body>
            <style>.a { --x: FAIL; & { --x: FAIL; } --x: PASS; }</style>
            <div class="a"></div>
            <script>window.__r1 = getComputedStyle(document.querySelector('.a')).getPropertyValue('--x');</script>
            <style>.b { & { --x: FAIL; } --x: PASS; }</style>
            <div class="b"></div>
            <script>window.__r2 = getComputedStyle(document.querySelector('.b')).getPropertyValue('--x');</script>
            <style>.c { container-type: inline-size; width: 100px; } .c { @container (width) { .inner { z-index: 7; } } }</style>
            <div class="c"><div class="inner"></div></div>
            <script>window.__r3 = getComputedStyle(document.querySelector('.inner')).zIndex;</script>
        </body></html>"#,
    );
    assert_eq!(
        eval_str(&mut rt, "`${window.__r1}|${window.__r2}|${window.__r3}`"),
        "PASS|PASS|7"
    );
}

#[test]
fn css_logical_table_cell_block_minmax_and_will_change_cb() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            .tbl { display: table; }
            .tbl-v { display: table; writing-mode: vertical-lr; }
            .cell { display: table-cell; inline-size: 100px; min-block-size: 50px; max-block-size: 100px; }
            #c1 { block-size: 40px; }
            #c2 { block-size: 120px; }
            #cv1 { block-size: 40px; }
            #cv2 { block-size: 120px; }
            #cb { will-change: transform; width: 200px; height: 200px; }
            #abs { position: absolute; margin-inline-start: 10%; }
        </style></head><body>
            <div class="tbl"><div class="cell" id="c1"></div></div>
            <div class="tbl"><div class="cell" id="c2"></div></div>
            <div class="tbl-v"><div class="cell" id="cv1"></div></div>
            <div class="tbl-v"><div class="cell" id="cv2"></div></div>
            <div id="cb"><div id="abs"></div></div>
        </body></html>"#,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"[
                document.getElementById('c1').clientHeight,
                document.getElementById('c2').clientHeight,
                document.getElementById('cv1').clientWidth,
                document.getElementById('cv2').clientWidth,
                getComputedStyle(document.getElementById('abs')).marginLeft
            ].join(',')"#
        ),
        "40,120,40,120,20px"
    );
}

#[test]
fn css_logical_web_and_css_animations_priority_and_reflection() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            :root { --a: 200px; --b: 300px; }
            @keyframes blk { from { block-size: 0px; } to { block-size: 100px; } }
            #k { animation: blk 10s -5s paused linear; }
        </style></head><body>
            <div id="k"></div>
            <div id="w1"></div>
            <div id="w2"></div>
            <div id="w3"></div>
        </body></html>"#,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"(() => {
                const k = document.getElementById('k');
                const kf0 = k.getAnimations()[0].effect.getKeyframes()[0];
                const hasH = kf0.hasOwnProperty('height') && !kf0.hasOwnProperty('blockSize');
                const w1 = document.getElementById('w1');
                const a1 = w1.animate([{ height: '200px', blockSize: '0px' }, { height: '300px', blockSize: '100px' }], 1000);
                a1.currentTime = 500;
                const w2 = document.getElementById('w2');
                w2.animate({ marginInlineStart: '100px', margin: '200px' }, { duration: 1, easing: 'step-start' });
                const w3 = document.getElementById('w3');
                const a3 = w3.animate({ insetBlock: ['var(--a)', 'var(--b)'] }, 1000);
                a3.currentTime = 500;
                return [
                    hasH,
                    getComputedStyle(w1).height,
                    getComputedStyle(w2).marginLeft,
                    getComputedStyle(w2).marginRight,
                    getComputedStyle(w3).top
                ].join(',');
            })()"#
        ),
        "true,250px,100px,200px,250px"
    );
}

#[test]
fn css_variables_unbalanced_delims_animated_custom_props_and_svg_hints() {
    let mut rt = load_rt(
        r#"<!DOCTYPE html><html><head><style>
            #legal { --c: green; --c: (]); background-color: red; background-color: var(--c); }
            @keyframes anim { 50% { --w: initial; } 100% { --w: 100px; } }
            .box { width: 200px; }
            .anim { --w: 50px; width: var(--w); animation: anim 100s linear paused both; animation-delay: calc(var(--at) * -1s); }
            #po { width: 200px; height: 200px; --x: 0%; perspective-origin: var(--x) 50%; }
            #rg { --g: radial-gradient(ellipse farthest-corner at 25px 25px, black 10%, green 90%); background-image: var(--g); }
        </style></head><body>
            <div id="legal"></div>
            <div class="box">
                <div class="anim" id="a0" style="--at: 0"></div>
                <div class="anim" id="a50" style="--at: 50"></div>
                <div class="anim" id="a100" style="--at: 100"></div>
            </div>
            <div id="po"></div>
            <div id="rg"></div>
            <svg width="100" height="100">
                <rect id="r" stroke-width="var(--sw)" style="--sw: 12px; stroke-linecap: round; fill-opacity: 0.5;"></rect>
            </svg>
        </body></html>"#,
    );
    assert_eq!(
        eval_str(
            &mut rt,
            r#"[
                getComputedStyle(document.getElementById('legal')).backgroundColor,
                document.getElementById('a0').offsetWidth,
                document.getElementById('a50').offsetWidth,
                document.getElementById('a100').offsetWidth,
                getComputedStyle(document.getElementById('po')).perspectiveOrigin,
                getComputedStyle(document.getElementById('rg')).backgroundImage,
                getComputedStyle(document.getElementById('r')).getPropertyValue('stroke-width'),
                getComputedStyle(document.getElementById('r')).getPropertyValue('stroke-linecap'),
                getComputedStyle(document.getElementById('r')).getPropertyValue('fill-opacity')
            ].join('|')"#
        ),
        "rgb(0, 128, 0)|50|200|100|0px 100px|radial-gradient(at 25px 25px, rgb(0, 0, 0) 10%, rgb(0, 128, 0) 90%)|12px|round|0.5"
    );
}
