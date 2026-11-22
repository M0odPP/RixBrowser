//! Pure-Rust W3C IndexedDB key conversion, key-path evaluation, `KeyRange`
//! matching, `autoIncrement` key generation, unique-index constraint enforcement,
//! and cursor record collection over `rix-storage`'s `IdbStoreSnapshot`.
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cmp::Ordering;
use std::collections::HashSet;

use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};
use rix_storage::{IdbIndexSchema, IdbKey, IdbRecord, IdbStoreSnapshot};

use crate::web_apis::{alloc_array_buffer, arg, create_dom_exception, define_host_method};

pub fn alloc_js_array(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

/// Allocates a `DOMStringList` (array of sorted strings with `.contains(s)` and `.item(i)`).
pub fn alloc_dom_string_list(vm: &mut JsVm, mut names: Vec<String>) -> HeapHandle {
    names.sort();
    let vals: Vec<JsValue> = names
        .iter()
        .map(|s| JsValue::from_string(vm.heap.alloc_string(s)))
        .collect();
    let arr = alloc_js_array(vm, &vals);
    let names_c = names.clone();
    define_host_method(vm, arr, "contains", move |vm, _, args| {
        let needle = vm.value_to_rust_string(arg(args, 0));
        Ok(JsValue::from_bool(names_c.iter().any(|n| n == &needle)))
    });
    define_host_method(vm, arr, "item", move |vm, _, args| {
        let idx = arg(args, 0).as_number().unwrap_or(-1.0);
        if idx >= 0.0 && (idx as usize) < names.len() {
            Ok(JsValue::from_string(
                vm.heap.alloc_string(&names[idx as usize]),
            ))
        } else {
            Ok(JsValue::NULL)
        }
    });
    arr
}

/// Converts a JS value to a valid W3C IndexedDB key, throwing `DataError` on invalid values.
pub fn js_to_idb_key(
    vm: &mut JsVm,
    val: JsValue,
    seen: &mut HashSet<HeapHandle>,
) -> Result<IdbKey, JsException> {
    if let Some(n) = val.as_number() {
        if n.is_nan() {
            return Err(create_dom_exception(
                vm,
                "DataError",
                "NaN is not a valid IndexedDB key",
            ));
        }
        return Ok(IdbKey::Number(if n == 0.0 { 0.0 } else { n }));
    }
    if val.is_string() {
        return Ok(IdbKey::String(vm.value_to_rust_string(val)));
    }
    if let Some(h) = val.as_object() {
        enum KindInfo {
            Date(f64),
            Binary(Vec<u8>),
            Array(Vec<JsValue>),
            Invalid,
        }
        let info = match vm.heap.get_object(h).map(|o| &o.kind) {
            Some(ObjectKind::Date(ms)) => KindInfo::Date(*ms),
            Some(ObjectKind::ArrayBuffer(buf)) => KindInfo::Binary(buf.as_ref().clone()),
            Some(ObjectKind::TypedArray(ta)) => {
                let bytes = match vm.heap.get_object(ta.buffer).map(|o| &o.kind) {
                    Some(ObjectKind::ArrayBuffer(buf)) => {
                        let start = (ta.byte_offset as usize).min(buf.len());
                        let byte_len =
                            (ta.length as usize).saturating_mul(ta.kind.bytes_per_element());
                        let end = start.saturating_add(byte_len).min(buf.len());
                        buf[start..end].to_vec()
                    }
                    _ => Vec::new(),
                };
                KindInfo::Binary(bytes)
            }
            Some(ObjectKind::Array) => {
                let len = vm
                    .heap
                    .get_object(h)
                    .map_or(0, |o| o.elements.len() as usize);
                let mut elems = Vec::with_capacity(len);
                for i in 0..len {
                    let v = u32::try_from(i)
                        .ok()
                        .and_then(|idx| vm.heap.get_object(h).and_then(|o| o.elements.get(idx)))
                        .unwrap_or(JsValue::UNDEFINED);
                    elems.push(v);
                }
                KindInfo::Array(elems)
            }
            _ => KindInfo::Invalid,
        };
        match info {
            KindInfo::Date(ms) => {
                if ms.is_nan() {
                    return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "Invalid Date is not a valid IndexedDB key",
                    ));
                }
                return Ok(IdbKey::Date(ms));
            }
            KindInfo::Binary(b) => return Ok(IdbKey::Binary(b)),
            KindInfo::Array(elems) => {
                if !seen.insert(h) {
                    return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "Cyclic array is not a valid IndexedDB key",
                    ));
                }
                let mut keys = Vec::with_capacity(elems.len());
                for elem in elems {
                    keys.push(js_to_idb_key(vm, elem, seen)?);
                }
                seen.remove(&h);
                return Ok(IdbKey::Array(keys));
            }
            KindInfo::Invalid => {}
        }
    }
    Err(create_dom_exception(
        vm,
        "DataError",
        "Value is not a valid IndexedDB key",
    ))
}

