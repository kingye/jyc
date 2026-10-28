//! Reading prior context back from a topic's `agent-context.jsonl` journal.
//!
//! The journal is append-only: one message per line, oldest first. This
//! reader parses it **backwards**, in blocks, and hands out the tail the
//! caller asked for. Only the tail is parsed, so a journal that grew to
//! hundreds of megabytes costs the same per turn as a small one.
//!
//! The reader owns the journal's read side — including the pair count it
//! keeps in process between turns. The writer
//! ([`crate::session::save_raw_context`]) owns the write side and tells the
//! reader what it appended, so the count stays current instead of being
//! recounted.

use jyc_core::session_state::AGENT_CONTEXT_FILE;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::types::PriorDepth;

/// How many bytes one backward read pulls. Internal detail — callers ask
/// for turns, never bytes.
const READ_BLOCK: u64 = 64 * 1024;

/// How many pairs a journal holds, kept in process between turns.
///
/// Counting them means reading the whole journal, so the count is kept
/// and the writer adds to it as it appends. It is keyed by the journal's
/// length and mtime: a journal that changed under us is counted again.
struct PairCount {
    /// The journal length the count belongs to.
    file_len: u64,
    /// Its modification time.
    file_mtime: Option<SystemTime>,
    /// How many pairs the journal holds.
    total_pairs: usize,
}

fn pair_counts() -> &'static Mutex<HashMap<String, PairCount>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PairCount>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where the writer appends the next message.
pub(crate) struct AppendPoint {
    /// The offset the new messages are written at. Anything after it is
    /// dropped.
    pub offset: u64,
    /// Bytes written before the new messages — the newline a recovered
    /// torn tail is missing.
    pub prefix: &'static [u8],
}

/// A slice of the journal: the tail the caller asked for, oldest first.
pub(crate) struct PriorSlice {
    /// The messages, oldest first. The slice starts at a turn boundary —
    /// the user message that opens the oldest turn it holds — or at the
    /// journal's first message.
    pub messages: Vec<Value>,
    /// The scan reached the journal's head: the slice covers the whole
    /// journal, nothing older was left to read.
    pub reached_head: bool,
    /// Absolute journal byte offset of each message's line start, aligned
    /// with `messages`. Only [`ContextReader::read_from`] fills this: the
    /// sliding window needs it to move its anchor a whole turn at a time.
    /// Empty when the offsets were not tracked.
    pub offsets: Vec<u64>,
}

/// The journal's parsed tail, plus where the next backward read ends.
struct Tail {
    /// The offset the next backward read ends at.
    read_offset: u64,
    /// The tail of the line that spans that offset.
    carry: Option<Vec<u8>>,
    /// The parsed messages, oldest first.
    messages: Vec<Value>,
}

impl Tail {
    /// The tail of a fresh scan: nothing read yet.
    fn empty(file_len: u64) -> Tail {
        Tail {
            read_offset: file_len,
            carry: None,
            messages: Vec::new(),
        }
    }
}

/// One block of the journal, parsed.
struct BlockParse {
    /// The complete lines, oldest first.
    messages: Vec<Value>,
    /// The fragment that continues further back.
    carry: Option<Vec<u8>>,
    /// The offset the next backward read ends at.
    read_offset: u64,
    /// Whether the scan must stop: a corrupt line ends it — the newer
    /// turns stay, the older ones are not trusted.
    stop: bool,
}

/// Reads prior context back from a topic's `agent-context.jsonl` journal.
pub(crate) struct ContextReader {
    topic_name: String,
    journal_path: PathBuf,
}

impl ContextReader {
    /// A reader for the journal in `state_dir`.
    pub(crate) fn new(topic_name: &str, state_dir: &Path) -> Self {
        Self {
            topic_name: topic_name.to_string(),
            journal_path: state_dir.join(AGENT_CONTEXT_FILE),
        }
    }

