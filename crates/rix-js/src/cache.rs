//! Persistent bytecode disk cache backed by `rix-storage`.
//!
//! Entries are keyed by `(source_sha256, BYTECODE_VERSION, source_type)` and
//! encoded via `postcard` with a magic/version envelope so incompatible or
//! corrupt payloads trigger clean transparent recompilation.

use crate::ast::SourceType;
use crate::bytecode::{CompiledProgram, BYTECODE_CACHE_MAGIC, BYTECODE_VERSION};
use crate::compiler::{compile, CompileError};
use rix_storage::StorageEngine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

const MAX_MEM_BYTECODE_ENTRIES: usize = 256;

#[derive(Default)]
struct MemBytecodeCache {
    entries: HashMap<([u8; 32], u8), (CompiledProgram, u64)>,
    tick: u64,
}

fn mem_bytecode_cache() -> &'static RwLock<MemBytecodeCache> {
    static CACHE: OnceLock<RwLock<MemBytecodeCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(MemBytecodeCache::default()))
}

fn source_type_tag(source_type: SourceType) -> u8 {
    match source_type {
        SourceType::Script => 0,
        SourceType::Module => 1,
    }
}

fn mem_cache_get(hash: [u8; 32], source_type: SourceType) -> Option<CompiledProgram> {
    let key = (hash, source_type_tag(source_type));
    let prog = mem_bytecode_cache()
        .read()
        .ok()?
        .entries
        .get(&key)
        .map(|(p, _)| p.clone())?;
    if let Ok(mut guard) = mem_bytecode_cache().write() {
        guard.tick = guard.tick.wrapping_add(1);
        let t = guard.tick;
        if let Some(entry) = guard.entries.get_mut(&key) {
            entry.1 = t;
        }
    }
    Some(prog)
}

fn mem_cache_put(hash: [u8; 32], program: &CompiledProgram) {
    let key = (hash, source_type_tag(program.source_type));
    if let Ok(mut guard) = mem_bytecode_cache().write() {
        if guard.entries.len() >= MAX_MEM_BYTECODE_ENTRIES && !guard.entries.contains_key(&key) {
            if let Some(victim) = guard
                .entries
                .iter()
                .min_by_key(|(_, (_, seq))| *seq)
                .map(|(k, _)| *k)
            {
                guard.entries.remove(&victim);
            }
        }
        guard.tick = guard.tick.wrapping_add(1);
        let t = guard.tick;
        guard.entries.insert(key, (program.clone(), t));
    }
}

/// Compiles `source` using the process-wide L1 RAM bytecode LRU cache (bypassing re-lexing and
/// re-parsing identical ES modules and scripts across warm reloads and multiple tabs).
pub fn compile_cached_in_memory(
    source: &str,
    source_type: SourceType,
) -> Result<CompiledProgram, CompileError> {
    if source.len() < 1_024 {
        return compile(source, source_type);
    }
    let hash = BytecodeCache::source_sha256(source);
    if let Some(cached) = mem_cache_get(hash, source_type) {
        return Ok(cached);
    }
    let compiled = compile(source, source_type)?;
    mem_cache_put(hash, &compiled);
    Ok(compiled)
}

/// Reports whether [`BytecodeCache::compile_or_load`] skipped compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Hit,
    Miss,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedBytecodeEnvelope {
    magic: [u8; 4],
    version: u32,
    source_sha256: [u8; 32],
    program: CompiledProgram,
}

/// Persistent `(sha256, bytecode_version)` bytecode cache backed by [`StorageEngine`].
pub struct BytecodeCache<'a> {
    storage: &'a StorageEngine,
}

impl<'a> BytecodeCache<'a> {
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self { storage }
    }

    /// Computes the deterministic SHA-256 hash of UTF-8 source bytes.
    pub fn source_sha256(source: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        hasher.finalize().into()
    }

    /// Formats the composite storage key `(sha256, bytecode_version, source_type)`.
    pub fn cache_key(source_sha256: &[u8; 32], source_type: SourceType) -> String {
        let kind = match source_type {
            SourceType::Script => "script",
            SourceType::Module => "module",
        };
        let mut hex = String::with_capacity(64);
        for b in source_sha256 {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{b:02x}");
        }
        format!("v{BYTECODE_VERSION}:{kind}:{hex}")
    }

    /// Attempts to deserialize a cached [`CompiledProgram`] without lexing or parsing.
    pub fn load(&self, source: &str, source_type: SourceType) -> Option<CompiledProgram> {
        let hash = Self::source_sha256(source);
        let key = Self::cache_key(&hash, source_type);
        let raw = self.storage.get_bytecode_cache(&key).ok().flatten()?;
        let envelope: CachedBytecodeEnvelope = postcard::from_bytes(&raw).ok()?;
        if envelope.magic != BYTECODE_CACHE_MAGIC
            || envelope.version != BYTECODE_VERSION
            || envelope.source_sha256 != hash
            || envelope.program.bytecode_version != BYTECODE_VERSION
            || envelope.program.source_type != source_type
        {
            return None;
        }
        Some(envelope.program)
    }

    /// Stores a compiled program under its `(sha256, bytecode_version, source_type)` key.
    pub fn store(
        &self,
        source: &str,
        program: &CompiledProgram,
    ) -> Result<(), rix_storage::StorageError> {
        let hash = Self::source_sha256(source);
        let key = Self::cache_key(&hash, program.source_type);
        let envelope = CachedBytecodeEnvelope {
            magic: BYTECODE_CACHE_MAGIC,
            version: BYTECODE_VERSION,
            source_sha256: hash,
            program: program.clone(),
        };
        if let Ok(bytes) = postcard::to_allocvec(&envelope) {
            self.storage.put_bytecode_cache(&key, &bytes)?;
        }
        Ok(())
    }

    /// Loads compiled bytecode directly from disk on cache hit, or parses, compiles,
    /// and persists on cache miss.
    pub fn compile_or_load(
        &self,
        source: &str,
        source_type: SourceType,
    ) -> Result<(CompiledProgram, CacheStatus), CompileError> {
        if let Some(cached) = self.load(source, source_type) {
            return Ok((cached, CacheStatus::Hit));
        }
        let compiled = compile(source, source_type)?;
        let _ = self.store(source, &compiled);
        Ok((compiled, CacheStatus::Miss))
    }

    /// L1 RAM + L2 persistent disk bytecode cache lookup used by `ContentRuntime` on warm reloads.
    pub fn compile_or_load_with_ram(
        &self,
        source: &str,
        source_type: SourceType,
    ) -> Result<(CompiledProgram, CacheStatus), CompileError> {
        let hash = Self::source_sha256(source);
        if let Some(mem_hit) = mem_cache_get(hash, source_type) {
            return Ok((mem_hit, CacheStatus::Hit));
        }
        if let Some(disk_hit) = self.load(source, source_type) {
            mem_cache_put(hash, &disk_hit);
            return Ok((disk_hit, CacheStatus::Hit));
        }
        let compiled = compile(source, source_type)?;
        mem_cache_put(hash, &compiled);
        let _ = self.store(source, &compiled);
        Ok((compiled, CacheStatus::Miss))
    }
}
