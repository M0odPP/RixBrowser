//! Integration tests for core WHATWG Web Platform APIs:
//! `URL`/`URLSearchParams`, `TextEncoder`/`TextDecoder`, `Blob`/`File`,
//! `structuredClone`, `AbortController`/`AbortSignal`, `Location`/`History`,
//! `Performance`, and persistent `localStorage` (`redb`) / `sessionStorage`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_storage::StorageEngine;
use rix_text::FontDatabase;

#[test]
fn url_encoding_blob_structured_clone_abort_history_performance_and_persistent_storage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("core_web_storage.redb");

    {
        let storage = StorageEngine::open(&db_path).expect("open redb");
        let doc = Rc::new(RefCell::new(Document::new()));
        let mut rt = ContentRuntime::with_origin_and_storage(
            doc,
            Size::new(800.0, 600.0),
            Arc::new(FontDatabase::new()),
            "https://example.test:8443/docs?a=1&b=2#top",
            storage,
        );
        rt.event_loop.borrow_mut().enable_virtual_clock();

        let script = r#"
            var u = new URL("/search?q=rust&tag=a", "https://rix.dev:443/base");
            u.searchParams.append("tag", "b");
            u.searchParams.set("page", "2");
            var urlOk = u.origin === "https://rix.dev"
                && u.pathname === "/search"
                && u.searchParams.getAll("tag").join(",") === "a,b"
                && u.search.indexOf("page=2") >= 0
                && URL.canParse("https://rix.dev");

            var enc = new TextEncoder();
            var dec = new TextDecoder("utf-8");
            var utf8 = enc.encode("Rix\u2764");
            var textOk = dec.decode(utf8) === "Rix\u2764" && utf8.length === 6;

            var blob = new Blob(["Hello ", new Uint8Array([82, 105, 120])], { type: "text/plain" });
            var file = new File(["abc"], "demo.txt", { type: "text/plain", lastModified: 12345 });
            var blobText = "";
            blob.text().then(function(t) { blobText = t; });
            var fileOk = file.name === "demo.txt" && file.size === 3 && file.lastModified === 12345;

            var orig = { a: 1, arr: [2, 3], map: new Map([["k", 9]]) };
            orig.self = orig;
            var cloned = structuredClone(orig);
            cloned.arr.push(99);
            var cloneErr = false;
            try { structuredClone(function() {}); } catch (e) { cloneErr = e.name === "DataCloneError"; }
            var cloneOk = cloned !== orig && cloned.self === cloned && orig.arr.length === 2
                && cloned.map.get("k") === 9 && cloneErr;

            var ac = new AbortController();
            var abortFired = false;
            ac.signal.addEventListener("abort", function() { abortFired = true; });
            ac.abort();
            var abortOk = abortFired && ac.signal.aborted && ac.signal.reason.name === "AbortError";

            history.pushState({ step: 1 }, "", "/next?x=9");
            var locAfterPush = location.pathname + location.search;
            history.back();
            var locAfterBack = location.pathname;
            history.forward();
            var histOk = locAfterPush === "/next?x=9" && locAfterBack === "/docs"
                && history.state.step === 1 && history.length === 2;

            performance.mark("t0");
            localStorage.setItem("theme", "dark");
            localStorage.setItem("token", "xyz-123");
            sessionStorage.setItem("tabKey", "ephemeral");
            var storageOk = localStorage.getItem("theme") === "dark"
                && localStorage.length === 2
                && sessionStorage.getItem("tabKey") === "ephemeral";
        "#;

        rt.eval_script(script).expect("eval web apis");
        rt.advance_and_drain(Duration::from_millis(25));
        let check = rt
            .eval_script(
                r#"
                performance.mark("t1");
                var m = performance.measure("span", "t0", "t1");
                var perfOk = m.duration === 25 && performance.getEntriesByType("measure").length === 1;
                JSON.stringify({ urlOk, textOk, fileOk, blobText, cloneOk, abortOk, histOk, storageOk, perfOk });
                "#,
            )
            .expect("verify web apis");
        let s = rt.vm.value_to_rust_string(check);
        for field in [
            r#""urlOk":true"#,
            r#""textOk":true"#,
            r#""fileOk":true"#,
            r#""blobText":"Hello Rix""#,
            r#""cloneOk":true"#,
            r#""abortOk":true"#,
            r#""histOk":true"#,
            r#""storageOk":true"#,
            r#""perfOk":true"#,
        ] {
            assert!(s.contains(field), "missing {field} in {s}");
        }
    }

    // Reopen redb database and verify localStorage persistence & origin isolation.
    {
        let storage = StorageEngine::open(&db_path).expect("reopen redb");
        let doc = Rc::new(RefCell::new(Document::new()));
        let mut rt = ContentRuntime::with_origin_and_storage(
            doc,
            Size::new(800.0, 600.0),
            Arc::new(FontDatabase::new()),
            "https://example.test:8443/other-page",
            storage,
        );
        let persisted = rt
            .eval_script(
                r#"localStorage.getItem("theme") + "|" + localStorage.getItem("token") + "|" + sessionStorage.getItem("tabKey")"#,
            )
            .expect("check persisted");
        assert_eq!(rt.vm.value_to_rust_string(persisted), "dark|xyz-123|null");

        rt.set_document_url("https://other.origin/");
        let isolated = rt
            .eval_script(r#"localStorage.getItem("theme") === null && localStorage.length === 0"#)
            .expect("check isolation");
        assert_eq!(isolated.as_bool(), Some(true));
    }
}