/// Converts a persisted [`IdbKey`] back into its JS representation.
pub fn idb_key_to_js(vm: &mut JsVm, key: &IdbKey) -> JsValue {
    match key {
        IdbKey::Number(n) => JsValue::from_number(*n),
        IdbKey::Date(ms) => {
            let h = vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::Date(*ms),
                Some(vm.date_proto),
            ));
            JsValue::from_object(h)
        }
        IdbKey::String(s) => JsValue::from_string(vm.heap.alloc_string(s)),
        IdbKey::Binary(bytes) => JsValue::from_object(alloc_array_buffer(vm, bytes.clone())),
        IdbKey::Array(items) => {
            let vals: Vec<JsValue> = items.iter().map(|k| idb_key_to_js(vm, k)).collect();
            JsValue::from_object(alloc_js_array(vm, &vals))
        }
    }
}

/// Parses a JS `keyPath` option (`string`, `string[]`, or `null`/`undefined`).
pub fn parse_key_path_arg(vm: &mut JsVm, val: JsValue) -> Result<Option<Vec<String>>, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    if val.is_string() {
        return Ok(Some(vec![vm.value_to_rust_string(val)]));
    }
    if let Some(h) = val.as_object() {
        if matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Array)
        ) {
            let len = vm
                .heap
                .get_object(h)
                .map_or(0, |o| o.elements.len() as usize);
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                let v = u32::try_from(i)
                    .ok()
                    .and_then(|idx| vm.heap.get_object(h).and_then(|o| o.elements.get(idx)))
                    .unwrap_or(JsValue::UNDEFINED);
                out.push(vm.value_to_rust_string(v));
            }
            if out.is_empty() {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    "Empty array keyPath is invalid",
                ));
            }
            return Ok(Some(out));
        }
    }
    Ok(Some(vec![vm.value_to_rust_string(val)]))
}

/// Converts a structured-clone JSON value to an [`IdbKey`] if valid.
pub fn json_to_idb_key(jv: &serde_json::Value) -> Option<IdbKey> {
    match jv {
        serde_json::Value::Number(n) => {
            let f = n.as_f64()?;
            if f.is_nan() {
                None
            } else {
                Some(IdbKey::Number(if f == 0.0 { 0.0 } else { f }))
            }
        }
        serde_json::Value::String(s) => Some(IdbKey::String(s.clone())),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(json_to_idb_key(item)?);
            }
            Some(IdbKey::Array(out))
        }
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(tag)) = map.get("$t") {
                if tag == "date" {
                    let ms = map.get("v")?.as_f64()?;
                    return (!ms.is_nan()).then_some(IdbKey::Date(ms));
                }
                if tag == "ab" {
                    let arr = map.get("bytes")?.as_array()?;
                    let bytes: Option<Vec<u8>> = arr
                        .iter()
                        .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                        .collect();
                    return bytes.map(IdbKey::Binary);
                }
            }
            None
        }
        _ => None,
    }
}

