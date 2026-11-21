use crate::error::{IpcError, Result};
use crate::frame::{
    decode_payload, encode_message, FrameDecoder, FLAG_NONE, MAX_CONTROL_FRAME_BYTES,
};
use bytes::BytesMut;
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{
    unix::{OwnedReadHalf, OwnedWriteHalf},
    UnixListener, UnixStream,
};

const INITIAL_SEND_CAPACITY: usize = 8192;
const COMPACT_SEND_THRESHOLD: usize = 256 * 1024;

async fn write_frame<W, Tx>(
    writer: &mut W,
    send_buf: &mut BytesMut,
    max_frame_bytes: usize,
    msg: &Tx,
) -> Result<usize>
where
    W: AsyncWrite + Unpin,
    Tx: Serialize,
{
    send_buf.clear();
    let written = encode_message(msg, FLAG_NONE, max_frame_bytes, send_buf)?;
    writer.write_all(send_buf).await?;
    writer.flush().await?;
    if send_buf.capacity() > COMPACT_SEND_THRESHOLD {
        *send_buf = BytesMut::with_capacity(INITIAL_SEND_CAPACITY);
    }
    Ok(written)
}

async fn read_frame<R, Rx>(reader: &mut R, decoder: &mut FrameDecoder) -> Result<Option<Rx>>
where
    R: AsyncRead + Unpin,
    Rx: DeserializeOwned,
{
    loop {
        if let Some((_header, payload)) = decoder.try_decode()? {
            let msg = decode_payload(&payload)?;
            return Ok(Some(msg));
        }
        let bytes_read = reader.read_buf(decoder.buf_mut()).await?;
        if bytes_read == 0 {
            return if decoder.has_partial_frame() {
                Err(IpcError::UnexpectedEof)
            } else {
                Ok(None)
            };
        }
    }
}

#[derive(Debug)]
pub struct FramedSender<W, Tx> {
    writer: W,
    send_buf: BytesMut,
    max_frame_bytes: usize,
    _marker: PhantomData<fn(&Tx)>,
}

impl<W, Tx> FramedSender<W, Tx> {
    pub fn new(writer: W) -> Self {
        Self::with_max_frame_bytes(writer, MAX_CONTROL_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(writer: W, max_frame_bytes: usize) -> Self {
        Self {
            writer,
            send_buf: BytesMut::with_capacity(INITIAL_SEND_CAPACITY),
            max_frame_bytes,
            _marker: PhantomData,
        }
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W, Tx> FramedSender<W, Tx>
where
    W: AsyncWrite + Unpin,
    Tx: Serialize,
{
    pub async fn send(&mut self, msg: &Tx) -> Result<usize> {
        write_frame(
            &mut self.writer,
            &mut self.send_buf,
            self.max_frame_bytes,
            msg,
        )
        .await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.writer.shutdown().await?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct FramedReceiver<R, Rx> {
    reader: R,
    decoder: FrameDecoder,
    _marker: PhantomData<fn() -> Rx>,
}

impl<R, Rx> FramedReceiver<R, Rx> {
    pub fn new(reader: R) -> Self {
        Self::with_max_frame_bytes(reader, MAX_CONTROL_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(reader: R, max_frame_bytes: usize) -> Self {
        Self {
            reader,
            decoder: FrameDecoder::with_max_frame_bytes(max_frame_bytes),
            _marker: PhantomData,
        }
    }

    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R, Rx> FramedReceiver<R, Rx>
where
    R: AsyncRead + Unpin,
    Rx: DeserializeOwned,
{
    pub async fn recv(&mut self) -> Result<Option<Rx>> {
        read_frame(&mut self.reader, &mut self.decoder).await
    }
}

#[derive(Debug)]
pub struct FramedChannel<S, Tx, Rx> {
    stream: S,
    send_buf: BytesMut,
    decoder: FrameDecoder,
    max_frame_bytes: usize,
    _marker: PhantomData<fn(&Tx) -> Rx>,
}

impl<S, Tx, Rx> FramedChannel<S, Tx, Rx> {
    pub fn new(stream: S) -> Self {
        Self::with_max_frame_bytes(stream, MAX_CONTROL_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(stream: S, max_frame_bytes: usize) -> Self {
        Self {
            stream,
            send_buf: BytesMut::with_capacity(INITIAL_SEND_CAPACITY),
            decoder: FrameDecoder::with_max_frame_bytes(max_frame_bytes),
            max_frame_bytes,
            _marker: PhantomData,
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S, Tx, Rx> FramedChannel<S, Tx, Rx>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Tx: Serialize,
    Rx: DeserializeOwned,
{
    pub async fn send(&mut self, msg: &Tx) -> Result<usize> {
        write_frame(
            &mut self.stream,
            &mut self.send_buf,
            self.max_frame_bytes,
            msg,
        )
        .await
    }

    pub async fn recv(&mut self) -> Result<Option<Rx>> {
        read_frame(&mut self.stream, &mut self.decoder).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }
}

#[cfg(unix)]
pub type UnixIpcChannel<Tx, Rx> = FramedChannel<UnixStream, Tx, Rx>;
#[cfg(unix)]
pub type UnixIpcSender<Tx> = FramedSender<OwnedWriteHalf, Tx>;
#[cfg(unix)]
pub type UnixIpcReceiver<Rx> = FramedReceiver<OwnedReadHalf, Rx>;

#[cfg(unix)]
impl<Tx, Rx> FramedChannel<UnixStream, Tx, Rx> {
    pub fn into_split(self) -> (UnixIpcSender<Tx>, UnixIpcReceiver<Rx>) {
        let (read_half, write_half) = self.stream.into_split();
        let sender = FramedSender {
            writer: write_half,
            send_buf: self.send_buf,
            max_frame_bytes: self.max_frame_bytes,
            _marker: PhantomData,
        };
        let receiver = FramedReceiver {
            reader: read_half,
            decoder: self.decoder,
            _marker: PhantomData,
        };
        (sender, receiver)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct IpcListener {
    listener: UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl IpcListener {
    pub fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() {
            return Err(IpcError::InvalidEndpoint(
                "Unix socket path must not be empty".to_string(),
            ));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_socket() {
                    std::fs::remove_file(&path)?;
                } else {
                    return Err(IpcError::InvalidEndpoint(format!(
                        "Refusing to overwrite non-socket path: {}",
                        path.display()
                    )));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let listener = UnixListener::bind(&path)?;
        Ok(Self { listener, path })
    }

    pub fn local_path(&self) -> &Path {
        &self.path
    }

    pub async fn accept<Tx, Rx>(&self) -> Result<UnixIpcChannel<Tx, Rx>> {
        let (stream, _) = self.listener.accept().await?;
        Ok(FramedChannel::new(stream))
    }
}

#[cfg(unix)]
impl Drop for IpcListener {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    path = %self.path.display(),
                    %err,
                    "failed to remove Unix IPC socket on drop"
                );
            }
        }
    }
}

#[cfg(unix)]
pub async fn connect_unix<Tx, Rx>(path: impl AsRef<Path>) -> Result<UnixIpcChannel<Tx, Rx>> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(IpcError::InvalidEndpoint(
            "Unix socket path must not be empty".to_string(),
        ));
    }
    let stream = UnixStream::connect(path).await?;
    Ok(FramedChannel::new(stream))
}

#[cfg(unix)]
pub fn unix_pair<TxA, RxA>() -> Result<(UnixIpcChannel<TxA, RxA>, UnixIpcChannel<RxA, TxA>)> {
    let (a, b) = UnixStream::pair()?;
    Ok((FramedChannel::new(a), FramedChannel::new(b)))
}
