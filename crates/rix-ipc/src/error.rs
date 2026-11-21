use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("IPC payload serialization error: {0}")]
    Serialization(#[from] postcard::Error),

    #[error("IPC frame size {size} bytes exceeds maximum {max} bytes")]
    FrameTooLarge { size: usize, max: usize },

    #[error("IPC protocol version mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: u8, actual: u8 },

    #[error("IPC frame contains unsupported flags: 0x{0:08x}")]
    UnexpectedFlags(u32),

    #[error("IPC stream ended unexpectedly mid-frame")]
    UnexpectedEof,

    #[error("Invalid IPC endpoint: {0}")]
    InvalidEndpoint(String),
}

pub type Result<T> = std::result::Result<T, IpcError>;
