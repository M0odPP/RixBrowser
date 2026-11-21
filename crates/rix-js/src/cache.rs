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
}