    /// The tail of the journal the caller needs, oldest first.
    ///
    /// The slice starts at a turn boundary — the user message that opens
    /// the oldest turn it holds — so the strategies see the same turns
    /// they would have seen in the full journal.
    pub(crate) async fn read_prior(&self, depth: PriorDepth) -> PriorSlice {
        let Some((file_len, _)) = self.stat().await else {
            return PriorSlice {
                messages: Vec::new(),
                reached_head: true,
                offsets: Vec::new(),
            };
        };

        // Read the journal backwards until the tail covers what the caller
        // asked for, or the journal's head is reached. Only the tail is
        // parsed, so the cost does not grow with the journal's length.
        let acc = self.scan_backwards(Tail::empty(file_len), depth).await;

        let start = slice_start(&acc.messages, depth);
        PriorSlice {
            messages: acc.messages[start..].to_vec(),
            reached_head: acc.read_offset == 0,
            offsets: Vec::new(),
        }
    }

    /// The journal forward from `offset` (a line start), oldest first.
    ///
    /// Used by the anchored sliding window: `agent-session.json` records
    /// `context_offset`, the line start where the window's oldest retained
    /// turn begins, and each load reads `[offset..EOF)` — so the prior
    /// grows by append between slides and the read amount stays bounded by
    /// the turn bound that triggers them. Returns an empty slice when
    /// `offset` is at or past the journal's end. Every parsed message
    /// carries its line-start offset in [`PriorSlice::offsets`], so the
    /// caller can advance the anchor a whole turn at a time without
    /// re-scanning.
    pub(crate) async fn read_from(&self, offset: u64) -> PriorSlice {
        let Some((file_len, _)) = self.stat().await else {
            return PriorSlice {
                messages: Vec::new(),
                reached_head: true,
                offsets: Vec::new(),
            };
        };
        let mut messages = Vec::new();
        let mut offsets = Vec::new();
        if offset >= file_len {
            return PriorSlice {
                messages,
                reached_head: offset == 0,
                offsets,
            };
        }

        // Read forward in blocks, carrying a partial line across them.
        // Invalid lines are skipped — a torn mid-file line does not end
        // the read (unlike the backward scan): the caller asked for a
        // known position, not for the deepest trustworthy tail.
        let mut pos = offset;
        let mut carry: Vec<u8> = Vec::new();
        let mut carry_start = offset;
        while pos < file_len {
            let end = (pos + READ_BLOCK).min(file_len);
            let Some((_, bytes)) = self.read_block(pos, end).await else {
                break;
            };
            let block_start = pos;
            pos = end;
            let mut rest: &[u8] = &bytes;
            while let Some(i) = rest.iter().position(|&b| b == b'\n') {
                let line = &rest[..i];
                let line_start = block_start + (bytes.len() - rest.len()) as u64;
                rest = &rest[i + 1..];
                let mut full = std::mem::take(&mut carry);
                full.extend_from_slice(line);
                if let Ok(v) = serde_json::from_slice::<Value>(&full)
                    && crate::provider::is_valid_message(&v)
                {
                    offsets.push(carry_start);
                    messages.push(v);
                }
                carry_start = line_start + i as u64 + 1;
            }
            carry.extend_from_slice(rest);
        }
        // A trailing line without a newline is a torn tail — the writer's
        // `clean_end` owns that repair; the reader drops it.
        PriorSlice {
            messages,
            // A read from a non-zero offset starts mid-journal: the head it
            // does not cover is exactly what `reached_head == false` must
            // mean, which is `offset != 0`. The corruption guard in
            // `finish_load` deletes the journal when a head-covering slice
            // holds no assistant message, and a window that starts at an
            // anchor is not entitled to that verdict.
            reached_head: offset == 0,
            offsets,
        }
    }

