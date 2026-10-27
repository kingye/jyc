//! End-to-end CLI test: `jyc send` delivers arguments in order and stops at
//! the first rejected message.

use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut needed = None;
    loop {
        if let Some(end) = header_end(&buf) {
            if needed.is_none() {
                let headers = String::from_utf8_lossy(&buf[..end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                needed = Some(end + 4 + content_length);
            }
            if let Some(n) = needed
                && buf.len() >= n
            {
                buf.truncate(n);
                return buf;
            }
        }
        let mut chunk = [0_u8; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn request_message(request: &[u8]) -> String {
    let end = header_end(request).expect("request headers");
    let body = &request[end + 4..];
    let value: serde_json::Value = serde_json::from_slice(body).expect("JSON body");
    value["message"]
        .as_str()
        .expect("message field")
        .to_string()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_cli_sends_in_order_and_stops_at_the_first_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(4);

    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            tx.send(request_message(&request)).await.unwrap();
            if index == 0 {
                respond(&mut stream, "200 OK", r#"{"message":"ok"}"#).await;
            } else {
                respond(
                    &mut stream,
                    "422 Unprocessable Entity",
                    r#"{"error":"Message cannot be empty"}"#,
                )
                .await;
            }
        }

        // A correct fail-fast loop sends no third request. If it does, answer
        // it so the command cannot hang, and report it through the channel.
        match tokio::time::timeout(Duration::from_millis(500), listener.accept()).await {
            Ok(Ok((mut stream, _))) => {
                let request = read_request(&mut stream).await;
                tx.send(format!("unexpected:{}", request_message(&request)))
                    .await
                    .unwrap();
                respond(&mut stream, "200 OK", r#"{"message":"ok"}"#).await;
            }
            _ => tx.send("no-third".to_string()).await.unwrap(),
        }
    });

    let data_dir = TempDir::new().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_jyc"))
        .args([
            "--data-dir",
            data_dir.path().to_str().unwrap(),
            "--log-file",
            "/dev/null",
            "send",
            "--addr",
            &addr.to_string(),
            "jyc",
            "first",
            "   ",
            "third",
        ])
        .output()
        .await
        .unwrap();

    server.await.unwrap();
    assert!(
        !output.status.success(),
        "a rejected message must fail the command: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to send message 2 of 3"),
        "unexpected stderr: {stderr}"
    );

    assert_eq!(rx.recv().await.as_deref(), Some("first"));
    assert_eq!(rx.recv().await.as_deref(), Some("   "));
    assert_eq!(rx.recv().await.as_deref(), Some("no-third"));
}
