//! Durable `redb`-backed per-origin `CacheStorage` and transactional `IndexedDB`
//! persistence for RixBrowser (`#![deny(unsafe_code)]`).
//!
//! Reference: Spec 06 §3, Issue #24.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::StorageEngine;
use crate::error::Result;

pub(crate) const TABLE_CACHE_STORAGE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("cache_storage_v1");
pub(crate) const TABLE_INDEXED_DB: TableDefinition<&str, &[u8]> =
    TableDefinition::new("indexed_db_v1");

// ---------------------------------------------------------------------------
// CacheStorage (per-origin named HTTP response caches on `redb`)
// ---------------------------------------------------------------------------

/// Serialized HTTP response entry stored inside a named `Cache`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedHttpResponse {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NamedCacheBucket {
    entries: BTreeMap<String, CachedHttpResponse>,
}

fn cache_storage_key(origin: &str, cache_name: &str) -> String {
    format!("{origin}\0{cache_name}")
}

fn cache_origin_prefix(origin: &str) -> String {
    format!("{origin}\0")
}

// ---------------------------------------------------------------------------
// IndexedDB W3C Key Model & Durable Per-Origin Snapshots
// ---------------------------------------------------------------------------

/// W3C IndexedDB 3.0 key with exact total ordering:
/// `Number < Date < String < Binary < Array`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IdbKey {
    Number(f64),
    Date(f64),
    String(String),
    Binary(Vec<u8>),
    Array(Vec<IdbKey>),
}

impl Eq for IdbKey {}

impl IdbKey {
    #[inline]
    const fn type_rank(&self) -> u8 {
        match self {
            Self::Number(_) => 0,
            Self::Date(_) => 1,
            Self::String(_) => 2,
            Self::Binary(_) => 3,
            Self::Array(_) => 4,
        }
    }

    /// Compares two keys according to W3C IndexedDB 3.0 §2.4 key comparison rules.
    #[must_use]
    pub fn idb_cmp(&self, other: &Self) -> Ordering {
        let ra = self.type_rank();
        let rb = other.type_rank();
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, other) {
            (Self::Number(a), Self::Number(b)) | (Self::Date(a), Self::Date(b)) => {
                let na = if *a == 0.0 { 0.0 } else { *a };
                let nb = if *b == 0.0 { 0.0 } else { *b };
                na.total_cmp(&nb)
            }
            (Self::String(a), Self::String(b)) => a.encode_utf16().cmp(b.encode_utf16()),
            (Self::Binary(a), Self::Binary(b)) => a.cmp(b),
            (Self::Array(a), Self::Array(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let ord = x.idb_cmp(y);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => Ordering::Equal,
        }
    }

    /// Encodes this key into a deterministic, order-preserving canonical byte key
    /// suitable for `BTreeMap` indexing and serialization.
    #[must_use]
    pub fn to_encoded_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Number(n) => {
                out.push(0x10);
                encode_sortable_f64(*n, out);
            }
            Self::Date(ms) => {
                out.push(0x20);
                encode_sortable_f64(*ms, out);
            }
            Self::String(s) => {
                out.push(0x30);
                for cu in s.encode_utf16() {
                    let hi = (cu >> 8) as u8;
                    let lo = (cu & 0xFF) as u8;
                    if hi == 0 {
                        out.push(0x01);
                        out.push(0x01);
                    } else if hi == 1 {
                        out.push(0x01);
                        out.push(0x02);
                    } else {
                        out.push(hi);
                    }
                    if lo == 0 {
                        out.push(0x01);
                        out.push(0x01);
                    } else if lo == 1 {
                        out.push(0x01);
                        out.push(0x02);
                    } else {
                        out.push(lo);
                    }
                }
                out.push(0x00);
            }
            Self::Binary(bytes) => {
                out.push(0x40);
                for &b in bytes {
                    if b == 0 {
                        out.push(0x01);
                        out.push(0x01);
                    } else if b == 1 {
                        out.push(0x01);
                        out.push(0x02);
                    } else {
                        out.push(b);
                    }
                }
                out.push(0x00);
            }
            Self::Array(items) => {
                out.push(0x50);
                for item in items {
                    item.encode_into(out);
                }
                out.push(0x00);
            }
        }
    }
}

