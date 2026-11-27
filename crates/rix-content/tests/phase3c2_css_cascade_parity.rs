//! Phase 3C-2 regression tests: WPT `css/css-cascade` parity (0 Firefox-only gaps).
#![deny(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_text::FontDatabase;

fn eval_doc(html: &str, js: &str) -> String {
    let doc = Rc::new(RefCell::new(Document::new()));
    let mut rt = ContentRuntime::new(doc, Size::new(800.0, 600.0), Arc::new(FontDatabase::new()));
    assert!(rt
        .load_document_with_external_scripts(html, &HashMap::new())
        .is_empty());
    let v = rt
        .eval_script(js)
        .unwrap_or_else(|e| panic!("JS error: {e:?}"));
    rt.vm.value_to_rust_string(v)
}

#[test]
fn cascade_layers_scope_revert_and_cssom_declaration_context_parity() {
    let html = r#"<!doctype html><style id="s1">
      @layer b, a;
      @import url("data:text/css,%23imp{color:rgb(0,128,0)}") layer(a) supports(font-format(woff2));
      @layer a { @counter-style cs { system: cyclic; symbols: "A"; } }
      @layer b { @counter-style cs { system: cyclic; symbols: "B"; } }
      #cnt::before { content: counter(c, cs); counter-reset: c 1; }
      @layer base { #ws { white-space: pre; } }
      @layer top { #ws { all: revert-layer; } }
      @scope (.a) { @scope (> #child) { :scope { z-index: 7; } } }
      @scope (:root) {
        @position-try --fb { left: 0px; width: 105px; height: var(--h); place-self: center; display: grid; --x: 1; }
        @page { display: grid; }
        display: grid;
      }
    </style><div id="imp"></div><div id="cnt"></div><div id="ws"></div><div class="a"><div id="child"></div></div>"#;
    let out = eval_doc(
        html,
        r#"(() => {
          const st = s1.sheet.cssRules[0], lb = s1.sheet.cssRules[2];
          const sc = s1.sheet.cssRules[s1.sheet.cssRules.length - 1];
          const pt = sc.cssRules[0], pg = sc.cssRules[1], nd = sc.cssRules[2];
          const ds = new CSSStyleSheet();
          ds.replaceSync('@scope { @scope (.a) { #child { opacity: 0.5; } } }');
          document.adoptedStyleSheets = [ds];
          return [
            getComputedStyle(imp).color, getComputedStyle(cnt, '::before').content,
            getComputedStyle(ws).textWrapMode, getComputedStyle(child).zIndex,
            getComputedStyle(child).opacity, pt instanceof CSSPositionTryRule,
            pt.style.getPropertyValue('left'), pt.style.getPropertyValue('height'),
            pt.style.getPropertyValue('place-self'), pt.style.getPropertyValue('display'),
            pt.style.getPropertyValue('--x'), pg.style.getPropertyValue('display'),
            nd.style.getPropertyValue('display'), Object.prototype.toString.call(st),
            Object.prototype.toString.call(lb), Object.prototype.toString.call(sc),
            Object.isFrozen(st.nameList), st.nameList.join(','), lb.name, sc.start,
          ].join('|');
        })()"#,
    );
    assert_eq!(
        out,
        "rgb(0, 128, 0)|\"A\"|nowrap|7|0.5|true|0px|var(--h)|center||||grid|[object CSSLayerStatementRule]|[object CSSLayerBlockRule]|[object CSSScopeRule]|true|b,a|a|:root"
    );
}
