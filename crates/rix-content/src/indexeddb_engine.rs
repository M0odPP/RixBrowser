//! Pure-Rust W3C IndexedDB key conversion, key-path evaluation, `KeyRange`
//! matching, `autoIncrement` key generation, unique-index constraint enforcement,
//! and cursor record collection over `rix-storage`'s `IdbStoreSnapshot`.
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cmp::Ordering;
use std::collections::HashSet;

use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_storage::{IdbIndexSchema, IdbKey, IdbRecord, IdbStoreSnapshot};

use crate::frames::SharedFrames;
use crate::structured_clone::{
    decode_storage_payload, encode_storage_payload, CloneNode, CloneValue, StructuredClonePayload,
};
use crate::web_apis::{alloc_array_buffer, create_dom_exception};

/// Maximum valid value yielded by an IndexedDB `autoIncrement` key generator ($2^{53}$).
pub const MAX_KEY_GENERATOR: u64 = 9_007_199_254_740_992;

/// WebIDL `[EnforceRange] unsigned long` (`0..=u32::MAX`) conversion.
#[allow(dead_code)]
pub fn enforce_range_u32(vm: &mut JsVm, val: JsValue) -> Result<u32, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.type_error("Value is outside the unsigned long range"));
    }
    let t = n.trunc();
    if t < 0.0 || t > f64::from(u32::MAX) {
        return Err(vm.type_error("Value is outside the unsigned long range"));
    }
    Ok(t as u32)
}

/// WebIDL `[EnforceRange] unsigned long long` + IndexedDB `version >= 1` validation.
#[allow(dead_code)]
pub fn enforce_range_idb_version(vm: &mut JsVm, val: JsValue) -> Result<u64, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.type_error("IndexedDB version must be a finite positive integer"));
    }
    let t = n.trunc();
    if t < 1.0 || t > 9_007_199_254_740_991.0 {
        return Err(vm.type_error("IndexedDB version is out of range"));
    }
    Ok(t as u64)
}

#[inline]
fn is_ecmascript_id_start(ch: char) -> bool {
    ch == '$' || ch == '_' || ch.is_ascii_alphabetic() || (!ch.is_ascii() && ch.is_alphabetic())
}

#[inline]
fn is_ecmascript_id_part(ch: char) -> bool {
    ch == '$'
        || ch == '_'
        || ch == '\u{200C}'
        || ch == '\u{200D}'
        || ch.is_ascii_alphanumeric()
        || (!ch.is_ascii() && (ch.is_alphanumeric() || is_unicode_combining_or_connector(ch)))
}

#[inline]
fn is_unicode_combining_or_connector(ch: char) -> bool {
    let cp = ch as u32;
    (0x0300..=0x036F).contains(&cp)
        || (0x1DC0..=0x1DFF).contains(&cp)
        || (0x20D0..=0x20FF).contains(&cp)
        || (0xFE20..=0xFE2F).contains(&cp)
        || cp == 0x203F
        || cp == 0x2040
        || cp == 0x2054
        || cp == 0xFE33
        || cp == 0xFE34
        || (0xFE4D..=0xFE4F).contains(&cp)
        || cp == 0xFF3F
}

/// Returns `true` iff `path` is a valid W3C IndexedDB 3.0 key path string:
/// either `""`, or one or more ECMAScript `IdentifierName`s separated by `.`.
#[must_use]
pub fn is_valid_key_path_str(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    for segment in path.split('.') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !is_ecmascript_id_start(first) {
            return false;
        }
        if !chars.all(is_ecmascript_id_part) {
            return false;
        }
    }
    true
}

/// Result of evaluating a key path against a cloned value per W3C IDB 3.0 §4.2.
#[derive(Debug, Clone)]
pub enum KeyPathEval {
    /// Key path resolved to a valid IndexedDB key.
    Found(IdbKey),
    /// Key path did not find a property.
    NotFound,
    /// Key path resolved to a value that is NOT a valid IndexedDB key.
    InvalidKey,
}