    /// Every message in the journal, oldest first.
    ///
    /// Unlike [`Self::read_prior`] this reads the whole journal, and it
    /// is not kept: the whole file in memory would pin it for the rest
    /// of the process' life.
    pub(crate) async fn read_all(&self) -> Vec<Value> {
        let Some((file_len, _)) = self.stat().await else {
            return Vec::new();
        };
        let acc = self
            .scan_backwards(Tail::empty(file_len), PriorDepth::All)
            .await;
        acc.messages
    }

    /// How many pairs the whole journal holds.
    ///
    /// The first call counts the journal; the writer keeps the count up
    /// to date as it appends, so later calls are free.
    pub(crate) async fn total_pairs(&self) -> usize {
        let Some((file_len, file_mtime)) = self.stat().await else {
            return 0;
        };
        if let Some(total) = self.cached_pair_count(file_len, file_mtime) {
            return total;
        }
        let messages = self.read_all().await;
        let total = crate::session::pairs_tail_start(&messages, usize::MAX).1;
        self.cache_pair_count(total, file_len, file_mtime);
        total
    }

    /// Where a new message is appended.
    ///
    /// The journal's tail may be torn: a crash mid-append leaves a partial
    /// line behind. That fragment is dropped — unless it is a complete
    /// message that lost only its newline (the usual shape, the write was
    /// cut at its last byte), which is kept and terminated.
    ///
    /// Returns `None` when the cut point cannot be determined — the journal
    /// cannot be read. The caller must skip the append then: appending at 0
    /// would discard a history that is merely unreadable.
    pub(crate) async fn clean_end(&self) -> Option<AppendPoint> {
        let Some((file_len, _)) = self.stat().await else {
            // No journal yet: the append creates it.
            return Some(AppendPoint {
                offset: 0,
                prefix: b"",
            });
        };
        if file_len == 0 {
            return Some(AppendPoint {
                offset: 0,
                prefix: b"",
            });
        }

        // Scan back for the last newline. The torn tail can be longer than
        // one block, so a block without a newline is not the answer.
        let mut end = file_len;
        loop {
            let start = end.saturating_sub(READ_BLOCK);
            let (_, bytes) = self.read_block(start, end).await?;
            if let Some(i) = bytes.iter().rposition(|&b| b == b'\n') {
                let cut = start + i as u64 + 1;
                return Some(self.append_point_after(cut, file_len).await);
            }
            if start == 0 {
                // The whole file is one unterminated line: a torn tail from
                // the very first append. It is dropped, the file starts
                // over.
                return Some(AppendPoint {
                    offset: 0,
                    prefix: b"",
                });
            }
            end = start;
        }
    }

    /// The append point for a journal whose last newline ends at `cut`:
    /// either `cut` itself (nothing follows the newline), or — when the
    /// fragment after it is a complete message missing only its newline —
    /// the fragment's end, with the newline supplied by the prefix.
    async fn append_point_after(&self, cut: u64, file_len: u64) -> AppendPoint {
        if cut == file_len {
            return AppendPoint {
                offset: cut,
                prefix: b"",
            };
        }
        let Some((_, bytes)) = self.read_block(cut, file_len).await else {
            // The fragment cannot be read: drop it rather than risk
            // continuing a partial line.
            return AppendPoint {
                offset: cut,
                prefix: b"",
            };
        };
        let complete = serde_json::from_slice::<Value>(&bytes)
            .is_ok_and(|v| crate::provider::is_valid_message(&v));
        if complete {
            return AppendPoint {
                offset: file_len,
                prefix: b"\n",
            };
        }
        AppendPoint {
            offset: cut,
            prefix: b"",
        }
    }