fn lookup_dotted_path<'a>(jv: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = jv;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Evaluates a single or compound `keyPath` against a structured-clone JSON record.
pub fn extract_key_path_from_json(jv: &serde_json::Value, key_path: &[String]) -> Option<IdbKey> {
    if key_path.is_empty() {
        return None;
    }
    if key_path.len() == 1 {
        let leaf = lookup_dotted_path(jv, &key_path[0])?;
        return json_to_idb_key(leaf);
    }
    let mut parts = Vec::with_capacity(key_path.len());
    for p in key_path {
        let leaf = lookup_dotted_path(jv, p)?;
        parts.push(json_to_idb_key(leaf)?);
    }
    Some(IdbKey::Array(parts))
}

/// Evaluates an index schema (including `multiEntry` deduplication) against a JSON record.
pub fn extract_index_keys_from_json(
    jv: &serde_json::Value,
    schema: &IdbIndexSchema,
) -> Vec<IdbKey> {
    if schema.multi_entry && schema.key_path.len() == 1 {
        if let Some(serde_json::Value::Array(items)) = lookup_dotted_path(jv, &schema.key_path[0]) {
            let mut out: Vec<IdbKey> = Vec::new();
            for item in items {
                if let Some(k) = json_to_idb_key(item) {
                    if !out
                        .iter()
                        .any(|existing| existing.idb_cmp(&k) == Ordering::Equal)
                    {
                        out.push(k);
                    }
                }
            }
            return out;
        }
    }
    extract_key_path_from_json(jv, &schema.key_path)
        .into_iter()
        .collect()
}

/// Injects an `autoIncrement` numeric key at a dotted `keyPath` on a JSON object.
pub fn inject_auto_key_into_json(jv: &mut serde_json::Value, path: &str, key_num: u64) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return false;
    }
    let mut cur = jv;
    for &seg in &parts[..parts.len() - 1] {
        let Some(map) = cur.as_object_mut() else {
            return false;
        };
        cur = map
            .entry(seg.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    let Some(map) = cur.as_object_mut() else {
        return false;
    };
    map.insert(
        parts[parts.len() - 1].to_string(),
        serde_json::Value::Number(serde_json::Number::from(key_num)),
    );
    true
}

/// W3C `IDBKeyRange` bounds predicate over [`IdbKey`].
#[derive(Debug, Clone)]
pub struct KeyRange {
    pub lower: Option<IdbKey>,
    pub upper: Option<IdbKey>,
    pub lower_open: bool,
    pub upper_open: bool,
}

impl KeyRange {
    #[must_use]
    pub fn only(k: IdbKey) -> Self {
        Self {
            lower: Some(k.clone()),
            upper: Some(k),
            lower_open: false,
            upper_open: false,
        }
    }

    #[must_use]
    pub fn matches(&self, key: &IdbKey) -> bool {
        if let Some(ref lo) = self.lower {
            match key.idb_cmp(lo) {
                Ordering::Less => return false,
                Ordering::Equal if self.lower_open => return false,
                _ => {}
            }
        }
        if let Some(ref hi) = self.upper {
            match key.idb_cmp(hi) {
                Ordering::Greater => return false,
                Ordering::Equal if self.upper_open => return false,
                _ => {}
            }
        }
        true
    }
}

/// W3C `IDBCursorDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorDirection {
    Next,
    NextUnique,
    Prev,
    PrevUnique,
}

impl CursorDirection {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "next" => Some(Self::Next),
            "nextunique" => Some(Self::NextUnique),
            "prev" => Some(Self::Prev),
            "prevunique" => Some(Self::PrevUnique),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::NextUnique => "nextunique",
            Self::Prev => "prev",
            Self::PrevUnique => "prevunique",
        }
    }

    #[must_use]
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::Prev | Self::PrevUnique)
    }

    #[must_use]
    pub fn is_unique(self) -> bool {
        matches!(self, Self::NextUnique | Self::PrevUnique)
    }
}

