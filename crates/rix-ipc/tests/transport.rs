use bytes::BytesMut;
use rix_core::Size;
use rix_ipc::{
    encode_message, unix_pair, BrowserToContent, ContentToBrowser, CursorIcon, FrameHeader,
    FramedReceiver, FramedSender, IpcError, IpcListener, LoadState, PipelineId, Point, TabId,
    ViewportMetrics, FLAG_NONE, HEADER_SIZE, MAX_CONTROL_FRAME_BYTES, PROTOCOL_VERSION,
};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn clean_eof_returns_none() {
    let (client, server) = tokio::io::duplex(256);
    drop(client);
    let mut rx: FramedReceiver<_, BrowserToContent> = FramedReceiver::new(server);
    assert!(rx.recv().await.expect("clean EOF").is_none());
}

#[tokio::test]
async fn truncated_header_or_payload_returns_unexpected_eof() {
    // Truncated header
    let (mut writer, reader) = tokio::io::duplex(256);
    writer.write_all(&[1, 2, 3, 4]).await.expect("write");
    drop(writer);
    let mut rx: FramedReceiver<_, BrowserToContent> = FramedReceiver::new(reader);
    assert!(matches!(rx.recv().await, Err(IpcError::UnexpectedEof)));

    // Truncated payload
    let (mut writer, reader) = tokio::io::duplex(256);
    let hdr = FrameHeader::new(32, FLAG_NONE).expect("header").encode();
    writer.write_all(&hdr).await.expect("write hdr");
    writer.write_all(&[0xAA; 10]).await.expect("write partial");
    drop(writer);
    let mut rx: FramedReceiver<_, BrowserToContent> = FramedReceiver::new(reader);
    assert!(matches!(rx.recv().await, Err(IpcError::UnexpectedEof)));
}

#[tokio::test]
async fn chunked_duplex_preserves_200_messages_in_order() {
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    let messages: Vec<BrowserToContent> = (0..200u64)
        .map(|i| match i % 3 {
            0 => BrowserToContent::Ping { seq: i },
            1 => BrowserToContent::BeginFrame {
                pipeline: PipelineId(i % 4),
                frame_seq: i,
                vsync_ns: i * 16_666_667,
            },
            _ => BrowserToContent::ScrollOffsetSync {
                pipeline: PipelineId(1),
                node_id: rix_ipc::SpatialNodeId((i % 8) as u32),
                offset: Point::new(i as f32, (i * 2) as f32),
            },
        })
        .collect();

    let mut wire = BytesMut::new();
    for msg in &messages {
        encode_message(msg, FLAG_NONE, MAX_CONTROL_FRAME_BYTES, &mut wire).expect("encode");
    }

    let feeder = tokio::spawn(async move {
        let mut pos = 0usize;
        let mut seed = 0x9E37_79B9u64;
        while pos < wire.len() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let chunk_len = ((seed >> 32) as usize % 37) + 1;
            let end = (pos + chunk_len).min(wire.len());
            writer
                .write_all(&wire[pos..end])
                .await
                .expect("chunk write");
            pos = end;
        }
    });

    let mut rx: FramedReceiver<_, BrowserToContent> = FramedReceiver::new(reader);
    let mut received = Vec::with_capacity(messages.len());
    while let Some(msg) = rx.recv().await.expect("recv") {
        received.push(msg);
    }
    feeder.await.expect("feeder");
    assert_eq!(received, messages);
}

#[tokio::test]
async fn uds_listener_connect_and_drop_cleans_socket_file() {
    let dir = TempDir::new().expect("tempdir");
    let sock_path = dir.path().join("nested").join("control.sock");

    let listener = IpcListener::bind(&sock_path).expect("bind");
    assert!(sock_path.exists());

    let accept_task = tokio::spawn(async move {
        listener
            .accept::<ContentToBrowser, BrowserToContent>()
            .await
            .expect("accept")
    });

    let mut client = rix_ipc::connect_unix::<BrowserToContent, ContentToBrowser>(&sock_path)
        .await
        .expect("connect");
    let mut server = accept_task.await.expect("accepted");

    let init = BrowserToContent::CreatePipeline {
        pipeline: PipelineId(10),
        tab: TabId(3),
        viewport: ViewportMetrics {
            logical_size: Size::new(1280.0, 800.0),
            device_pixel_ratio: 1.0,
        },
    };
    client.send(&init).await.expect("send");
    assert_eq!(server.recv().await.expect("recv"), Some(init));

    let ready = ContentToBrowser::PipelineReady {
        pipeline: PipelineId(10),
    };
    server.send(&ready).await.expect("send");
    assert_eq!(client.recv().await.expect("recv"), Some(ready));

    drop(client);
    assert_eq!(server.recv().await.expect("eof"), None);

    // Rebinding on a stale socket path succeeds; dropping the listener removes the file.
    {
        let _rebound = IpcListener::bind(&sock_path).expect("rebind");
        assert!(sock_path.exists());
    }
    assert!(!sock_path.exists());
}