    /// Record the messages a writer appended, so the pair count stays
    /// current instead of being recounted from the whole journal.
    ///
    /// `before` describes the journal before the append, `after` the
    /// journal after it. A count kept for a different file is dropped:
    /// the journal moved, the reader must not serve a stale count.
    pub(crate) fn note_append(
        &self,
        appended: &[Value],
        before: (u64, Option<SystemTime>),
        after: (u64, Option<SystemTime>),
    ) {
        let mut cache = pair_counts().lock().expect("context cache poisoned");
        let Some(entry) = cache.get_mut(&self.topic_name) else {
            return;
        };
        if entry.file_len != before.0 || entry.file_mtime != before.1 {
            // The journal is not the one the count was taken from.
            cache.remove(&self.topic_name);
            return;
        }
        // The appended messages are this turn: its user message and its
        // assistant reply are both in here, so the turn is counted whole.
        // The reader repairs the journal as it reads it; applying the same
        // repair here keeps the count from including a turn that will be
        // cut away.
        let appended = crate::provider::repair_dangling_tool_calls(appended.to_vec());
        let added = crate::session::pairs_tail_start(&appended, usize::MAX).1;
        entry.total_pairs += added;
        entry.file_len = after.0;
        entry.file_mtime = after.1;
    }

    /// Drop the cached count — the journal was reset or rewritten.
    pub(crate) fn invalidate(&self) {
        pair_counts()
            .lock()
            .expect("context cache poisoned")
            .remove(&self.topic_name);
    }

    /// The journal's length and modification time, or `None` when it does
    /// not exist.
    pub(crate) async fn stat(&self) -> Option<(u64, Option<SystemTime>)> {
        let meta = tokio::fs::metadata(&self.journal_path).await.ok()?;
        Some((meta.len(), meta.modified().ok()))
    }

    /// The cached pair count, when it still matches the journal.
    fn cached_pair_count(&self, file_len: u64, file_mtime: Option<SystemTime>) -> Option<usize> {
        let cache = pair_counts().lock().expect("context cache poisoned");
        let entry = cache.get(&self.topic_name)?;
        if entry.file_len != file_len || entry.file_mtime != file_mtime {
            return None;
        }
        Some(entry.total_pairs)
    }

    /// Store the journal's pair count.
    fn cache_pair_count(&self, total: usize, file_len: u64, file_mtime: Option<SystemTime>) {
        let mut cache = pair_counts().lock().expect("context cache poisoned");
        cache.insert(
            self.topic_name.clone(),
            PairCount {
                file_len,
                file_mtime,
                total_pairs: total,
            },
        );
    }

    /// Read the journal backwards until `depth` is covered or the head is
    /// reached, continuing from `acc`.
    async fn scan_backwards(&self, mut acc: Tail, depth: PriorDepth) -> Tail {
        loop {
            if acc.read_offset == 0 || depth_satisfied(&acc.messages, depth) {
                break;
            }
            let Some(block) = self
                .read_block_before(acc.read_offset, acc.carry.take())
                .await
            else {
                break;
            };
            let stop = block.stop;
            // The block is older than what we have: put it in front.
            let mut messages = block.messages;
            messages.extend(acc.messages);
            acc = Tail {
                read_offset: block.read_offset,
                carry: block.carry,
                messages,
            };
            if stop {
                break;
            }
        }
        // The whole-file load path repaired dangling tool calls before the
        // strategies saw the context; the append-only reader must do the
        // same, or the repair would only run on the wire — where it drops
        // the dangling assistant *and everything after it*, the current
        // turn included.
        acc.messages = crate::provider::repair_dangling_tool_calls(acc.messages);
        acc
    }

    /// Read the block that ends at `offset` — the one before the tail.
    ///
    /// `carry` is the tail of the line that continues into this block
    /// from the newer side; the block's own first fragment continues
    /// further back and comes back as the new carry.
    async fn read_block_before(&self, offset: u64, carry: Option<Vec<u8>>) -> Option<BlockParse> {
        let start = offset.saturating_sub(READ_BLOCK);
        let (prev, bytes) = self.read_block(start, offset).await?;
        let mut block = parse_block(prev, &bytes, carry);
        // The next read ends where this block starts.
        block.read_offset = start;
        Some(block)
    }