/// Materialized cursor entry pairing effective key, primary key, and serialized JSON value.
#[derive(Debug, Clone)]
pub struct IdbCursorEntry {
    pub key: IdbKey,
    pub primary_key: IdbKey,
    pub value_json: String,
}

/// Applies `put` or `add` onto `store` with keyPath extraction, `autoIncrement` key
/// generation, duplicate-key checking, and `unique` secondary index validation.
pub fn store_put_or_add(
    store: &mut IdbStoreSnapshot,
    mut val_json: serde_json::Value,
    explicit_key: Option<IdbKey>,
    no_overwrite: bool,
) -> Result<IdbKey, (&'static str, String)> {
    let key = if let Some(ref kp) = store.key_path {
        if explicit_key.is_some() {
            return Err((
                "DataError",
                "Explicit key is forbidden when object store has a keyPath".to_string(),
            ));
        }
        if let Some(extracted) = extract_key_path_from_json(&val_json, kp) {
            if store.auto_increment {
                if let IdbKey::Number(n) = extracted {
                    if n >= 1.0 && n.is_finite() && (n as u64) >= store.next_key {
                        store.next_key = (n as u64).saturating_add(1);
                    }
                }
            }
            extracted
        } else if store.auto_increment && kp.len() == 1 {
            let gen = store.next_key;
            store.next_key = store.next_key.saturating_add(1);
            if !inject_auto_key_into_json(&mut val_json, &kp[0], gen) {
                return Err((
                    "DataError",
                    "Cannot inject autoIncrement key into non-object value".to_string(),
                ));
            }
            IdbKey::Number(gen as f64)
        } else {
            return Err((
                "DataError",
                "Evaluating the object store's keyPath did not yield a valid key".to_string(),
            ));
        }
    } else if let Some(k) = explicit_key {
        if store.auto_increment {
            if let IdbKey::Number(n) = k {
                if n >= 1.0 && n.is_finite() && (n as u64) >= store.next_key {
                    store.next_key = (n as u64).saturating_add(1);
                }
            }
        }
        k
    } else if store.auto_increment {
        let gen = store.next_key;
        store.next_key = store.next_key.saturating_add(1);
        IdbKey::Number(gen as f64)
    } else {
        return Err((
            "DataError",
            "No key provided and store does not use keyPath or autoIncrement".to_string(),
        ));
    };

    let encoded_pk = key.to_encoded_bytes();
    if no_overwrite && store.records.contains_key(&encoded_pk) {
        return Err((
            "ConstraintError",
            "Key already exists in object store".to_string(),
        ));
    }

    // Validate every `unique: true` index BEFORE mutating `store.records`!
    for idx_schema in store.indexes.values() {
        if !idx_schema.unique {
            continue;
        }
        let new_idx_keys = extract_index_keys_from_json(&val_json, idx_schema);
        if new_idx_keys.is_empty() {
            continue;
        }
        for (other_pk, other_rec) in &store.records {
            if other_pk == &encoded_pk {
                continue;
            }
            let Ok(other_val) = serde_json::from_str::<serde_json::Value>(&other_rec.value_json)
            else {
                continue;
            };
            let existing_keys = extract_index_keys_from_json(&other_val, idx_schema);
            for nk in &new_idx_keys {
                if existing_keys
                    .iter()
                    .any(|ek| ek.idb_cmp(nk) == Ordering::Equal)
                {
                    return Err((
                        "ConstraintError",
                        format!("Unique constraint violated on index '{}'", idx_schema.name),
                    ));
                }
            }
        }
    }

    let serialized = serde_json::to_string(&val_json).unwrap_or_else(|_| "null".into());
    store.records.insert(
        encoded_pk,
        IdbRecord {
            key: key.clone(),
            value_json: serialized,
        },
    );
    Ok(key)
}

