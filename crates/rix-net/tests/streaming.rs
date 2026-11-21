use rix_core::url::RixUrl;
use rix_net::HttpClient;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[tokio::test]
async fn chunked_html_streams_before_response_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let send_second_chunk = Arc::new(Notify::new());
    let server_gate = Arc::clone(&send_second_chunk);

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let mut req_buf = [0u8; 2048];
        let _ = stream.read(&mut req_buf).await.expect("read request");

        let chunk1 = b"<html><head><link rel=\"stylesheet\" href=\"a.css\"></head>";
        let chunk2 = b"<body>hello world</body></html>";

        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
            chunk1.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(chunk1).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();

        // Wait until the client explicitly receives chunk 1 before sending chunk 2.
        server_gate.notified().await;

        let tail = format!("{:X}\r\n", chunk2.len());
        stream.write_all(tail.as_bytes()).await.unwrap();
        stream.write_all(chunk2).await.unwrap();
        stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
        stream.flush().await.unwrap();
    });

    let client = HttpClient::new(None).expect("client");
    let url = RixUrl::parse(&format!("http://{addr}/index.html")).expect("url");
    let mut resp = client.fetch_stream(&url).await.expect("stream headers");

    // Chunk 1 must arrive immediately without waiting for chunk 2 or EOF.
    let first = resp
        .body
        .next_chunk()
        .await
        .expect("first chunk present")
        .expect("first chunk ok");
    assert_eq!(
        &first[..],
        b"<html><head><link rel=\"stylesheet\" href=\"a.css\"></head>"
    );

    // Unblock server to transmit chunk 2 and EOF.
    send_second_chunk.notify_one();

    let mut rest = Vec::new();
    while let Some(chunk) = resp.body.next_chunk().await {
        rest.extend_from_slice(&chunk.expect("next chunk ok"));
    }
    assert_eq!(rest, b"<body>hello world</body></html>");

    server.await.expect("server join");
}
