#![deny(unsafe_code)]

mod cookie_crypto;
pub mod db;
pub mod error;
pub mod idb_cache;
pub mod models;
pub mod secrets;

pub use db::StorageEngine;
pub use error::{Result, StorageError};
pub use idb_cache::{
    CachedHttpResponse, IdbDatabaseSnapshot, IdbIndexSchema, IdbKey, IdbRecord, IdbStoreSnapshot,
};
pub use models::{Bookmark, Cookie, HistoryEntry, SameSitePolicy, SessionState, TabSession};
pub use secrets::SecretStoreError;