    /// Read `[start, end)` plus the byte before `start` — it tells
    /// whether the block starts mid-line.
    async fn read_block(&self, start: u64, end: u64) -> Option<(Option<u8>, Vec<u8>)> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let has_prev = start > 0;
        let from = if has_prev { start - 1 } else { start };
        let mut f = tokio::fs::File::open(&self.journal_path).await.ok()?;
        f.seek(std::io::SeekFrom::Start(from)).await.ok()?;
        let mut buf = vec![0u8; (end - from) as usize];
        f.read_exact(&mut buf).await.ok()?;
        if has_prev {
            let prev = buf[0];
            buf.remove(0);
            Some((Some(prev), buf))
        } else {
            Some((None, buf))
        }
    }
}

/// Whether the parsed tail already covers what the caller asked for.
fn depth_satisfied(messages: &[Value], depth: PriorDepth) -> bool {
    match depth {
        // The whole journal — only the head satisfies this.
        PriorDepth::All => false,
        PriorDepth::Window { pairs, openers } => {
            crate::session::pairs_tail_start(messages, pairs).1 >= pairs
                && crate::session::count_turn_openers(messages) >= openers
        }
    }
}

/// Where the slice starts: the turn boundary that covers both halves of
/// the depth — the last `pairs` pair-producing turns and the last
/// `openers` turn openers.
fn slice_start(messages: &[Value], depth: PriorDepth) -> usize {
    match depth {
        PriorDepth::All => 0,
        PriorDepth::Window { pairs, openers } => {
            let pair_start = crate::session::pairs_tail_start(messages, pairs).0;
            let opener_start = crate::session::turn_openers_start(messages, openers);
            pair_start.min(opener_start)
        }
    }
}