/// Collects matching primary-key records from `store` in cursor `dir` order.
#[must_use]
pub fn collect_store_entries(
    store: &IdbStoreSnapshot,
    range: Option<&KeyRange>,
    dir: CursorDirection,
) -> Vec<IdbCursorEntry> {
    let mut list: Vec<IdbCursorEntry> = store
        .records
        .values()
        .filter(|rec| range.is_none_or(|r| r.matches(&rec.key)))
        .map(|rec| IdbCursorEntry {
            key: rec.key.clone(),
            primary_key: rec.key.clone(),
            value_json: rec.value_json.clone(),
        })
        .collect();
    if dir.is_reverse() {
        list.reverse();
    }
    list
}

/// Collects matching secondary-index entries from `store` in W3C cursor `dir` order.
#[must_use]
pub fn collect_index_entries(
    store: &IdbStoreSnapshot,
    idx_schema: &IdbIndexSchema,
    range: Option<&KeyRange>,
    dir: CursorDirection,
) -> Vec<IdbCursorEntry> {
    let mut pairs: Vec<IdbCursorEntry> = Vec::new();
    for rec in store.records.values() {
        let Ok(jv) = serde_json::from_str::<serde_json::Value>(&rec.value_json) else {
            continue;
        };
        for ik in extract_index_keys_from_json(&jv, idx_schema) {
            if range.is_none_or(|r| r.matches(&ik)) {
                pairs.push(IdbCursorEntry {
                    key: ik,
                    primary_key: rec.key.clone(),
                    value_json: rec.value_json.clone(),
                });
            }
        }
    }
    pairs.sort_by(|a, b| {
        a.key
            .idb_cmp(&b.key)
            .then_with(|| a.primary_key.idb_cmp(&b.primary_key))
    });
    if dir.is_reverse() {
        pairs.reverse();
    }
    if dir.is_unique() {
        let mut deduped: Vec<IdbCursorEntry> = Vec::with_capacity(pairs.len());
        for entry in pairs {
            let dup = deduped
                .last()
                .is_some_and(|prev| prev.key.idb_cmp(&entry.key) == Ordering::Equal);
            if !dup {
                deduped.push(entry);
            }
        }
        return deduped;
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_engine_enforces_unique_multientry_and_key_range_bounds() {
        let mut store = IdbStoreSnapshot::new("users".into(), Some(vec!["id".into()]), true);
        store.indexes.insert(
            "email".into(),
            IdbIndexSchema {
                name: "email".into(),
                key_path: vec!["email".into()],
                unique: true,
                multi_entry: false,
            },
        );
        store.indexes.insert(
            "tags".into(),
            IdbIndexSchema {
                name: "tags".into(),
                key_path: vec!["tags".into()],
                unique: false,
                multi_entry: true,
            },
        );

        let k1 = store_put_or_add(
            &mut store,
            serde_json::json!({ "email": "a@ex.com", "tags": ["rust", "rust", "gpu"] }),
            None,
            true,
        )
        .expect("insert 1");
        assert_eq!(k1, IdbKey::Number(1.0));

        let dup = store_put_or_add(
            &mut store,
            serde_json::json!({ "email": "a@ex.com", "tags": ["js"] }),
            None,
            true,
        );
        assert_eq!(dup.unwrap_err().0, "ConstraintError");

        let tag_schema = store.indexes.get("tags").unwrap();
        let entries = collect_index_entries(&store, tag_schema, None, CursorDirection::Next);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, IdbKey::String("gpu".into()));
        assert_eq!(entries[1].key, IdbKey::String("rust".into()));

        let kr = KeyRange {
            lower: Some(IdbKey::Number(1.0)),
            upper: Some(IdbKey::Number(5.0)),
            lower_open: true,
            upper_open: false,
        };
        assert!(!kr.matches(&IdbKey::Number(1.0)));
        assert!(kr.matches(&IdbKey::Number(5.0)));
    }
}