impl Ord for IdbKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.idb_cmp(other)
    }
}

impl PartialOrd for IdbKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn encode_sortable_f64(v: f64, out: &mut Vec<u8>) {
    let normalized = if v == 0.0 { 0.0f64 } else { v };
    let bits = normalized.to_bits();
    let sortable = if (bits & (1u64 << 63)) != 0 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    };
    out.extend_from_slice(&sortable.to_be_bytes());
}

/// Secondary index schema stored on an IndexedDB object store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdbIndexSchema {
    pub name: String,
    pub key_path: Vec<String>,
    #[serde(default)]
    pub key_path_is_array: bool,
    pub unique: bool,
    pub multi_entry: bool,
}

/// Persisted IndexedDB record pairing a structured W3C key with its payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdbRecord {
    pub key: IdbKey,
    /// Legacy JSON string payload (empty when binary `value_bytes` is populated).
    #[serde(default)]
    pub value_json: String,
    /// Compact binary structured-clone payload (exact IEEE-754 floats, zero JSON byte blowup).
    #[serde(default)]
    pub value_bytes: Vec<u8>,
}

/// Persisted schema and ordered records for one IndexedDB object store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdbStoreSnapshot {
    pub name: String,
    pub key_path: Option<Vec<String>>,
    #[serde(default)]
    pub key_path_is_array: bool,
    pub auto_increment: bool,
    pub next_key: u64,
    pub indexes: BTreeMap<String, IdbIndexSchema>,
    /// Records keyed by canonical order-preserving [`IdbKey::to_encoded_bytes`].
    #[serde(with = "records_serde")]
    pub records: BTreeMap<Vec<u8>, IdbRecord>,
}

mod records_serde {
    use super::{BTreeMap, IdbRecord};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(
        map: &BTreeMap<Vec<u8>, IdbRecord>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let items: Vec<&IdbRecord> = map.values().collect();
        items.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<Vec<u8>, IdbRecord>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let items = Vec::<IdbRecord>::deserialize(deserializer)?;
        let mut map = BTreeMap::new();
        for rec in items {
            map.insert(rec.key.to_encoded_bytes(), rec);
        }
        Ok(map)
    }
}

impl IdbStoreSnapshot {
    #[must_use]
    pub fn new(name: String, key_path: Option<Vec<String>>, auto_increment: bool) -> Self {
        let key_path_is_array = key_path.as_ref().is_some_and(|kp| kp.len() > 1);
        Self {
            name,
            key_path,
            key_path_is_array,
            auto_increment,
            next_key: 1,
            indexes: BTreeMap::new(),
            records: BTreeMap::new(),
        }
    }
}

/// Full atomic snapshot of a single per-origin IndexedDB database persisted in `redb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdbDatabaseSnapshot {
    pub name: String,
    pub version: u64,
    pub stores: BTreeMap<String, IdbStoreSnapshot>,
}

impl IdbDatabaseSnapshot {
    #[must_use]
    pub fn new(name: String, version: u64) -> Self {
        Self {
            name,
            version,
            stores: BTreeMap::new(),
        }
    }
}

const IDB_BIN_MAGIC: &[u8; 8] = b"RIXIDB01";

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_bytes(out, s.as_bytes());
}