fn clone_value_to_idb_key(
    payload: &StructuredClonePayload,
    val: &CloneValue,
    visiting: &mut HashSet<usize>,
) -> Option<IdbKey> {
    match val {
        CloneValue::Number(n) => {
            if n.is_nan() {
                None
            } else {
                Some(IdbKey::Number(if *n == 0.0 { 0.0 } else { *n }))
            }
        }
        CloneValue::String(s) => Some(IdbKey::String(s.clone())),
        CloneValue::Node(idx) => {
            let node = payload.nodes.get(*idx)?;
            match node {
                CloneNode::Date(ms) => {
                    if ms.is_nan() {
                        None
                    } else {
                        Some(IdbKey::Date(if *ms == 0.0 { 0.0 } else { *ms }))
                    }
                }
                CloneNode::ArrayBuffer { bytes, .. } => Some(IdbKey::Binary(bytes.clone())),
                CloneNode::TypedArray {
                    kind,
                    buffer_node,
                    byte_offset,
                    length,
                    ..
                } => {
                    let CloneNode::ArrayBuffer { bytes, .. } = payload.nodes.get(*buffer_node)?
                    else {
                        return None;
                    };
                    let start = (*byte_offset as usize).min(bytes.len());
                    let byte_len = (*length as usize).saturating_mul(kind.bytes_per_element());
                    let end = start.saturating_add(byte_len).min(bytes.len());
                    Some(IdbKey::Binary(bytes[start..end].to_vec()))
                }
                CloneNode::DataView {
                    buffer_node,
                    byte_offset,
                    byte_length,
                    ..
                } => {
                    let CloneNode::ArrayBuffer { bytes, .. } = payload.nodes.get(*buffer_node)?
                    else {
                        return None;
                    };
                    let start = (*byte_offset as usize).min(bytes.len());
                    let end = start.saturating_add(*byte_length as usize).min(bytes.len());
                    Some(IdbKey::Binary(bytes[start..end].to_vec()))
                }
                CloneNode::Array {
                    length, indexed, ..
                } => {
                    if !visiting.insert(*idx) {
                        return None;
                    }
                    let len = *length as usize;
                    if indexed.len() < len {
                        visiting.remove(idx);
                        return None;
                    }
                    let mut out = Vec::with_capacity(len);
                    for i in 0..len {
                        let Some((_, item)) = indexed.iter().find(|(pos, _)| *pos as usize == i)
                        else {
                            visiting.remove(idx);
                            return None;
                        };
                        let Some(k) = clone_value_to_idb_key(payload, item, visiting) else {
                            visiting.remove(idx);
                            return None;
                        };
                        out.push(k);
                    }
                    visiting.remove(idx);
                    Some(IdbKey::Array(out))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn lookup_dotted_path_in_payload(
    payload: &StructuredClonePayload,
    path: &str,
) -> Option<CloneValue> {
    if path.is_empty() {
        return Some(payload.root.clone());
    }
    let mut cur = payload.root.clone();
    for seg in path.split('.') {
        match cur {
            CloneValue::String(ref s) => {
                if seg == "length" {
                    cur = CloneValue::Number(s.encode_utf16().count() as f64);
                } else {
                    return None;
                }
            }
            CloneValue::Node(idx) => {
                let node = payload.nodes.get(idx)?;
                match node {
                    CloneNode::Object { props } => {
                        let (_, v) = props.iter().find(|(k, _)| k == seg)?;
                        cur = v.clone();
                    }
                    CloneNode::Array {
                        length,
                        indexed,
                        extra_props,
                    } => {
                        if seg == "length" {
                            cur = CloneValue::Number(f64::from(*length));
                        } else if let Ok(i) = seg.parse::<u32>() {
                            if i.to_string() == seg {
                                let (_, v) = indexed.iter().find(|(pos, _)| *pos == i)?;
                                cur = v.clone();
                            } else {
                                let (_, v) = extra_props.iter().find(|(k, _)| k == seg)?;
                                cur = v.clone();
                            }
                        } else {
                            let (_, v) = extra_props.iter().find(|(k, _)| k == seg)?;
                            cur = v.clone();
                        }
                    }
                    CloneNode::Blob {
                        bytes,
                        content_type,
                    } => match seg {
                        "size" => cur = CloneValue::Number(bytes.len() as f64),
                        "type" => cur = CloneValue::String(content_type.clone()),
                        _ => return None,
                    },
                    CloneNode::File {
                        bytes,
                        content_type,
                        name,
                        last_modified,
                    } => match seg {
                        "size" => cur = CloneValue::Number(bytes.len() as f64),
                        "type" => cur = CloneValue::String(content_type.clone()),
                        "name" => cur = CloneValue::String(name.clone()),
                        "lastModified" => cur = CloneValue::Number(*last_modified),
                        _ => return None,
                    },
                    CloneNode::BoxedString(s) if seg == "length" => {
                        cur = CloneValue::Number(s.encode_utf16().count() as f64);
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(cur)
}

/// Evaluates a single or compound `keyPath` against a [`StructuredClonePayload`].
#[must_use]
pub fn evaluate_key_path_on_payload(
    payload: &StructuredClonePayload,
    key_path: &[String],
    is_array: bool,
) -> KeyPathEval {
    if key_path.is_empty() {
        return KeyPathEval::NotFound;
    }
    if !is_array && key_path.len() == 1 {
        let Some(leaf) = lookup_dotted_path_in_payload(payload, &key_path[0]) else {
            return KeyPathEval::NotFound;
        };
        let mut visiting = HashSet::new();
        return match clone_value_to_idb_key(payload, &leaf, &mut visiting) {
            Some(k) => KeyPathEval::Found(k),
            None => KeyPathEval::InvalidKey,
        };
    }
    let mut parts = Vec::with_capacity(key_path.len());
    for p in key_path {
        let Some(leaf) = lookup_dotted_path_in_payload(payload, p) else {
            return KeyPathEval::NotFound;
        };
        let mut visiting = HashSet::new();
        match clone_value_to_idb_key(payload, &leaf, &mut visiting) {
            Some(k) => parts.push(k),
            None => return KeyPathEval::InvalidKey,
        }
    }
    KeyPathEval::Found(IdbKey::Array(parts))
}

/// Evaluates an index schema (including `multiEntry` deduplication) against a payload.
#[must_use]
pub fn extract_index_keys_from_payload(
    payload: &StructuredClonePayload,
    schema: &IdbIndexSchema,
) -> Vec<IdbKey> {
    if schema.multi_entry && !schema.key_path_is_array && schema.key_path.len() == 1 {
        if let Some(CloneValue::Node(idx)) =
            lookup_dotted_path_in_payload(payload, &schema.key_path[0])
        {
            if let Some(CloneNode::Array { indexed, .. }) = payload.nodes.get(idx) {
                let mut out: Vec<IdbKey> = Vec::new();
                for (_, item) in indexed {
                    let mut visiting = HashSet::new();
                    if let Some(k) = clone_value_to_idb_key(payload, item, &mut visiting) {
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
    }
    match evaluate_key_path_on_payload(payload, &schema.key_path, schema.key_path_is_array) {
        KeyPathEval::Found(k) => vec![k],
        KeyPathEval::NotFound | KeyPathEval::InvalidKey => Vec::new(),
    }
}

/// Checks per W3C IDB 3.0 §4.2 whether an `autoIncrement` key can be injected at `path`.
#[must_use]
pub fn can_inject_key_into_payload(payload: &StructuredClonePayload, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let parts: Vec<&str> = path.split('.').collect();
    let CloneValue::Node(mut cur_idx) = payload.root else {
        return false;
    };
    for &seg in &parts[..parts.len() - 1] {
        let Some(CloneNode::Object { props }) = payload.nodes.get(cur_idx) else {
            return false;
        };
        if let Some((_, val)) = props.iter().find(|(k, _)| k == seg) {
            let CloneValue::Node(next_idx) = *val else {
                return false;
            };
            cur_idx = next_idx;
        } else {
            return true;
        }
    }
    matches!(payload.nodes.get(cur_idx), Some(CloneNode::Object { .. }))
}

/// Injects an `autoIncrement` numeric key at dotted `path` into `payload`.
pub fn inject_key_into_payload(
    payload: &mut StructuredClonePayload,
    path: &str,
    key_num: u64,
) -> bool {
    if !can_inject_key_into_payload(payload, path) {
        return false;
    }
    let parts: Vec<&str> = path.split('.').collect();
    let CloneValue::Node(mut cur_idx) = payload.root else {
        return false;
    };
    for &seg in &parts[..parts.len() - 1] {
        let existing_next = match payload.nodes.get(cur_idx) {
            Some(CloneNode::Object { props }) => {
                props
                    .iter()
                    .find(|(k, _)| k == seg)
                    .and_then(|(_, v)| match *v {
                        CloneValue::Node(ni) => Some(ni),
                        _ => None,
                    })
            }
            _ => return false,
        };
        if let Some(ni) = existing_next {
            cur_idx = ni;
        } else {
            let new_idx = payload.nodes.len();
            payload.nodes.push(CloneNode::Object { props: Vec::new() });
            if let Some(CloneNode::Object { props }) = payload.nodes.get_mut(cur_idx) {
                props.push((seg.to_string(), CloneValue::Node(new_idx)));
            }
            cur_idx = new_idx;
        }
    }
    let leaf = parts[parts.len() - 1];
    let Some(CloneNode::Object { props }) = payload.nodes.get_mut(cur_idx) else {
        return false;
    };
    let val = CloneValue::Number(key_num as f64);
    if let Some((_, slot)) = props.iter_mut().find(|(k, _)| k == leaf) {
        *slot = val;
    } else {
        props.push((leaf.to_string(), val));
    }
    true
}

fn update_key_generator(store: &mut IdbStoreSnapshot, key: &IdbKey) {
    if !store.auto_increment {
        return;
    }
    if let IdbKey::Number(n) = *key {
        let floored = n.floor();
        if floored >= 1.0 {
            if floored >= MAX_KEY_GENERATOR as f64 {
                store.next_key = MAX_KEY_GENERATOR + 1;
            } else {
                let candidate = (floored as u64).saturating_add(1);
                if candidate > store.next_key {
                    store.next_key = candidate;
                }
            }
        }
    }
}

/// Decodes an [`IdbRecord`] into a [`StructuredClonePayload`].
#[must_use]
pub fn decode_record_payload(rec: &IdbRecord) -> Option<StructuredClonePayload> {
    if !rec.value_bytes.is_empty() {
        return decode_storage_payload(&rec.value_bytes);
    }
    if !rec.value_json.is_empty() {
        let jv: serde_json::Value = serde_json::from_str(&rec.value_json).ok()?;
        return Some(json_value_to_payload(&jv));
    }
    None
}

fn json_value_to_payload(jv: &serde_json::Value) -> StructuredClonePayload {
    let mut nodes = Vec::new();
    let root = json_to_clone_val(jv, &mut nodes);
    StructuredClonePayload {
        root,
        nodes,
        transferred_ports: Vec::new(),
        has_transferred_media_handle: false,
    }
}

fn json_to_clone_val(jv: &serde_json::Value, nodes: &mut Vec<CloneNode>) -> CloneValue {
    match jv {
        serde_json::Value::Null => CloneValue::Null,
        serde_json::Value::Bool(b) => CloneValue::Bool(*b),
        serde_json::Value::Number(n) => CloneValue::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => CloneValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            let mut indexed = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                indexed.push((i as u32, json_to_clone_val(item, nodes)));
            }
            nodes[idx] = CloneNode::Array {
                length: arr.len() as u32,
                indexed,
                extra_props: Vec::new(),
            };
            CloneValue::Node(idx)
        }
        serde_json::Value::Object(map) => {
            let idx = nodes.len();
            nodes.push(CloneNode::Placeholder);
            let mut props = Vec::with_capacity(map.len());
            for (k, v) in map {
                props.push((k.clone(), json_to_clone_val(v, nodes)));
            }
            nodes[idx] = CloneNode::Object { props };
            CloneValue::Node(idx)
        }
    }
}

/// Synchronously validates `put`/`add` key-path / explicit-key / auto-increment constraints.
pub fn validate_put_or_add_sync(
    store: &IdbStoreSnapshot,
    payload: &StructuredClonePayload,
    explicit_key: Option<IdbKey>,
) -> Result<Option<IdbKey>, (&'static str, String)> {
    if let Some(ref kp) = store.key_path {
        if explicit_key.is_some() {
            return Err((
                "DataError",
                "Explicit key is forbidden when object store has a keyPath".to_string(),
            ));
        }
        match evaluate_key_path_on_payload(payload, kp, store.key_path_is_array) {
            KeyPathEval::Found(k) => Ok(Some(k)),
            KeyPathEval::InvalidKey => Err((
                "DataError",
                "Evaluating the object store's keyPath yielded an invalid key".to_string(),
            )),
            KeyPathEval::NotFound => {
                if store.auto_increment && !store.key_path_is_array && kp.len() == 1 {
                    if !can_inject_key_into_payload(payload, &kp[0]) {
                        return Err((
                            "DataError",
                            "Cannot inject autoIncrement key into value".to_string(),
                        ));
                    }
                    Ok(None)
                } else {
                    Err((
                        "DataError",
                        "Evaluating the object store's keyPath did not yield a valid key"
                            .to_string(),
                    ))
                }
            }
        }
    } else if let Some(k) = explicit_key {
        Ok(Some(k))
    } else if store.auto_increment {
        Ok(None)
    } else {
        Err((
            "DataError",
            "No key provided and store does not use keyPath or autoIncrement".to_string(),
        ))
    }
}

/// Applies `put` or `add` onto `store` with key generation (`1..=2^53`), key injection,
/// duplicate-key checking, and `unique` secondary index validation.
/// Per W3C IDB §5.6, failed writes leave `store.next_key` and `store.records` untouched.
pub fn store_put_or_add_payload(
    store: &mut IdbStoreSnapshot,
    mut payload: StructuredClonePayload,
    resolved_key: Option<IdbKey>,
    no_overwrite: bool,
) -> Result<IdbKey, (&'static str, String)> {
    let mut generated_key: Option<u64> = None;
    let key = if let Some(k) = resolved_key {
        k
    } else {
        if store.next_key > MAX_KEY_GENERATOR {
            return Err((
                "ConstraintError",
                "Key generator reached maximum value (2^53)".to_string(),
            ));
        }
        let gen = store.next_key;
        if let Some(ref kp) = store.key_path {
            if !store.key_path_is_array
                && kp.len() == 1
                && !inject_key_into_payload(&mut payload, &kp[0], gen)
            {
                return Err((
                    "DataError",
                    "Cannot inject autoIncrement key into non-object value".to_string(),
                ));
            }
        }
        generated_key = Some(gen);
        IdbKey::Number(gen as f64)
    };

    let encoded_pk = key.to_encoded_bytes();
    if no_overwrite && store.records.contains_key(&encoded_pk) {
        return Err((
            "ConstraintError",
            "Key already exists in object store".to_string(),
        ));
    }

    for idx_schema in store.indexes.values() {
        if !idx_schema.unique {
            continue;
        }
        let new_idx_keys = extract_index_keys_from_payload(&payload, idx_schema);
        if new_idx_keys.is_empty() {
            continue;
        }
        for (other_pk, other_rec) in &store.records {
            if other_pk == &encoded_pk {
                continue;
            }
            let Some(other_payload) = decode_record_payload(other_rec) else {
                continue;
            };
            let existing_keys = extract_index_keys_from_payload(&other_payload, idx_schema);
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

    // Commit key generator update ONLY after all uniqueness constraints succeed.
    if let Some(gen) = generated_key {
        store.next_key = gen.saturating_add(1);
    } else {
        update_key_generator(store, &key);
    }

    let value_bytes = encode_storage_payload(&payload);
    store.records.insert(
        encoded_pk,
        IdbRecord {
            key: key.clone(),
            value_json: String::new(),
            value_bytes,
        },
    );
    Ok(key)
}

/// Validates a newly created `unique` secondary index against all existing records in `store`.
pub fn validate_index_unique_constraint(
    store: &IdbStoreSnapshot,
    idx_schema: &IdbIndexSchema,
) -> Result<(), (&'static str, String)> {
    if !idx_schema.unique {
        return Ok(());
    }
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for rec in store.records.values() {
        let Some(payload) = decode_record_payload(rec) else {
            continue;
        };
        for ik in extract_index_keys_from_payload(&payload, idx_schema) {
            if !seen.insert(ik.to_encoded_bytes()) {
                return Err((
                    "ConstraintError",
                    format!(
                        "Existing records violate unique constraint on index '{}'",
                        idx_schema.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub fn alloc_js_array(vm: &mut JsVm, items: &[JsValue]) -> HeapHandle {
    let arr = vm.alloc_array();
    for &v in items {
        vm.heap.object_push_element(arr, v);
    }
    arr
}

/// Allocates a W3C `DOMStringList` sorted by UTF-16 code units, inheriting from
/// `dom_string_list_proto` (with `.length`, indexed properties `0..n`, `.item(i)`, `.contains(s)`).
pub fn alloc_dom_string_list(
    vm: &mut JsVm,
    dom_string_list_proto: HeapHandle,
    mut names: Vec<String>,
) -> HeapHandle {
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    names.dedup();
    let obj = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(dom_string_list_proto),
    ));
    for (idx, s) in names.iter().enumerate() {
        let v = JsValue::from_string(vm.heap.alloc_string(s));
        let k = vm.heap.intern_key(&idx.to_string());
        vm.heap.object_define_data(
            obj,
            k,
            v,
            PropertyAttrs::ENUMERABLE | PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    let k_len = vm.heap.intern_key("__rix_dsl_len__");
    vm.heap.object_define_data(
        obj,
        k_len,
        JsValue::from_number(names.len() as f64),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    obj
}

fn read_buffer_bytes_for_key(
    vm: &mut JsVm,
    frames: Option<&SharedFrames>,
    buf_h: HeapHandle,
) -> Result<Vec<u8>, JsException> {
    let Some(o) = vm.heap.get_object(buf_h) else {
        return Err(create_dom_exception(
            vm,
            "DataError",
            "Buffer is not a valid IndexedDB key",
        ));
    };
    let detached = o.rare.as_deref().is_some_and(|r| r.ab_detached)
        || frames.is_some_and(|f| f.borrow().is_buffer_detached(buf_h));
    if detached {
        return Err(create_dom_exception(
            vm,
            "DataError",
            "Detached ArrayBuffer is not a valid IndexedDB key",
        ));
    }
    if let Some(shared) = o.rare.as_deref().and_then(|r| r.shared_mem.as_ref()) {
        let bytes = shared.data.lock().map(|g| g.clone()).unwrap_or_default();
        return Ok(bytes);
    }
    match &o.kind {
        ObjectKind::ArrayBuffer(b) => Ok((**b).clone()),
        _ => Err(create_dom_exception(
            vm,
            "DataError",
            "Buffer is not a valid IndexedDB key",
        )),
    }
}

/// Converts a JS value to a valid W3C IndexedDB 3.0 key, throwing `DataError` on invalid values.
pub fn js_to_idb_key(
    vm: &mut JsVm,
    frames: Option<&SharedFrames>,
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
            ArrayBuffer,
            TypedArray {
                buffer: HeapHandle,
                byte_offset: u32,
                length: u32,
                bpe: usize,
                auto_length: bool,
            },
            DataView {
                buffer: HeapHandle,
                byte_offset: u32,
                byte_length: u32,
                auto_length: bool,
            },
            Array,
            Invalid,
        }
        let info = match vm.heap.get_object(h) {
            Some(o) => match &o.kind {
                ObjectKind::Date(ms) => KindInfo::Date(*ms),
                ObjectKind::ArrayBuffer(_) => KindInfo::ArrayBuffer,
                ObjectKind::TypedArray(ta) => KindInfo::TypedArray {
                    buffer: ta.buffer,
                    byte_offset: ta.byte_offset,
                    length: ta.length,
                    bpe: ta.kind.bytes_per_element(),
                    auto_length: o.rare.as_deref().is_some_and(|r| r.view_auto_length),
                },
                ObjectKind::DataView(dv) => KindInfo::DataView {
                    buffer: dv.buffer,
                    byte_offset: dv.byte_offset,
                    byte_length: dv.byte_length,
                    auto_length: o.rare.as_deref().is_some_and(|r| r.view_auto_length),
                },
                ObjectKind::Array => KindInfo::Array,
                _ => KindInfo::Invalid,
            },
            None => KindInfo::Invalid,
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
                return Ok(IdbKey::Date(if ms == 0.0 { 0.0 } else { ms }));
            }
            KindInfo::ArrayBuffer => {
                let bytes = read_buffer_bytes_for_key(vm, frames, h)?;
                return Ok(IdbKey::Binary(bytes));
            }
            KindInfo::TypedArray {
                buffer,
                byte_offset,
                length,
                bpe,
                auto_length,
            } => {
                let raw = read_buffer_bytes_for_key(vm, frames, buffer)?;
                let start = byte_offset as usize;
                let end = if auto_length {
                    if start > raw.len() {
                        return Err(create_dom_exception(
                            vm,
                            "DataError",
                            "TypedArray key view is out of bounds",
                        ));
                    }
                    let count = (raw.len() - start) / bpe;
                    start + count * bpe
                } else {
                    let e = start.saturating_add((length as usize).saturating_mul(bpe));
                    if e > raw.len() {
                        return Err(create_dom_exception(
                            vm,
                            "DataError",
                            "TypedArray key view is out of bounds",
                        ));
                    }
                    e
                };
                return Ok(IdbKey::Binary(raw[start..end].to_vec()));
            }
            KindInfo::DataView {
                buffer,
                byte_offset,
                byte_length,
                auto_length,
            } => {
                let raw = read_buffer_bytes_for_key(vm, frames, buffer)?;
                let start = byte_offset as usize;
                let end = if auto_length {
                    if start > raw.len() {
                        return Err(create_dom_exception(
                            vm,
                            "DataError",
                            "DataView key is out of bounds",
                        ));
                    }
                    raw.len()
                } else {
                    let e = start.saturating_add(byte_length as usize);
                    if e > raw.len() {
                        return Err(create_dom_exception(
                            vm,
                            "DataError",
                            "DataView key is out of bounds",
                        ));
                    }
                    e
                };
                return Ok(IdbKey::Binary(raw[start..end].to_vec()));
            }
            KindInfo::Array => {
                if !seen.insert(h) {
                    return Err(create_dom_exception(
                        vm,
                        "DataError",
                        "Cyclic array is not a valid IndexedDB key",
                    ));
                }
                let k_len = vm.heap.intern_key("length");
                let len_val = vm.get_property_by_key(h, k_len)?;
                let len = vm.to_u32(len_val)? as usize;
                let mut keys = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    let key_s = i.to_string();
                    let pk = vm.heap.intern_key(&key_s);
                    let has_own = vm
                        .heap
                        .get_object(h)
                        .is_some_and(|o| o.elements.has(i as u32))
                        || vm.heap.object_get_own(h, pk, &vm.shapes).is_some();
                    if !has_own {
                        seen.remove(&h);
                        return Err(create_dom_exception(
                            vm,
                            "DataError",
                            "Sparse array is not a valid IndexedDB key",
                        ));
                    }
                    let elem = match vm.get_property_by_key(h, pk) {
                        Ok(v) => v,
                        Err(e) => {
                            seen.remove(&h);
                            return Err(e);
                        }
                    };
                    match js_to_idb_key(vm, frames, elem, seen) {
                        Ok(k) => keys.push(k),
                        Err(e) => {
                            seen.remove(&h);
                            return Err(e);
                        }
                    }
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

/// Performs WebIDL argument conversion of `keyPath` (`DOMString | sequence<DOMString> | null`)
/// WITHOUT running W3C IDB syntax validation yet (so callers can enforce exact W3C exception
/// ordering such as `TransactionInactiveError`/`ConstraintError` before `SyntaxError`).
pub fn parse_key_path_raw(
    vm: &mut JsVm,
    val: JsValue,
    allow_null: bool,
) -> Result<Option<(Vec<String>, bool)>, JsException> {
    if val.is_undefined() || (allow_null && val.is_null()) {
        return Ok(None);
    }
    if val.is_string() {
        return Ok(Some((vec![vm.value_to_rust_string(val)], false)));
    }
    if let Some(h) = val.as_object() {
        let is_array = matches!(
            vm.heap.get_object(h).map(|o| &o.kind),
            Some(ObjectKind::Array)
        );
        let is_iterable = is_array || {
            let sym_iter = vm.key_symbol_iterator;
            vm.get_property_by_key(h, sym_iter)?
                .as_object()
                .is_some_and(|f| vm.is_callable_handle(f))
        };
        if is_iterable {
            let raw_items = vm.collect_iterable(val)?;
            let mut out = Vec::with_capacity(raw_items.len());
            for item in raw_items {
                out.push(vm.to_js_string(item)?);
            }
            return Ok(Some((out, true)));
        }
    }
    Ok(Some((vec![vm.to_js_string(val)?], false)))
}

/// Validates W3C IndexedDB 3.0 syntax of an already WebIDL-converted `keyPath`.
pub fn validate_key_path_syntax(
    vm: &mut JsVm,
    paths: &[String],
    is_array: bool,
) -> Result<(), JsException> {
    if is_array && paths.is_empty() {
        return Err(create_dom_exception(
            vm,
            "SyntaxError",
            "Empty array keyPath is invalid",
        ));
    }
    for s in paths {
        if !is_valid_key_path_str(s) {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Invalid keyPath '{s}'"),
            ));
        }
    }
    Ok(())
}

/// Parses and validates a JS `keyPath` option (`string | sequence<string> | null`).
#[allow(dead_code)]
pub fn parse_key_path_arg(
    vm: &mut JsVm,
    val: JsValue,
    allow_null: bool,
) -> Result<Option<(Vec<String>, bool)>, JsException> {
    let raw = parse_key_path_raw(vm, val, allow_null)?;
    if let Some((ref paths, is_array)) = raw {
        validate_key_path_syntax(vm, paths, is_array)?;
    }
    Ok(raw)
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

/// Materialized cursor / `getAllRecords` entry pairing effective key, primary key, and payload.
#[derive(Debug, Clone)]
pub struct IdbCursorEntry {
    pub key: IdbKey,
    pub primary_key: IdbKey,
    pub value_bytes: Vec<u8>,
    pub value_json: String,
}

/// Live-position cursor step operation (avoids snapshotting entire stores in memory!).
#[derive(Debug, Clone)]
pub enum CursorStepRequest {
    /// Initial cursor open: find the first matching record in `range` along `direction`.
    Initial,
    /// `cursor.continue(target_key?)`: advance past current position (or seek to `target_key`).
    Continue(Option<IdbKey>),
    /// `cursor.advance(count)`: advance `count` steps (`>= 1`) from current position.
    Advance(usize),
    /// `cursor.continuePrimaryKey(target_key, target_pk)`: seek a non-unique index cursor.
    ContinuePrimaryKey(IdbKey, IdbKey),
}

/// Steps a live object-store cursor in O(log N + steps) over `store.records` (`BTreeMap`),
/// cloning ONLY the single winning record's payload (or zero bytes when `key_only == true`).
#[must_use]
pub fn step_store_cursor(
    store: &IdbStoreSnapshot,
    range: Option<&KeyRange>,
    dir: CursorDirection,
    cur_key: Option<&IdbKey>,
    step: &CursorStepRequest,
    key_only: bool,
) -> Option<IdbCursorEntry> {
    use std::ops::Bound;

    let mut remaining = match *step {
        CursorStepRequest::Advance(n) => n.max(1),
        _ => 1,
    };

    let make_entry = |rec: &IdbRecord| IdbCursorEntry {
        key: rec.key.clone(),
        primary_key: rec.key.clone(),
        value_bytes: if key_only {
            Vec::new()
        } else {
            rec.value_bytes.clone()
        },
        value_json: if key_only {
            String::new()
        } else {
            rec.value_json.clone()
        },
    };

    if !dir.is_reverse() {
        let mut lo_bound: Bound<Vec<u8>> = match range.and_then(|r| r.lower.as_ref()) {
            Some(lo) => {
                if range.is_some_and(|r| r.lower_open) {
                    Bound::Excluded(lo.to_encoded_bytes())
                } else {
                    Bound::Included(lo.to_encoded_bytes())
                }
            }
            None => Bound::Unbounded,
        };
        let step_lo: Option<Bound<Vec<u8>>> = match step {
            CursorStepRequest::Continue(Some(tk)) => Some(Bound::Included(tk.to_encoded_bytes())),
            CursorStepRequest::Continue(None) | CursorStepRequest::Advance(_) => {
                cur_key.map(|ck| Bound::Excluded(ck.to_encoded_bytes()))
            }
            _ => None,
        };
        if let Some(sl) = step_lo {
            lo_bound = match (&lo_bound, &sl) {
                (Bound::Unbounded, _) => sl,
                (Bound::Included(a), Bound::Included(b)) => {
                    if b > a {
                        sl
                    } else {
                        lo_bound
                    }
                }
                (Bound::Included(a), Bound::Excluded(b)) => {
                    if b >= a {
                        sl
                    } else {
                        lo_bound
                    }
                }
                (Bound::Excluded(a), Bound::Included(b)) => {
                    if b > a {
                        sl
                    } else {
                        lo_bound
                    }
                }
                (Bound::Excluded(a), Bound::Excluded(b)) => {
                    if b > a {
                        sl
                    } else {
                        lo_bound
                    }
                }
                (_, Bound::Unbounded) => lo_bound,
            };
        }

        for (_, rec) in store.records.range((lo_bound, Bound::Unbounded)) {
            if let Some(r) = range {
                if let Some(ref hi) = r.upper {
                    match rec.key.idb_cmp(hi) {
                        Ordering::Greater => break,
                        Ordering::Equal if r.upper_open => break,
                        _ => {}
                    }
                }
                if !r.matches(&rec.key) {
                    continue;
                }
            }
            if let Some(ck) = cur_key {
                if rec.key.idb_cmp(ck) != Ordering::Greater {
                    continue;
                }
            }
            if let CursorStepRequest::Continue(Some(ref tk)) = step {
                if rec.key.idb_cmp(tk) == Ordering::Less {
                    continue;
                }
            }
            remaining -= 1;
            if remaining == 0 {
                return Some(make_entry(rec));
            }
        }
        None
    } else {
        let mut hi_bound: Bound<Vec<u8>> = match range.and_then(|r| r.upper.as_ref()) {
            Some(hi) => {
                if range.is_some_and(|r| r.upper_open) {
                    Bound::Excluded(hi.to_encoded_bytes())
                } else {
                    Bound::Included(hi.to_encoded_bytes())
                }
            }
            None => Bound::Unbounded,
        };
        let step_hi: Option<Bound<Vec<u8>>> = match step {
            CursorStepRequest::Continue(Some(tk)) => Some(Bound::Included(tk.to_encoded_bytes())),
            CursorStepRequest::Continue(None) | CursorStepRequest::Advance(_) => {
                cur_key.map(|ck| Bound::Excluded(ck.to_encoded_bytes()))
            }
            _ => None,
        };
        if let Some(sh) = step_hi {
            hi_bound = match (&hi_bound, &sh) {
                (Bound::Unbounded, _) => sh,
                (Bound::Included(a), Bound::Included(b)) => {
                    if b < a {
                        sh
                    } else {
                        hi_bound
                    }
                }
                (Bound::Included(a), Bound::Excluded(b)) => {
                    if b <= a {
                        sh
                    } else {
                        hi_bound
                    }
                }
                (Bound::Excluded(a), Bound::Included(b)) => {
                    if b < a {
                        sh
                    } else {
                        hi_bound
                    }
                }
                (Bound::Excluded(a), Bound::Excluded(b)) => {
                    if b < a {
                        sh
                    } else {
                        hi_bound
                    }
                }
                (_, Bound::Unbounded) => hi_bound,
            };
        }

        for (_, rec) in store.records.range((Bound::Unbounded, hi_bound)).rev() {
            if let Some(r) = range {
                if let Some(ref lo) = r.lower {
                    match rec.key.idb_cmp(lo) {
                        Ordering::Less => break,
                        Ordering::Equal if r.lower_open => break,
                        _ => {}
                    }
                }
                if !r.matches(&rec.key) {
                    continue;
                }
            }
            if let Some(ck) = cur_key {
                if rec.key.idb_cmp(ck) != Ordering::Less {
                    continue;
                }
            }
            if let CursorStepRequest::Continue(Some(ref tk)) = step {
                if rec.key.idb_cmp(tk) == Ordering::Greater {
                    continue;
                }
            }
            remaining -= 1;
            if remaining == 0 {
                return Some(make_entry(rec));
            }
        }
        None
    }
}

/// Steps a live secondary-index cursor without cloning record payloads during scanning;
/// ONLY the single matching record's payload is cloned (or zero bytes when `key_only == true`).
#[must_use]
pub fn step_index_cursor(
    store: &IdbStoreSnapshot,
    idx_schema: &IdbIndexSchema,
    range: Option<&KeyRange>,
    dir: CursorDirection,
    cur_pos: Option<(&IdbKey, &IdbKey)>,
    step: &CursorStepRequest,
    key_only: bool,
) -> Option<IdbCursorEntry> {
    let mut keys_only: Vec<(IdbKey, IdbKey, &Vec<u8>)> = Vec::new();
    for (pk_bytes, rec) in &store.records {
        let Some(payload) = decode_record_payload(rec) else {
            continue;
        };
        for ik in extract_index_keys_from_payload(&payload, idx_schema) {
            if range.is_none_or(|r| r.matches(&ik)) {
                keys_only.push((ik, rec.key.clone(), pk_bytes));
            }
        }
    }
    match dir {
        CursorDirection::Next | CursorDirection::NextUnique => {
            keys_only.sort_by(|a, b| a.0.idb_cmp(&b.0).then_with(|| a.1.idb_cmp(&b.1)));
        }
        CursorDirection::Prev => {
            keys_only.sort_by(|a, b| b.0.idb_cmp(&a.0).then_with(|| b.1.idb_cmp(&a.1)));
        }
        CursorDirection::PrevUnique => {
            keys_only.sort_by(|a, b| b.0.idb_cmp(&a.0).then_with(|| a.1.idb_cmp(&b.1)));
        }
    }
    if dir.is_unique() {
        let mut deduped: Vec<(IdbKey, IdbKey, &Vec<u8>)> = Vec::with_capacity(keys_only.len());
        for item in keys_only {
            let dup = deduped
                .last()
                .is_some_and(|prev| prev.0.idb_cmp(&item.0) == Ordering::Equal);
            if !dup {
                deduped.push(item);
            }
        }
        keys_only = deduped;
    }

    let mut remaining = match *step {
        CursorStepRequest::Advance(n) => n.max(1),
        _ => 1,
    };

    for (ik, pk, pk_bytes) in keys_only {
        if let Some((ck, cpk)) = cur_pos {
            let past_cur = match dir {
                CursorDirection::Next => match ik.idb_cmp(ck) {
                    Ordering::Greater => true,
                    Ordering::Equal => pk.idb_cmp(cpk) == Ordering::Greater,
                    Ordering::Less => false,
                },
                CursorDirection::NextUnique => ik.idb_cmp(ck) == Ordering::Greater,
                CursorDirection::Prev => match ik.idb_cmp(ck) {
                    Ordering::Less => true,
                    Ordering::Equal => pk.idb_cmp(cpk) == Ordering::Less,
                    Ordering::Greater => false,
                },
                CursorDirection::PrevUnique => ik.idb_cmp(ck) == Ordering::Less,
            };
            if !past_cur {
                continue;
            }
        }
        match step {
            CursorStepRequest::Initial
            | CursorStepRequest::Continue(None)
            | CursorStepRequest::Advance(_) => {}
            CursorStepRequest::Continue(Some(tk)) => {
                let reached = if dir.is_reverse() {
                    ik.idb_cmp(tk) != Ordering::Greater
                } else {
                    ik.idb_cmp(tk) != Ordering::Less
                };
                if !reached {
                    continue;
                }
            }
            CursorStepRequest::ContinuePrimaryKey(tk, tpk) => {
                let cmp_k = ik.idb_cmp(tk);
                let cmp_pk = pk.idb_cmp(tpk);
                let reached = if dir.is_reverse() {
                    cmp_k == Ordering::Less
                        || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Greater)
                } else {
                    cmp_k == Ordering::Greater
                        || (cmp_k == Ordering::Equal && cmp_pk != Ordering::Less)
                };
                if !reached {
                    continue;
                }
            }
        }
        remaining -= 1;
        if remaining == 0 {
            let rec = store.records.get(pk_bytes)?;
            return Some(IdbCursorEntry {
                key: ik,
                primary_key: pk,
                value_bytes: if key_only {
                    Vec::new()
                } else {
                    rec.value_bytes.clone()
                },
                value_json: if key_only {
                    String::new()
                } else {
                    rec.value_json.clone()
                },
            });
        }
    }
    None
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
            value_bytes: rec.value_bytes.clone(),
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
        let Some(payload) = decode_record_payload(rec) else {
            continue;
        };
        for ik in extract_index_keys_from_payload(&payload, idx_schema) {
            if range.is_none_or(|r| r.matches(&ik)) {
                pairs.push(IdbCursorEntry {
                    key: ik,
                    primary_key: rec.key.clone(),
                    value_bytes: rec.value_bytes.clone(),
                    value_json: rec.value_json.clone(),
                });
            }
        }
    }
    match dir {
        CursorDirection::Next | CursorDirection::NextUnique => {
            pairs.sort_by(|a, b| {
                a.key
                    .idb_cmp(&b.key)
                    .then_with(|| a.primary_key.idb_cmp(&b.primary_key))
            });
        }
        CursorDirection::Prev => {
            pairs.sort_by(|a, b| {
                b.key
                    .idb_cmp(&a.key)
                    .then_with(|| b.primary_key.idb_cmp(&a.primary_key))
            });
        }
        CursorDirection::PrevUnique => {
            pairs.sort_by(|a, b| {
                b.key
                    .idb_cmp(&a.key)
                    .then_with(|| a.primary_key.idb_cmp(&b.primary_key))
            });
        }
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
                key_path_is_array: false,
                unique: true,
                multi_entry: false,
            },
        );
        store.indexes.insert(
            "tags".into(),
            IdbIndexSchema {
                name: "tags".into(),
                key_path: vec!["tags".into()],
                key_path_is_array: false,
                unique: false,
                multi_entry: true,
            },
        );

        let k1 = store_put_or_add_payload(
            &mut store,
            json_value_to_payload(
                &serde_json::json!({ "email": "a@ex.com", "tags": ["rust", "rust", "gpu"] }),
            ),
            None,
            true,
        )
        .expect("insert 1");
        assert_eq!(k1, IdbKey::Number(1.0));

        let dup = store_put_or_add_payload(
            &mut store,
            json_value_to_payload(&serde_json::json!({ "email": "a@ex.com", "tags": ["js"] })),
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

        assert!(is_valid_key_path_str(""));
        assert!(is_valid_key_path_str("my.køi"));
        assert!(!is_valid_key_path_str("j a"));
        assert!(!is_valid_key_path_str("1a"));

        let mut pstore = IdbStoreSnapshot::new("p".into(), Some(vec!["id".into()]), true);
        let payload = StructuredClonePayload {
            root: CloneValue::Node(0),
            nodes: vec![CloneNode::Object {
                props: vec![("v".into(), CloneValue::String("ok".into()))],
            }],
            transferred_ports: Vec::new(),
            has_transferred_media_handle: false,
        };
        let rk = validate_put_or_add_sync(&pstore, &payload, None).expect("sync ok");
        let gen_k = store_put_or_add_payload(&mut pstore, payload, rk, true).expect("put ok");
        assert_eq!(gen_k, IdbKey::Number(1.0));
    }
}