#[tokio::test]
async fn split_unix_pair_handles_1000_concurrent_bidirectional_messages() {
    let (browser_chan, content_chan) =
        unix_pair::<BrowserToContent, ContentToBrowser>().expect("pair");
    let (mut b_tx, mut b_rx) = browser_chan.into_split();
    let (mut c_tx, mut c_rx) = content_chan.into_split();

    const COUNT: u64 = 1000;

    let browser_send = tokio::spawn(async move {
        for i in 0..COUNT {
            b_tx.send(&BrowserToContent::Ping { seq: i })
                .await
                .expect("b send");
        }
        b_tx.shutdown().await.expect("b shutdown");
    });

    let content_send = tokio::spawn(async move {
        for i in 0..COUNT {
            let msg = if i.is_multiple_of(2) {
                ContentToBrowser::Pong { seq: i }
            } else {
                ContentToBrowser::CursorChanged {
                    pipeline: PipelineId(1),
                    cursor: CursorIcon::Pointer,
                }
            };
            c_tx.send(&msg).await.expect("c send");
        }
        c_tx.shutdown().await.expect("c shutdown");
    });

    let browser_recv = tokio::spawn(async move {
        let mut count = 0u64;
        while let Some(msg) = b_rx.recv().await.expect("b recv") {
            if count.is_multiple_of(2) {
                assert_eq!(msg, ContentToBrowser::Pong { seq: count });
            } else {
                assert_eq!(
                    msg,
                    ContentToBrowser::CursorChanged {
                        pipeline: PipelineId(1),
                        cursor: CursorIcon::Pointer
                    }
                );
            }
            count += 1;
        }
        assert_eq!(count, COUNT);
    });

    let content_recv = tokio::spawn(async move {
        let mut count = 0u64;
        while let Some(msg) = c_rx.recv().await.expect("c recv") {
            assert_eq!(msg, BrowserToContent::Ping { seq: count });
            count += 1;
        }
        assert_eq!(count, COUNT);
    });

    let (r1, r2, r3, r4) = tokio::join!(browser_send, content_send, browser_recv, content_recv);
    r1.expect("b_send");
    r2.expect("c_send");
    r3.expect("b_recv");
    r4.expect("c_recv");
}

#[tokio::test]
async fn listener_refuses_non_socket_file() {
    let dir = TempDir::new().expect("tempdir");
    let file_path = dir.path().join("regular.txt");
    std::fs::write(&file_path, b"do not delete").expect("write");

    let err = IpcListener::bind(&file_path).unwrap_err();
    assert!(matches!(err, IpcError::InvalidEndpoint(_)));
    assert_eq!(
        std::fs::read(&file_path).expect("still intact"),
        b"do not delete"
    );
}

#[tokio::test]
async fn framed_sender_rejects_oversized_payload() {
    let (writer, _reader) = tokio::io::duplex(256);
    let mut tx: FramedSender<_, ContentToBrowser> = FramedSender::with_max_frame_bytes(writer, 16);
    let msg = ContentToBrowser::LoadStateChanged {
        pipeline: PipelineId(1),
        state: LoadState::Failed {
            message: "a very long error payload that exceeds 16 bytes".to_string(),
        },
    };
    let err = tx.send(&msg).await.unwrap_err();
    assert!(matches!(err, IpcError::FrameTooLarge { max: 16, .. }));
    assert_eq!(HEADER_SIZE, 8);
    assert_eq!(PROTOCOL_VERSION, 1);
}