fn write_key(out: &mut Vec<u8>, key: &IdbKey) {
    match key {
        IdbKey::Number(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        IdbKey::Date(ms) => {
            out.push(2);
            out.extend_from_slice(&ms.to_le_bytes());
        }
        IdbKey::String(s) => {
            out.push(3);
            write_str(out, s);
        }
        IdbKey::Binary(b) => {
            out.push(4);
            write_bytes(out, b);
        }
        IdbKey::Array(items) => {
            out.push(5);
            write_u32(out, items.len() as u32);
            for item in items {
                write_key(out, item);
            }
        }
    }
}

fn encode_idb_snapshot(snap: &IdbDatabaseSnapshot) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(IDB_BIN_MAGIC);
    write_u64(&mut out, snap.version);
    write_str(&mut out, &snap.name);
    write_u32(&mut out, snap.stores.len() as u32);
    for store in snap.stores.values() {
        write_str(&mut out, &store.name);
        match &store.key_path {
            None => out.push(0),
            Some(parts) => {
                out.push(1);
                write_u32(&mut out, parts.len() as u32);
                for p in parts {
                    write_str(&mut out, p);
                }
            }
        }
        out.push(u8::from(store.key_path_is_array));
        out.push(u8::from(store.auto_increment));
        write_u64(&mut out, store.next_key);
        write_u32(&mut out, store.indexes.len() as u32);
        for idx in store.indexes.values() {
            write_str(&mut out, &idx.name);
            write_u32(&mut out, idx.key_path.len() as u32);
            for p in &idx.key_path {
                write_str(&mut out, p);
            }
            out.push(u8::from(idx.key_path_is_array));
            out.push(u8::from(idx.unique));
            out.push(u8::from(idx.multi_entry));
        }
        write_u32(&mut out, store.records.len() as u32);
        for rec in store.records.values() {
            write_key(&mut out, &rec.key);
            write_str(&mut out, &rec.value_json);
            write_bytes(&mut out, &rec.value_bytes);
        }
    }
    out
}

struct BinReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BinReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let slice = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(slice.try_into().ok()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let slice = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(slice.try_into().ok()?))
    }

    fn read_f64(&mut self) -> Option<f64> {
        let slice = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(f64::from_le_bytes(slice.try_into().ok()?))
    }

    fn read_bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.read_u32()? as usize;
        let slice = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(slice.to_vec())
    }

    fn read_str(&mut self) -> Option<String> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes).ok()
    }

    fn read_key(&mut self, depth: u16) -> Option<IdbKey> {
        if depth > 256 {
            return None;
        }
        match self.read_u8()? {
            1 => Some(IdbKey::Number(self.read_f64()?)),
            2 => Some(IdbKey::Date(self.read_f64()?)),
            3 => Some(IdbKey::String(self.read_str()?)),
            4 => Some(IdbKey::Binary(self.read_bytes()?)),
            5 => {
                let len = self.read_u32()? as usize;
                let mut items = Vec::with_capacity(len.min(4096));
                for _ in 0..len {
                    items.push(self.read_key(depth + 1)?);
                }
                Some(IdbKey::Array(items))
            }
            _ => None,
        }
    }
}

fn decode_idb_snapshot(raw: &[u8]) -> Result<IdbDatabaseSnapshot> {
    if raw.len() >= 8 && &raw[..8] == IDB_BIN_MAGIC {
        let mut r = BinReader::new(&raw[8..]);
        let mut decode_inner = || -> Option<IdbDatabaseSnapshot> {
            let version = r.read_u64()?;
            let name = r.read_str()?;
            let store_count = r.read_u32()? as usize;
            let mut stores = BTreeMap::new();
            for _ in 0..store_count {
                let sname = r.read_str()?;
                let key_path = match r.read_u8()? {
                    0 => None,
                    1 => {
                        let plen = r.read_u32()? as usize;
                        let mut parts = Vec::with_capacity(plen.min(64));
                        for _ in 0..plen {
                            parts.push(r.read_str()?);
                        }
                        Some(parts)
                    }
                    _ => return None,
                };
                let key_path_is_array = r.read_u8()? != 0;
                let auto_increment = r.read_u8()? != 0;
                let next_key = r.read_u64()?;
                let idx_count = r.read_u32()? as usize;
                let mut indexes = BTreeMap::new();
                for _ in 0..idx_count {
                    let iname = r.read_str()?;
                    let klen = r.read_u32()? as usize;
                    let mut kp = Vec::with_capacity(klen.min(64));
                    for _ in 0..klen {
                        kp.push(r.read_str()?);
                    }
                    let idx_kp_is_arr = r.read_u8()? != 0;
                    let unique = r.read_u8()? != 0;
                    let multi_entry = r.read_u8()? != 0;
                    indexes.insert(
                        iname.clone(),
                        IdbIndexSchema {
                            name: iname,
                            key_path: kp,
                            key_path_is_array: idx_kp_is_arr,
                            unique,
                            multi_entry,
                        },
                    );
                }
                let rec_count = r.read_u32()? as usize;
                let mut records = BTreeMap::new();
                for _ in 0..rec_count {
                    let key = r.read_key(0)?;
                    let value_json = r.read_str()?;
                    let value_bytes = r.read_bytes()?;
                    records.insert(
                        key.to_encoded_bytes(),
                        IdbRecord {
                            key,
                            value_json,
                            value_bytes,
                        },
                    );
                }
                stores.insert(
                    sname.clone(),
                    IdbStoreSnapshot {
                        name: sname,
                        key_path,
                        key_path_is_array,
                        auto_increment,
                        next_key,
                        indexes,
                        records,
                    },
                );
            }
            Some(IdbDatabaseSnapshot {
                name,
                version,
                stores,
            })
        };
        if let Some(snap) = decode_inner() {
            return Ok(snap);
        }
    }
    let snap: IdbDatabaseSnapshot = serde_json::from_slice(raw)?;
    Ok(snap)
}

