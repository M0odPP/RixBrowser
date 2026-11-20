use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Database error: {0}")]
    Database(#[from] Box<redb::DatabaseError>),

    #[error("Database transaction error: {0}")]
    Transaction(#[from] Box<redb::TransactionError>),

    #[error("Database table error: {0}")]
    Table(#[from] Box<redb::TableError>),

    #[error("Database commit error: {0}")]
    Commit(#[from] Box<redb::CommitError>),

    #[error("Database storage error: {0}")]
    Storage(#[from] Box<redb::StorageError>),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protected cookie storage is unavailable or damaged")]
    CookieProtection,

    #[error("Entity not found: {0}")]
    NotFound(String),
}

impl From<redb::DatabaseError> for StorageError {
    fn from(err: redb::DatabaseError) -> Self {
        Self::Database(Box::new(err))
    }
}

impl From<redb::TransactionError> for StorageError {
    fn from(err: redb::TransactionError) -> Self {
        Self::Transaction(Box::new(err))
    }
}

impl From<redb::TableError> for StorageError {
    fn from(err: redb::TableError) -> Self {
        Self::Table(Box::new(err))
    }
}

impl From<redb::CommitError> for StorageError {
    fn from(err: redb::CommitError) -> Self {
        Self::Commit(Box::new(err))
    }
}

impl From<redb::StorageError> for StorageError {
    fn from(err: redb::StorageError) -> Self {
        Self::Storage(Box::new(err))
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
