//! Persistence, origin isolation, and W3C key-order tests for `redb`-backed
//! `CacheStorage` and `IndexedDB` in `rix-storage`.

use std::cmp::Ordering;

use rix_storage::{
    CachedHttpResponse, IdbDatabaseSnapshot, IdbIndexSchema, IdbKey, IdbRecord, IdbStoreSnapshot,
    StorageEngine,
};
use tempfile::TempDir;

#[test]
fn idb_key_total_ordering_and_encoded_byte_order_match_w3c_spec() {
    let keys = vec![
        IdbKey::Number(-100.5),
        IdbKey::Number(-1.0),
        IdbKey::Number(0.0),
        IdbKey::Number(42.0),
        IdbKey::Number(1e9),
        IdbKey::Date(0.0),
        IdbKey::Date(1_700_000_000_000.0),
        IdbKey::String(String::new()),
        IdbKey::String("alpha".to_string()),
        IdbKey::String("beta".to_string()),
        IdbKey::Binary(vec![0, 1, 2]),
        IdbKey::Binary(vec![0, 2, 0]),
        IdbKey::Array(vec![IdbKey::Number(1.0), IdbKey::String("a".to_string())]),
        IdbKey::Array(vec![IdbKey::Number(1.0), IdbKey::String("b".to_string())]),
        IdbKey::Array(vec![IdbKey::Number(2.0)]),
    ];

    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            assert_eq!(
                keys[i].idb_cmp(&keys[j]),
                Ordering::Less,
                "expected {:?} < {:?}",
                keys[i],
                keys[j]
            );
            assert!(
                keys[i].to_encoded_bytes() < keys[j].to_encoded_bytes(),
                "encoded bytes must preserve W3C order for {:?} < {:?}",
                keys[i],
                keys[j]
            );
        }
    }
}

#[test]
fn cache_storage_and_indexeddb_persist_durably_across_redb_restarts() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("profile.redb");

    // 1. Write CacheStorage + IndexedDB entries and close StorageEngine cleanly.
    {
        let storage = StorageEngine::open(&db_path).expect("open redb");
        storage
            .cache_open("https://app.example", "v1")
            .expect("cache open");
        storage
            .cache_put(
                "https://app.example",
                "v1",
                "https://app.example/api/data",
                CachedHttpResponse {
                    url: "https://app.example/api/data".to_string(),
                    status: 200,
                    status_text: "OK".to_string(),
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: br#"{"cached":true}"#.to_vec(),
                },
            )
            .expect("cache put");

        let mut snap = IdbDatabaseSnapshot::new("notes".to_string(), 2);
        let mut store =
            IdbStoreSnapshot::new("items".to_string(), Some(vec!["id".to_string()]), true);
        store.indexes.insert(
            "by_title".to_string(),
            IdbIndexSchema {
                name: "by_title".to_string(),
                key_path: vec!["title".to_string()],
                unique: true,
                multi_entry: false,
            },
        );
        let k1 = IdbKey::Number(1.0);
        store.records.insert(
            k1.to_encoded_bytes(),
            IdbRecord {
                key: k1,
                value_json: r#"{"id":1,"title":"persisted"}"#.to_string(),
            },
        );
        store.next_key = 2;
        snap.stores.insert("items".to_string(), store);
        storage
            .idb_commit_database("https://app.example", &snap)
            .expect("idb commit");
    }

    // 2. Reopen fresh StorageEngine from disk and verify full durability + origin isolation!
    {
        let storage = StorageEngine::open(&db_path).expect("reopen redb");
        assert!(storage.cache_has("https://app.example", "v1").expect("has"));
        assert!(!storage
            .cache_has("https://other.example", "v1")
            .expect("origin isolated"));

        let cached = storage
            .cache_match("https://app.example", "v1", "https://app.example/api/data")
            .expect("match")
            .expect("hit");
        assert_eq!(cached.status, 200);
        assert_eq!(cached.body, br#"{"cached":true}"#);

        let loaded = storage
            .idb_load_database("https://app.example", "notes")
            .expect("load idb")
            .expect("exists");
        assert_eq!(loaded.version, 2);
        let store = loaded.stores.get("items").expect("items store");
        assert_eq!(store.next_key, 2);
        assert!(store.indexes.contains_key("by_title"));
        let rec = store
            .records
            .get(&IdbKey::Number(1.0).to_encoded_bytes())
            .expect("record 1");
        assert_eq!(rec.value_json, r#"{"id":1,"title":"persisted"}"#);

        assert!(storage
            .idb_load_database("https://other.example", "notes")
            .expect("other origin")
            .is_none());
    }
}
