//! Minimal SSE (Server-Sent Events) client.
//!
//! Replaces `reqwest-eventsource`, which pins reqwest 0.12 and blocks the
//! workspace-wide reqwest 0.13 upgrade. Only `data:` payloads are surfaced;
//! `event:`/`id:`/comments are ignored.
//!
//! Error-string contract (relied on by the retry classifier and
//! `extract_retry_after` in `agent_loop`):
//! - non-2xx responses produce `Invalid status code: <code> [retry-after: <Ns>] body: <body>`
//! - clean stream end produces `Stream ended`

use anyhow::Result;
use futures::{Stream, StreamExt};
use std::pin::Pin;

/// A single SSE event, mirroring `reqwest_eventsource::Event`.
#[derive(Debug)]
pub enum Event {
    /// The stream opened (HTTP 2xx, headers received).
    Open,
    /// A complete `data:` block.
    Message(Message),
}

/// An SSE message — only `data` is populated.
#[derive(Debug)]
pub struct Message {
    /// The `data:` payload (multiple `data:` lines joined with `\n`).
    pub data: String,
}

enum SseState {
    /// Request built but not yet sent.
    Send(reqwest::RequestBuilder),
    /// Response accepted; SSE body being parsed.
    Read {
        stream: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
        buf: Vec<u8>,
    },
    Done,
}

/// Stream SSE events from a prepared request.
pub fn stream_sse(
    request: reqwest::RequestBuilder,
) -> impl Stream<Item = Result<Event, anyhow::Error>> + Send {
    futures::stream::unfold(SseState::Send(request), |state| async move {
        match state {
            SseState::Send(request) => {
                let response = match request.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return Some((
                            Err(anyhow::anyhow!("SSE connection failed: {e}")),
                            SseState::Done,
                        ));
                    }
                };
                let status = response.status();
                if !status.is_success() {
                    return Some((Err(non_2xx_error(response).await), SseState::Done));
                }
                let stream = Box::pin(response.bytes_stream().map(|r| r.map(|b| b.to_vec())));
                Some((
                    Ok(Event::Open),
                    SseState::Read {
                        stream,
                        buf: Vec::new(),
                    },
                ))
            }
            SseState::Read {
                mut stream,
                mut buf,
            } => loop {
                // A complete event ends at a blank line.
                if let Some(data) = take_complete_event(&mut buf) {
                    return Some((
                        Ok(Event::Message(Message { data })),
                        SseState::Read { stream, buf },
                    ));
                }
                match stream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    Some(Err(e)) => {
                        return Some((
                            Err(anyhow::anyhow!("SSE stream error: {e}")),
                            SseState::Done,
                        ));
                    }
                    None => {
                        // EOF: surface any trailing `data:` as a final event,
                        // then signal the clean end (callers match "Stream ended").
                        if !buf.is_empty() {
                            let raw = std::mem::take(&mut buf);
                            if let Some(data) = parse_data(&String::from_utf8_lossy(&raw)) {
                                return Some((
                                    Ok(Event::Message(Message { data })),
                                    SseState::Read { stream, buf },
                                ));
                            }
                        }
                        return Some((Err(anyhow::anyhow!("Stream ended")), SseState::Done));
                    }
                }
            },
            SseState::Done => None,
        }
    })
}

/// Extract one complete event (terminated by a blank line) from `buf`,
/// returning its `data:` payload. Incomplete trailing bytes stay in `buf`.
fn take_complete_event(buf: &mut Vec<u8>) -> Option<String> {
    let end = find_event_end(buf)?;
    let raw: Vec<u8> = buf.drain(..end).collect();
    parse_data(&String::from_utf8_lossy(&raw))
}

/// Index just past the first blank line (`\n\n` or `\r\n\r\n`).
fn find_event_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some(i + 2);
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some(i + 3);
            }
        }
    }
    None
}

/// Join the `data:` lines of an event block (SSE spec: multiple `data:`
/// lines concatenate with `\n`). `None` when the block has no `data:` lines.
fn parse_data(event: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in event.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

/// Build the error for a non-2xx response, embedding status, `Retry-After`
/// and the body (bounded) so callers get the provider's actual error message
/// without a separate diagnostic re-POST.
async fn non_2xx_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let body = match tokio::time::timeout(std::time::Duration::from_secs(15), response.text()).await
    {
        Ok(Ok(text)) => truncate_body(&text),
        _ => String::from("<unreadable body>"),
    };
    let msg = match retry_after {
        Some(secs) => format!("Invalid status code: {status} retry-after: {secs}s body: {body}"),
        None => format!("Invalid status code: {status} body: {body}"),
    };
    anyhow::anyhow!(msg)
}

/// Bound error-body size — we only need the leading error message.
fn truncate_body(text: &str) -> String {
    if text.len() <= 2000 {
        return text.to_string();
    }
    let head: String = text.chars().take(2000).collect();
    format!("{head}…(truncated, {} bytes total)", text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_sse_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sse"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "data: {\"a\":1}\n\ndata: hello\n\ndata: line1\ndata: line2\n\n",
                "text/event-stream",
            ))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let mut events = stream_sse(client.post(format!("{}/sse", server.uri())));

        assert!(matches!(events.next().await.unwrap().unwrap(), Event::Open));
        let Event::Message(m1) = events.next().await.unwrap().unwrap() else {
            panic!("expected message");
        };
        assert_eq!(m1.data, "{\"a\":1}");
        let Event::Message(m2) = events.next().await.unwrap().unwrap() else {
            panic!("expected message");
        };
        assert_eq!(m2.data, "hello");
        let Event::Message(m3) = events.next().await.unwrap().unwrap() else {
            panic!("expected message");
        };
        assert_eq!(m3.data, "line1\nline2");
        let err = events.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("Stream ended"), "got: {err}");
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn embeds_status_retry_after_and_body_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sse"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string("{\"error\":\"rate limited\"}"),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let mut events = stream_sse(client.post(format!("{}/sse", server.uri())));

        let err = events.next().await.unwrap().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Invalid status code: 429"), "got: {msg}");
        assert!(msg.contains("retry-after: 30s"), "got: {msg}");
        assert!(msg.contains("rate limited"), "got: {msg}");
    }
}