fn decode_idb_version_fast(raw: &[u8]) -> Option<u64> {
    if raw.len() >= 16 && &raw[..8] == IDB_BIN_MAGIC {
        return Some(u64::from_le_bytes(raw[8..16].try_into().ok()?));
    }
    serde_json::from_slice::<IdbDatabaseSnapshot>(raw)
        .ok()
        .map(|s| s.version)
}

fn idb_storage_key(origin: &str, db_name: &str) -> String {
    format!("{origin}\0{db_name}")
}

fn idb_origin_prefix(origin: &str) -> String {
    format!("{origin}\0")
}

// ---------------------------------------------------------------------------
// StorageEngine persistence methods (ACID `redb` transactions)
// ---------------------------------------------------------------------------

impl StorageEngine {
    // --- CacheStorage ---

    /// Ensures a named cache exists for `origin` in `redb`.
    pub fn cache_open(&self, origin: &str, cache_name: &str) -> Result<()> {
        let key = cache_storage_key(origin, cache_name);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(TABLE_CACHE_STORAGE)?;
            if table.get(key.as_str())?.is_none() {
                let bytes = serde_json::to_vec(&NamedCacheBucket::default())?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Returns whether `cache_name` exists for `origin`.
    pub fn cache_has(&self, origin: &str, cache_name: &str) -> Result<bool> {
        let key = cache_storage_key(origin, cache_name);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_CACHE_STORAGE)?;
        Ok(table.get(key.as_str())?.is_some())
    }

    /// Deletes an entire named cache for `origin`, returning `true` if it existed.
    pub fn cache_delete_bucket(&self, origin: &str, cache_name: &str) -> Result<bool> {
        let key = cache_storage_key(origin, cache_name);
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(TABLE_CACHE_STORAGE)?;
            let removed = table.remove(key.as_str())?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Lists all named caches for `origin`.
    pub fn cache_list_names(&self, origin: &str) -> Result<Vec<String>> {
        let prefix = cache_origin_prefix(origin);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_CACHE_STORAGE)?;
        let mut names = Vec::new();
        for entry in table.iter()? {
            let (k, _) = entry?;
            if let Some(name) = k.value().strip_prefix(&prefix) {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    /// Stores `response` under `request_url` in `(origin, cache_name)` atomically.
    pub fn cache_put(
        &self,
        origin: &str,
        cache_name: &str,
        request_url: &str,
        response: CachedHttpResponse,
    ) -> Result<()> {
        let key = cache_storage_key(origin, cache_name);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(TABLE_CACHE_STORAGE)?;
            let mut bucket: NamedCacheBucket = match table.get(key.as_str())? {
                Some(v) => serde_json::from_slice(v.value()).unwrap_or_default(),
                None => NamedCacheBucket::default(),
            };
            bucket.entries.insert(request_url.to_string(), response);
            let bytes = serde_json::to_vec(&bucket)?;
            table.insert(key.as_str(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Matches `request_url` inside `(origin, cache_name)`.
    pub fn cache_match(
        &self,
        origin: &str,
        cache_name: &str,
        request_url: &str,
    ) -> Result<Option<CachedHttpResponse>> {
        let key = cache_storage_key(origin, cache_name);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_CACHE_STORAGE)?;
        let Some(raw) = table.get(key.as_str())? else {
            return Ok(None);
        };
        let bucket: NamedCacheBucket = serde_json::from_slice(raw.value()).unwrap_or_default();
        Ok(bucket.entries.get(request_url).cloned())
    }

    /// Matches `request_url` across all named caches for `origin` (for `caches.match(...)`).
    pub fn cache_match_any(
        &self,
        origin: &str,
        request_url: &str,
    ) -> Result<Option<CachedHttpResponse>> {
        for name in self.cache_list_names(origin)? {
            if let Some(hit) = self.cache_match(origin, &name, request_url)? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    /// Removes `request_url` from `(origin, cache_name)`, returning `true` if present.
    pub fn cache_delete_entry(
        &self,
        origin: &str,
        cache_name: &str,
        request_url: &str,
    ) -> Result<bool> {
        let key = cache_storage_key(origin, cache_name);
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(TABLE_CACHE_STORAGE)?;
            let mut bucket: NamedCacheBucket = match table.get(key.as_str())? {
                Some(v) => serde_json::from_slice(v.value()).unwrap_or_default(),
                None => return Ok(false),
            };
            let did_remove = bucket.entries.remove(request_url).is_some();
            if did_remove {
                let bytes = serde_json::to_vec(&bucket)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            did_remove
        };
        write_txn.commit()?;
        Ok(removed)
    }

    /// Returns all cached request URLs inside `(origin, cache_name)`.
    pub fn cache_keys(&self, origin: &str, cache_name: &str) -> Result<Vec<String>> {
        let key = cache_storage_key(origin, cache_name);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_CACHE_STORAGE)?;
        let Some(raw) = table.get(key.as_str())? else {
            return Ok(Vec::new());
        };
        let bucket: NamedCacheBucket = serde_json::from_slice(raw.value()).unwrap_or_default();
        Ok(bucket.entries.keys().cloned().collect())
    }

    // --- IndexedDB ---

    /// Loads an existing per-origin IndexedDB database snapshot from `redb`.
    pub fn idb_load_database(
        &self,
        origin: &str,
        db_name: &str,
    ) -> Result<Option<IdbDatabaseSnapshot>> {
        let key = idb_storage_key(origin, db_name);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_INDEXED_DB)?;
        let Some(raw) = table.get(key.as_str())? else {
            return Ok(None);
        };
        let snap = decode_idb_snapshot(raw.value())?;
        Ok(Some(snap))
    }

    /// Atomically persists `snapshot` into `redb` via a durable `WriteTransaction`.
    pub fn idb_commit_database(&self, origin: &str, snapshot: &IdbDatabaseSnapshot) -> Result<()> {
        let key = idb_storage_key(origin, &snapshot.name);
        let bytes = encode_idb_snapshot(snapshot);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(TABLE_INDEXED_DB)?;
            table.insert(key.as_str(), bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Deletes a per-origin IndexedDB database from `redb`.
    pub fn idb_delete_database(&self, origin: &str, db_name: &str) -> Result<bool> {
        let key = idb_storage_key(origin, db_name);
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(TABLE_INDEXED_DB)?;
            let removed = table.remove(key.as_str())?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(existed)
    }

    /// Lists all `(name, version)` IndexedDB databases for `origin`.
    pub fn idb_list_databases(&self, origin: &str) -> Result<Vec<(String, u64)>> {
        let prefix = idb_origin_prefix(origin);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_INDEXED_DB)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            if let Some(name) = k.value().strip_prefix(&prefix) {
                if let Some(version) = decode_idb_version_fast(v.value()) {
                    out.push((name.to_string(), version));
                }
            }
        }
        Ok(out)
    }
}
