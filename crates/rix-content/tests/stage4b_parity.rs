//! Specification regression tests for HTML5 §4.10.5.3.13 (`HTMLInputElement.list` /
//! `showPicker`), dynamic subresource connection hooks (`innerHTML` / `setAttribute`), and
//! incremental `<style>` stylesheet append caching.

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
    ContentRuntime::new(doc, Size::new(800.0, 600.0), Arc::new(FontDatabase::new()))
}

#[test]
fn html_input_list_resolves_associated_datalist_and_show_picker_is_callable() {
    let html = r#"<!doctype html><html><body>
        <input id="lang" list="languages">
        <datalist id="languages"><option value="Rust"></option></datalist>
        <input id="other" list="missing">
        <script>
            const inp = document.getElementById("lang");
            const dl = inp.list;
            window.__listId = dl ? dl.id : "NONE";
            window.__listTag = dl ? dl.tagName : "NONE";
            window.__missing = document.getElementById("other").list === null;
            inp.showPicker();
            window.__pickerOk = true;
        </script>
    </body></html>"#;
    let mut rt = make_runtime();
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "Unexpected JS errors: {errs:?}");
    let summary = rt
        .eval_script(
            "`${window.__listId}|${window.__listTag}|${window.__missing}|${window.__pickerOk}`",
        )
        .expect("eval");
    assert_eq!(
        rt.vm.value_to_rust_string(summary),
        "languages|DATALIST|true|true"
    );
}

#[test]
fn dynamic_img_inner_html_and_set_attribute_trigger_image_loading_pipeline() {
    let html = r#"<!doctype html><html><body>
        <div id="host"></div>
        <script>
            document.getElementById("host").innerHTML = '<img id="a" src="logo-a.png">';
            const imgB = document.createElement("img");
            imgB.id = "b";
            document.body.appendChild(imgB);
            imgB.setAttribute("src", "logo-b.png");
        </script>
    </body></html>"#;
    let mut rt = make_runtime();
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "Unexpected JS errors: {errs:?}");
    let host = rt.host.borrow();
    // Both images entered the dynamic image loading pipeline (pending or already completed).
    let tracked = host.pending_dynamic_images.len() + host.completed_images.len();
    assert!(
        tracked >= 2,
        "Expected >= 2 dynamically loaded images (innerHTML + setAttribute), tracked={tracked}"
    );
}

#[test]
fn incremental_style_append_updates_computed_style_without_losing_prior_rules() {
    let html = r#"<!doctype html><html><head>
        <style>.box { width: 100px; color: rgb(10, 20, 30); }</style>
    </head><body>
        <div id="b" class="box">hi</div>
        <script>
            const b = document.getElementById("b");
            window.__c1 = getComputedStyle(b).color;
            const s = document.createElement("style");
            s.textContent = ".box { color: rgb(200, 100, 50); }";
            document.head.appendChild(s);
            window.__c2 = getComputedStyle(b).color;
            window.__w2 = getComputedStyle(b).width;
        </script>
    </body></html>"#;
    let mut rt = make_runtime();
    let errs = rt.load_document_with_external_scripts(html, &HashMap::new());
    assert!(errs.is_empty(), "Unexpected JS errors: {errs:?}");
    let res = rt
        .eval_script("`${window.__c1}|${window.__c2}|${window.__w2}`")
        .expect("eval");
    assert_eq!(
        rt.vm.value_to_rust_string(res),
        "rgb(10, 20, 30)|rgb(200, 100, 50)|100px"
    );
}
