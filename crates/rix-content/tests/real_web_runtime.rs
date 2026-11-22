use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_core::Size;
use rix_css::{CascadeOrigin, MediaContext, Stylesheet};
use rix_dom::{Document, NodeData};
use rix_ipc::PipelineId;
use rix_render::ResourceRegistry;
use rix_storage::StorageEngine;
use rix_text::FontDatabase;

use rix_content::ContentRuntime;

#[test]
fn external_scripts_modules_events_console_and_incremental_restyle() {
    let storage = StorageEngine::in_memory().expect("in-memory storage");
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    let mut rt = ContentRuntime::with_origin_and_storage(
        doc,
        Size::new(1280.0, 800.0),
        font_db,
        "https://app.example/index.html",
        storage,
    );

    let mut external_scripts = HashMap::new();
    external_scripts.insert(
        "https://app.example/app.js".to_string(),
        r#"
            window.__order.push("ext-sync");
            console.log("app booted", innerWidth, innerHeight);
        "#
        .to_string(),
    );
    external_scripts.insert(
        "https://app.example/deferred.js".to_string(),
        r#"
            window.__order.push("defer");
            document.title = "Updated Title";
        "#
        .to_string(),
    );
    external_scripts.insert(
        "https://app.example/mod.js".to_string(),
        r#"
            export const tag = "esm-ok";
        "#
        .to_string(),
    );

    let html = r#"<!doctype html>
    <html>
    <head>
        <title>Initial</title>
        <script type="importmap">{"imports": {"mod": "https://app.example/mod.js"}}</script>
        <script>
            window.__order = ["inline-1"];
            document.addEventListener("DOMContentLoaded", function() {
                window.__order.push("dcl:" + document.readyState);
            });
            window.addEventListener("load", function() {
                window.__order.push("load:" + document.readyState);
            });
        </script>
        <script src="/app.js"></script>
        <script defer src="/deferred.js"></script>
        <script type="module">
            import { tag } from "mod";
            window.__order.push(tag);
        </script>
    </head>
    <body onload="window.__order.push('body-onload')">
        <div id="root"><button id="btn" onclick="window.__clicked = (window.__clicked || 0) + 1">Click</button></div>
    </body>
    </html>"#;

    let errors = rt.load_document_with_external_scripts(html, &external_scripts);
    assert!(
        errors.is_empty(),
        "expected no script errors, got: {errors:?}"
    );

    let order = rt
        .eval_script("window.__order.join(',')")
        .expect("read order");
    let order_str = rt.vm.value_to_rust_string(order);
    assert_eq!(
        order_str,
        "inline-1,ext-sync,defer,esm-ok,dcl:interactive,load:complete,body-onload"
    );

    let title = rt.eval_script("document.title").expect("read title");
    assert_eq!(rt.vm.value_to_rust_string(title), "Updated Title");

    let console = rt.drain_console_messages();
    assert!(
        console
            .iter()
            .any(|(lvl, msg)| lvl == "log" && msg.contains("app booted 1280 800")),
        "expected console.log capture, got {console:?}"
    );

    // Apply external stylesheet and verify incremental paint + restyle_dirty fast path.
    let ext_css = Stylesheet::parse_cached(
        "#root { background: #1e293b; color: #f8fafc; } #root.active { background: #2563eb; }",
        CascadeOrigin::Author,
        &MediaContext::default(),
    );
    rt.host.borrow_mut().set_external_stylesheets(vec![ext_css]);
    let mut resources = ResourceRegistry::default();
    let first = rt
        .host
        .borrow_mut()
        .paint_if_dirty_with_metrics(PipelineId(1), &mut resources);
    assert!(first.is_some());

    // Mutate DOM via toggleAttribute & insertAdjacentHTML, then ensure incremental restyle runs.
    rt.eval_script(
        r#"
            const root = document.getElementById("root");
            root.setAttribute("class", "active");
            root.toggleAttribute("data-open", true);
            root.insertAdjacentHTML("beforeend", "<span id='badge'>New</span>");
            const svgEl = document.createElementNS("http://www.w3.org/2000/svg", "svg");
            root.appendChild(svgEl);
        "#,
    )
    .expect("dom mutations");

    let second = rt
        .host
        .borrow_mut()
        .paint_if_dirty_with_metrics(PipelineId(1), &mut resources);
    assert!(second.is_some());

    // Dispatch click event directly to #btn and verify inline onclick + bubbling + preventDefault().
    let btn_id = {
        let doc_rc = Rc::clone(&rt.host.borrow().document);
        let doc = doc_rc.borrow();
        let found = doc
            .descendants(doc.root())
            .find(|&n| {
                matches!(
                    doc.get(n),
                    Some(NodeData::Element(e)) if e.get_attribute("id") == Some("btn")
                )
            })
            .expect("btn node");
        found
    };
    let not_prevented = rt.dispatch_node_event(btn_id, "click", true, true, &[]);
    assert!(not_prevented);
    let clicks = rt.eval_script("window.__clicked").expect("read clicks");
    assert_eq!(clicks.as_number(), Some(1.0));

    // Listener calling preventDefault() returns false.
    rt.eval_script(
        r#"document.getElementById("btn").addEventListener("click", (e) => e.preventDefault());"#,
    )
    .expect("add preventDefault");
    let after_prevent = rt.dispatch_node_event(btn_id, "click", true, true, &[]);
    assert!(!after_prevent);

    // Verify matchMedia dark mode toggle, JIT reconfiguration, and IDLE 0 Hz wakeup delay.
    rt.host.borrow_mut().set_dark_mode(true);
    let dark_matches = rt
        .eval_script("window.matchMedia('(prefers-color-scheme: dark)').matches")
        .expect("matchMedia dark");
    assert_eq!(dark_matches.as_bool(), Some(true));

    rt.configure_jit(true, false);
    rt.configure_jit(false, false);
    rt.configure_jit(true, true);
    let sum = rt
        .eval_script_cached("let s = 0; for (let i = 0; i < 100; i++) s += i; s")
        .expect("cached eval");
    assert_eq!(sum.as_number(), Some(4950.0));
    assert_eq!(
        rt.next_wakeup_delay(),
        None,
        "idle runtime must report 0 Hz wakeup"
    );
}
