//! Integration tests for async transactional W3C IndexedDB (`IDBFactory`,
//! `IDBDatabase`, `IDBTransaction`, `IDBObjectStore`, `IDBIndex`, `IDBCursor`,
//! `IDBKeyRange`) persisted per origin in `redb`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rix_content::ContentRuntime;
use rix_core::Size;
use rix_dom::Document;
use rix_storage::StorageEngine;
use rix_text::FontDatabase;

fn runtime_with_storage(origin_url: &str, storage: StorageEngine) -> ContentRuntime {
    let doc = Rc::new(RefCell::new(Document::new()));
    let font_db = Arc::new(FontDatabase::new());
    ContentRuntime::with_origin_and_storage(
        doc,
        Size::new(800.0, 600.0),
        font_db,
        origin_url,
        storage,
    )
}

#[test]
fn indexeddb_crud_indexes_cursors_key_ranges_and_acid_rollback() {
    let storage = StorageEngine::in_memory().expect("in-memory storage");
    let mut rt = runtime_with_storage("https://idb-app.example/index.html", storage);

    rt.eval_script(
        r#"
        globalThis.done = false;
        globalThis.err = "";
        globalThis.out = {};

        const openReq = indexedDB.open("notes-db", 1);
        openReq.onupgradeneeded = (ev) => {
            globalThis.out.oldVer = ev.oldVersion;
            globalThis.out.newVer = ev.newVersion;
            const db = openReq.result;
            const store = db.createObjectStore("users", { keyPath: "id", autoIncrement: true });
            store.createIndex("by_email", "email", { unique: true });
            store.createIndex("by_tag", "tags", { multiEntry: true });
            store.createIndex("by_age", "age");
        };
        openReq.onerror = () => {
            globalThis.err = "open failed: " + (openReq.error && openReq.error.name);
            globalThis.done = true;
        };
        openReq.onsuccess = () => {
            const db = openReq.result;
            globalThis.out.dbName = db.name;
            globalThis.out.dbVer = db.version;
            globalThis.out.hasUsers = db.objectStoreNames.contains("users");

            // Tx 1: insert 4 users and commit
            const tx1 = db.transaction("users", "readwrite");
            const s1 = tx1.objectStore("users");
            s1.add({ email: "alice@ex.com", age: 30, tags: ["rust", "gpu"] });
            s1.add({ email: "bob@ex.com", age: 22, tags: ["js", "rust"] });
            s1.add({ email: "carol@ex.com", age: 30, tags: ["css"] });
            s1.add({ email: "dave@ex.com", age: 40, tags: ["gpu"] });

            tx1.oncomplete = () => {
                // Tx 2: mutate & explicitly abort -> must roll back completely!
                const tx2 = db.transaction("users", "readwrite");
                const s2 = tx2.objectStore("users");
                s2.put({ id: 99, email: "ghost@ex.com", age: 99, tags: [] });
                s2.delete(1);
                tx2.abort();

                tx2.onabort = () => {
                    globalThis.out.tx2Aborted = true;

                    // Tx 3: trigger unique ConstraintError without preventDefault -> rolls back!
                    const tx3 = db.transaction("users", "readwrite");
                    const s3 = tx3.objectStore("users");
                    s3.add({ email: "temp@ex.com", age: 19, tags: [] });
                    const dupReq = s3.add({ email: "alice@ex.com", age: 50, tags: [] });
                    dupReq.onerror = () => {
                        globalThis.out.dupErrName = dupReq.error && dupReq.error.name;
                    };
                    tx3.onabort = () => {
                        globalThis.out.tx3RolledBack = true;

                        // Tx 4: duplicate error with preventDefault() keeps tx alive!
                        const tx4 = db.transaction("users", "readwrite");
                        const s4 = tx4.objectStore("users");
                        const dupHandled = s4.add({ email: "bob@ex.com", age: 25, tags: [] });
                        dupHandled.onerror = (e) => { e.preventDefault(); };
                        s4.add({ email: "eve@ex.com", age: 28, tags: ["rust"] });

                        tx4.oncomplete = () => {
                            // Tx 5: verify indexes, key ranges, cursors (prev, nextunique, update, delete)
                            const tx5 = db.transaction("users", "readwrite");
                            const s5 = tx5.objectStore("users");
                            const countReq = s5.count();
                            countReq.onsuccess = () => { globalThis.out.countAfterRollbacks = countReq.result; };

                            const ghostReq = s5.get(99);
                            ghostReq.onsuccess = () => { globalThis.out.ghostUndefined = (ghostReq.result === undefined); };

                            const aliceReq = s5.index("by_email").get("alice@ex.com");
                            aliceReq.onsuccess = () => { globalThis.out.aliceAge = aliceReq.result.age; };

                            const rustReq = s5.index("by_tag").getAll("rust");
                            rustReq.onsuccess = () => {
                                globalThis.out.rustEmails = rustReq.result.map((u) => u.email);
                            };

                            const rangeReq = s5.index("by_age").getAll(IDBKeyRange.bound(25, 35));
                            rangeReq.onsuccess = () => {
                                globalThis.out.midAges = rangeReq.result.map((u) => u.age);
                            };

                            const uniqAges = [];
                            const uCur = s5.index("by_age").openKeyCursor(null, "nextunique");
                            uCur.onsuccess = () => {
                                const c = uCur.result;
                                if (c) {
                                    uniqAges.push(c.key);
                                    c.continue();
                                } else {
                                    globalThis.out.uniqAges = uniqAges;
                                }
                            };

                            const revIds = [];
                            const pCur = s5.openCursor(IDBKeyRange.lowerBound(2), "prev");
                            pCur.onsuccess = () => {
                                const c = pCur.result;
                                if (c) {
                                    revIds.push(c.key);
                                    if (c.value.email === "dave@ex.com") {
                                        c.delete();
                                    }
                                    c.continue();
                                } else {
                                    globalThis.out.revIds = revIds;
                                }
                            };

                            tx5.oncomplete = () => {
                                const tx6 = db.transaction("users", "readonly");
                                const finalCount = tx6.objectStore("users").count();
                                finalCount.onsuccess = () => {
                                    globalThis.out.finalCountAfterCursorDelete = finalCount.result;
                                    globalThis.out.cmpNumStr = indexedDB.cmp(10, "a");
                                    globalThis.out.rangeIncludes = IDBKeyRange.bound(1, 5, true, false).includes(1) === false
                                        && IDBKeyRange.bound(1, 5, true, false).includes(5) === true;
                                    globalThis.done = true;
                                };
                            };
                        };
                    };
                };
            };
        };
        "#,
    )
    .expect("eval idb script");

    rt.advance_and_drain(Duration::from_millis(20));

    let err_v = rt.eval_script("globalThis.err").unwrap();
    assert_eq!(rt.vm.value_to_rust_string(err_v), "");
    assert_eq!(
        rt.eval_script("globalThis.done").unwrap().as_bool(),
        Some(true),
        "IDB async chain should complete cleanly"
    );

    let out_v = rt.eval_script("JSON.stringify(globalThis.out)").unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&rt.vm.value_to_rust_string(out_v)).unwrap();
    assert_eq!(parsed["oldVer"], 0);
    assert_eq!(parsed["newVer"], 1);
    assert_eq!(parsed["dbName"], "notes-db");
    assert_eq!(parsed["dbVer"], 1);
    assert_eq!(parsed["hasUsers"], true);
    assert_eq!(parsed["tx2Aborted"], true);
    assert_eq!(parsed["dupErrName"], "ConstraintError");
    assert_eq!(parsed["tx3RolledBack"], true);
    assert_eq!(parsed["countAfterRollbacks"], 5);
    assert_eq!(parsed["ghostUndefined"], true);
    assert_eq!(parsed["aliceAge"], 30);
    assert_eq!(
        parsed["rustEmails"],
        serde_json::json!(["alice@ex.com", "bob@ex.com", "eve@ex.com"])
    );
    assert_eq!(parsed["midAges"], serde_json::json!([28, 30, 30]));
    // Per W3C IndexedDB 3.0 §5.7, cursors iterate live store state in transaction FIFO order:
    // `pCur` deletes `dave` (id 4, age 40) while `uCur` is at age 30, so `uCur` sees `[22, 28, 30]`.
    assert_eq!(parsed["uniqAges"], serde_json::json!([22, 28, 30]));
    // Tx 2 & Tx 3 aborted (reverting key generator to 5); in Tx 4 the prevented ConstraintError
    // reverts its failed operation per W3C IndexedDB 3.0 §5.1 (matching WPT
    // `request-event-ordering-small-values.any.js`) so `eve` receives autoIncrement key 5.
    assert_eq!(parsed["revIds"], serde_json::json!([5, 4, 3, 2]));
    assert_eq!(parsed["finalCountAfterCursorDelete"], 4);
    assert_eq!(parsed["cmpNumStr"], -1);
    assert_eq!(parsed["rangeIncludes"], true);
}