/// Parse one block of the journal.
///
/// `prev` is the byte before the block (`None` at the file's start) — it
/// tells whether the block starts mid-line. `carry` is the tail of the
/// line that continues into this block from the newer side.
fn parse_block(prev: Option<u8>, bytes: &[u8], carry: Option<Vec<u8>>) -> BlockParse {
    // The block may start mid-line: its first fragment is the tail of a
    // line whose head is further back, and the rest of the block starts
    // at a line boundary.
    let (first_fragment, rest): (&[u8], &[u8]) = match prev {
        None | Some(b'\n') => (&bytes[..0], bytes),
        Some(_) => match bytes.iter().position(|&b| b == b'\n') {
            Some(i) => (&bytes[..i], &bytes[i + 1..]),
            // The whole block lies inside one line.
            None => (bytes, &bytes[bytes.len()..]),
        },
    };

    // A block without a newline of its own lies entirely inside one line:
    // its content joins the carry and waits for the head further back.
    if rest.is_empty() {
        let mut next = first_fragment.to_vec();
        if let Some(tail) = carry {
            next.extend_from_slice(&tail);
        }
        return BlockParse {
            messages: Vec::new(),
            carry: Some(next),
            stop: false,
            read_offset: 0,
        };
    }

    // The line that spans the newer boundary: its head is the block's
    // last line.
    let body = rest.strip_suffix(b"\n").unwrap_or(rest);
    let head_start = body
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let (complete, head) = body.split_at(head_start);

    // The line that spans the older boundary: this block's first
    // fragment. The block starts mid-line unless the byte before it
    // ended a line.
    let next_carry = match prev {
        None | Some(b'\n') => None,
        Some(_) => Some(first_fragment.to_vec()),
    };

    // The line that spans the newer boundary is complete when the carry
    // supplies its tail, or when the block ends the line itself. A block
    // that ends mid-line without a carry is the journal's torn tail: a
    // crash mid-append. It is kept when it is a complete message that lost
    // only its newline — the usual shape, the write was cut at its last
    // byte — and dropped otherwise.
    let spanning = if carry.is_some() || bytes.ends_with(b"\n") {
        let mut line = head.to_vec();
        if let Some(tail) = carry {
            line.extend_from_slice(&tail);
        }
        Some(line)
    } else if serde_json::from_slice::<Value>(head)
        .is_ok_and(|v| crate::provider::is_valid_message(&v))
    {
        Some(head.to_vec())
    } else {
        None
    };

    let mut stop = false;
    let mut joined: Option<Value> = None;
    if let Some(line) = spanning
        && !line.is_empty()
    {
        match serde_json::from_slice::<Value>(&line) {
            Ok(v) => {
                // An assistant message with neither content nor tool
                // calls carries nothing — it is dropped here, before
                // the turn counting, so the slice holds the same
                // turns the strategies would have seen.
                if crate::provider::is_valid_message(&v) {
                    joined = Some(v);
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "agent-context.jsonl: unparseable line, keeping the newer turns"
                );
                stop = true;
            }
        }
    }

    // The complete lines, newest first: a corrupt line ends the scan and
    // the older lines are dropped with it.
    let mut older: Vec<Value> = Vec::new();
    if !stop {
        for line in complete.split(|&b| b == b'\n').rev() {
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(v) => {
                    if crate::provider::is_valid_message(&v) {
                        older.push(v);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "agent-context.jsonl: unparseable line, keeping the newer turns"
                    );
                    stop = true;
                    break;
                }
            }
        }
        older.reverse();
    }

    let mut messages = older;
    if let Some(v) = joined {
        messages.push(v);
    }
    BlockParse {
        messages,
        carry: next_carry,
        stop,
        read_offset: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A journal body with `turns` user+assistant pairs.
    fn journal(turns: usize) -> String {
        let mut out = String::new();
        for i in 1..=turns {
            out.push_str(&format!(
                "{}\n",
                json!({"role": "user", "content": format!("u{i}")})
            ));
            out.push_str(&format!(
                "{}\n",
                json!({"role": "assistant", "content": format!("a{i}")})
            ));
        }
        out
    }

    /// A reader over a journal holding `body`.
    async fn reader_for(tmp: &tempfile::TempDir, body: &str) -> ContextReader {
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join(AGENT_CONTEXT_FILE), body)
            .await
            .unwrap();
        ContextReader::new("reader-test", &state_dir)
    }

    #[tokio::test]
    async fn reads_the_last_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(5)).await;
        let slice = reader
            .read_prior(PriorDepth::Window {
                pairs: 2,
                openers: 2,
            })
            .await;
        assert_eq!(slice.messages.len(), 4, "the last two turns");
        assert_eq!(slice.messages[0]["content"], "u4");
        assert_eq!(slice.messages[3]["content"], "a5");
    }

    #[tokio::test]
    async fn the_slice_starts_at_a_turn_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(5)).await;
        let slice = reader
            .read_prior(PriorDepth::Window {
                pairs: 1,
                openers: 1,
            })
            .await;
        assert_eq!(slice.messages.len(), 2);
        assert_eq!(slice.messages[0]["role"], "user");
        assert_eq!(slice.messages[0]["content"], "u5");
    }

    #[tokio::test]
    async fn an_unanswered_user_message_is_kept_with_its_opener() {
        // An opener without a reply: the strategies read it (it is an
        // opener), and the slice must not cut it off.
        let body = format!(
            "{}\n{}\n{}\n",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2 unanswered"}),
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader
            .read_prior(PriorDepth::Window {
                pairs: 1,
                openers: 2,
            })
            .await;
        assert_eq!(slice.messages.len(), 3, "both openers are in the slice");
        assert_eq!(slice.messages[2]["content"], "u2 unanswered");
    }

    #[tokio::test]
    async fn a_torn_tail_is_dropped() {
        let body = format!(
            "{}\n{{\"role\":\"user\",\"content\":\"tor",
            json!({"role": "assistant", "content": "survived"})
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_prior(PriorDepth::All).await;
        assert_eq!(slice.messages.len(), 1, "the torn line is dropped");
        assert_eq!(slice.messages[0]["content"], "survived");
    }

    #[tokio::test]
    async fn a_torn_tail_that_lost_only_its_newline_is_kept() {
        // The usual crash shape: every byte of the message reached disk but
        // the newline did not. The whole-file parser used to recover it.
        let body = format!(
            "{}\n{}\n{}",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "assistant", "content": "a2"}),
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_prior(PriorDepth::All).await;
        assert_eq!(slice.messages.len(), 3, "the completed message is kept");
        assert_eq!(slice.messages[2]["content"], "a2");
    }

    #[tokio::test]
    async fn a_dangling_tool_call_is_repaired_out_of_the_slice() {
        // The wire-side repair drops a dangling assistant and everything
        // after it — the current turn included. The slice must be clean
        // before it gets there.
        let body = format!(
            "{}\n{}\n{}\n",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "x", "arguments": "{}"}
                }]
            }),
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_prior(PriorDepth::All).await;
        assert_eq!(slice.messages.len(), 2, "the dangling assistant is dropped");
        assert_eq!(slice.messages[1]["content"], "a1");
    }

    #[tokio::test]
    async fn clean_end_finds_the_last_newline_behind_a_long_torn_tail() {
        // The torn tail is longer than one block: a single backward read
        // finds no newline in it, and cutting at 0 would take the whole
        // history away.
        let body = format!(
            "{}\n{{\"role\":\"user\",\"content\":\"{}",
            json!({"role": "assistant", "content": "survived"}),
            "x".repeat(READ_BLOCK as usize * 2)
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let point = reader.clean_end().await.expect("the cut point is known");
        assert_eq!(
            point.offset,
            body.find('\n').unwrap() as u64 + 1,
            "the history is kept, the fragment dropped"
        );
        assert!(point.prefix.is_empty());
    }

    #[tokio::test]
    async fn clean_end_keeps_a_torn_tail_that_is_a_complete_message() {
        let body = format!(
            "{}\n{}",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let point = reader.clean_end().await.expect("the cut point is known");
        assert_eq!(point.offset, body.len() as u64, "the message is kept");
        assert_eq!(
            point.prefix,
            b"\n".as_slice(),
            "the missing newline is written back"
        );
    }

    #[tokio::test]
    async fn clean_end_cuts_a_partial_tail_at_the_last_newline() {
        let body = format!(
            "{}\n{{\"role\":\"user\",\"cont",
            json!({"role": "assistant", "content": "survived"}),
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let point = reader.clean_end().await.expect("the cut point is known");
        assert_eq!(point.offset, body.find('\n').unwrap() as u64 + 1);
        assert!(point.prefix.is_empty());
    }

    #[tokio::test]
    async fn a_line_longer_than_a_block_is_read_as_one_message() {
        // A message whose serialized form spans several blocks: the reader
        // has to join the fragments back into one line.
        let big = "x".repeat(READ_BLOCK as usize * 2);
        let body = format!(
            "{}\n{}\n",
            json!({"role": "user", "content": big}),
            json!({"role": "assistant", "content": "done"})
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_prior(PriorDepth::All).await;
        assert_eq!(slice.messages.len(), 2);
        assert_eq!(slice.messages[0]["content"], big);
        assert_eq!(slice.messages[1]["content"], "done");
    }

    #[tokio::test]
    async fn a_corrupt_line_keeps_the_newer_turns() {
        let body = format!(
            "{}\nnot json\n{}\n",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a2"})
        );
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_prior(PriorDepth::All).await;
        assert_eq!(slice.messages.len(), 1, "the corrupt line ends the scan");
        assert_eq!(slice.messages[0]["content"], "a2");
    }

    #[tokio::test]
    async fn read_all_returns_the_whole_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(4)).await;
        let all = reader.read_all().await;
        assert_eq!(all.len(), 8);
        assert_eq!(all[0]["content"], "u1");
        assert_eq!(all[7]["content"], "a4");
    }

    #[tokio::test]
    async fn total_pairs_counts_the_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(4)).await;
        assert_eq!(reader.total_pairs().await, 4);
    }

    #[tokio::test]
    async fn a_window_wider_than_the_journal_returns_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(2)).await;
        let slice = reader
            .read_prior(PriorDepth::Window {
                pairs: 10,
                openers: 10,
            })
            .await;
        assert_eq!(slice.messages.len(), 4);
        assert!(slice.reached_head, "the scan reached the head");
    }

    #[tokio::test]
    async fn the_pair_count_grows_with_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = reader_for(&tmp, &journal(3)).await;
        assert_eq!(reader.total_pairs().await, 3);

        // The writer appends a turn and tells the reader what it added.
        let appended = vec![
            json!({"role": "user", "content": "u4"}),
            json!({"role": "assistant", "content": "a4"}),
        ];
        let state_dir = tmp.path().join(".jyc");
        let before = reader.stat().await.unwrap();
        let mut body = journal(3);
        for v in &appended {
            body.push_str(&format!("{}\n", v));
        }
        tokio::fs::write(state_dir.join(AGENT_CONTEXT_FILE), body)
            .await
            .unwrap();
        let after = reader.stat().await.unwrap();
        reader.note_append(&appended, before, after);

        assert_eq!(
            reader.total_pairs().await,
            4,
            "the count followed the append"
        );

        let slice = reader
            .read_prior(PriorDepth::Window {
                pairs: 1,
                openers: 1,
            })
            .await;
        assert_eq!(slice.messages.len(), 2);
        assert_eq!(slice.messages[0]["content"], "u4");
        assert_eq!(slice.messages[1]["content"], "a4");
    }

    #[tokio::test]
    async fn read_from_parses_forward_with_line_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let body = journal(3);
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_from(0).await;
        assert_eq!(slice.messages.len(), 6);
        assert_eq!(slice.offsets.len(), 6);
        assert_eq!(slice.offsets[0], 0);
        assert!(slice.reached_head);
        // Each offset points at its message's line start.
        let bytes = body.as_bytes();
        for (i, &off) in slice.offsets.iter().enumerate() {
            let line = body.lines().nth(i).unwrap().as_bytes();
            assert_eq!(
                &bytes[off as usize..off as usize + line.len()],
                line,
                "offset {off} must start line {i}"
            );
        }
    }

    #[tokio::test]
    async fn read_from_mid_journal_starts_at_the_offset_line() {
        let tmp = tempfile::tempdir().unwrap();
        let body = journal(3);
        let reader = reader_for(&tmp, &body).await;
        // Byte offset of the "u3" line: find the text, walk back to the
        // preceding newline.
        let pos = body.find("\"u3\"").unwrap();
        let line_start = body[..pos].rfind('\n').map(|n| n + 1).unwrap_or(0) as u64;
        let slice = reader.read_from(line_start).await;
        assert_eq!(slice.messages.len(), 2, "only the u3/a3 turn");
        assert_eq!(slice.messages[0]["content"], "u3");
        assert_eq!(slice.messages[1]["content"], "a3");
        assert_eq!(slice.offsets[0], line_start);
        assert!(
            !slice.reached_head,
            "a read from a non-zero offset does not cover the journal's head"
        );
    }

    #[tokio::test]
    async fn read_from_past_eof_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let body = journal(2);
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_from(body.len() as u64 + 100).await;
        assert!(slice.messages.is_empty());
        assert!(
            !slice.reached_head,
            "an offset past the end never reached the head — only offset 0 did"
        );
    }

    #[tokio::test]
    async fn read_from_skips_invalid_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\nnot-json\n{}\n",
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        );
        let reader = reader_for(&tmp, &body).await;
        let slice = reader.read_from(0).await;
        assert_eq!(slice.messages.len(), 2, "the corrupt line is skipped");
        // Offsets align with the surviving messages, not the raw lines.
        let bytes = body.as_bytes();
        let line0 = body.lines().next().unwrap().as_bytes();
        assert_eq!(
            &bytes[slice.offsets[0] as usize..slice.offsets[0] as usize + line0.len()],
            line0
        );
    }
}