#[test]
fn indexeddb_persists_durably_across_redb_restarts_and_isolates_origins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("indexeddb_restart.redb");
    let origin_a = "https://alpha.example/app";
    let origin_b = "https://beta.example/app";

    // Session 1: write committed data + aborted data on origin A
    {
        let storage = StorageEngine::open(&db_path).expect("open redb");
        let mut rt = runtime_with_storage(origin_a, storage);
        rt.eval_script(
            r#"
            globalThis.s1Done = false;
            const req = indexedDB.open("vault", 2);
            req.onupgradeneeded = () => {
                const store = req.result.createObjectStore("kv", { keyPath: "k" });
                store.createIndex("by_group", "g");
                store.put({ k: "k1", g: "core", v: [1, 2, 3] });
            };
            req.onsuccess = () => {
                const db = req.result;
                const tx = db.transaction("kv", "readwrite");
                tx.objectStore("kv").put({ k: "k2", g: "core", v: "durable" });
                tx.oncomplete = () => {
                    const abortedTx = db.transaction("kv", "readwrite");
                    abortedTx.objectStore("kv").put({ k: "k3-uncommitted", g: "core", v: "nope" });
                    abortedTx.abort();
                    abortedTx.onabort = () => { globalThis.s1Done = true; };
                };
            };
            "#,
        )
        .expect("session 1");
        rt.advance_and_drain(Duration::from_millis(20));
        assert_eq!(
            rt.eval_script("globalThis.s1Done").unwrap().as_bool(),
            Some(true)
        );
    }

    // Session 2: reopen SAME redb file on origin A -> committed data survives, aborted gone!
    {
        let storage = StorageEngine::open(&db_path).expect("reopen redb");
        let mut rt = runtime_with_storage(origin_a, storage);
        rt.eval_script(
            r#"
            globalThis.s2Done = false;
            globalThis.s2Report = "";
            indexedDB.databases().then((dbs) => {
                const req = indexedDB.open("vault");
                req.onupgradeneeded = () => { globalThis.unexpectedUpgrade = true; };
                req.onsuccess = () => {
                    const db = req.result;
                    const tx = db.transaction("kv", "readonly");
                    const rAll = tx.objectStore("kv").index("by_group").getAll("core");
                    rAll.onsuccess = () => {
                        globalThis.s2Report = JSON.stringify({
                            dbs,
                            version: db.version,
                            unexpectedUpgrade: Boolean(globalThis.unexpectedUpgrade),
                            items: rAll.result,
                        });
                        globalThis.s2Done = true;
                    };
                };
            });
            "#,
        )
        .expect("session 2");
        rt.advance_and_drain(Duration::from_millis(20));
        assert_eq!(
            rt.eval_script("globalThis.s2Done").unwrap().as_bool(),
            Some(true)
        );

        let rep_v = rt.eval_script("globalThis.s2Report").unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&rt.vm.value_to_rust_string(rep_v)).unwrap();
        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["unexpectedUpgrade"], false);
        assert_eq!(
            parsed["dbs"],
            serde_json::json!([{ "name": "vault", "version": 2 }])
        );
        assert_eq!(
            parsed["items"],
            serde_json::json!([
                { "k": "k1", "g": "core", "v": [1, 2, 3] },
                { "k": "k2", "g": "core", "v": "durable" }
            ])
        );
    }

    // Session 3: reopen SAME redb file on origin B -> completely isolated!
    {
        let storage = StorageEngine::open(&db_path).expect("reopen redb origin B");
        let mut rt = runtime_with_storage(origin_b, storage);
        rt.eval_script(
            r#"
            globalThis.bCount = -1;
            indexedDB.databases().then((dbs) => { globalThis.bCount = dbs.length; });
            "#,
        )
        .expect("session 3");
        rt.advance_and_drain(Duration::from_millis(10));
        assert_eq!(
            rt.eval_script("globalThis.bCount").unwrap().as_number(),
            Some(0.0)
        );
    }
}
