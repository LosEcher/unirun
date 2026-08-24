//! Output coalescing limiter — merge adjacent same-stream output chunks into
//! batches before forwarding, cutting downstream forwarding overhead.
//!
//! Borrowed semantics from grok-bot's `ShellOutputLimiter` (TS):
//! - adjacent same-type chunks are merged into one batch;
//! - a batch flushes when the buffered byte count reaches a threshold
//!   (`max_buffered_bytes`, default 128 KiB) or after a quiet interval
//!   (`flush_interval_ms`, default 100 ms);
//! - `close()` flushes whatever remains (EOF), so coalescing never loses
//!   content — it only changes *when* chunks are forwarded, never *what*
//!   content reaches the consumer, and never the run's exit code.
//!
//! stderr handling is a policy choice: `coalesce_stderr: false` (default)
//! keeps stderr real-time (errors must surface immediately); `true`
//! coalesces it independently with the same threshold/timer.
//!
//! One `OutputCoalescer` per stream (the exec engine creates one for stdout,
//! and one for stderr only when `coalesce_stderr` is set). The type is
//! generic over `StreamKind` so the merge logic stays testable for the
//! mixed-stream case.

use crate::exec::{StreamChunk, StreamKind};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Flush when buffered bytes reach this many (default).
pub const DEFAULT_MAX_BUFFERED_BYTES: usize = 128 * 1024;
/// Flush pending data this long after the last flush (default).
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 100;

/// Effective limits for one coalescer. Zero fields mean "use the default".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalesceConfig {
    pub max_buffered_bytes: usize,
    pub flush_interval_ms: u64,
    /// `false` (default): stderr chunks pass through immediately (real-time
    /// errors). `true`: stderr is coalesced independently like stdout.
    pub coalesce_stderr: bool,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        CoalesceConfig {
            max_buffered_bytes: DEFAULT_MAX_BUFFERED_BYTES,
            flush_interval_ms: DEFAULT_FLUSH_INTERVAL_MS,
            coalesce_stderr: false,
        }
    }
}

impl CoalesceConfig {
    /// Zero fields normalized to the documented defaults.
    pub fn effective(&self) -> CoalesceConfig {
        CoalesceConfig {
            max_buffered_bytes: if self.max_buffered_bytes == 0 {
                DEFAULT_MAX_BUFFERED_BYTES
            } else {
                self.max_buffered_bytes
            },
            flush_interval_ms: if self.flush_interval_ms == 0 {
                DEFAULT_FLUSH_INTERVAL_MS
            } else {
                self.flush_interval_ms
            },
            coalesce_stderr: self.coalesce_stderr,
        }
    }
}

/// How a run should coalesce its streaming output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoalescePolicy {
    /// Enabled with the documented defaults (128 KiB / 100 ms, stderr real-time).
    #[default]
    Default,
    /// Enabled with explicit limits.
    Custom(CoalesceConfig),
    /// Disabled: every decoded chunk is forwarded immediately.
    Off,
}

/// Merge adjacent same-stream chunks in a batch (grok's
/// `coalesceAdjacentShellOutputEvents`): consecutive `stdout` chunks become
/// one `stdout` chunk; a `stderr` chunk breaks the run. Order is preserved.
fn merge_adjacent(chunks: Vec<StreamChunk>) -> Vec<StreamChunk> {
    let mut out: Vec<StreamChunk> = Vec::new();
    for c in chunks {
        match out.last_mut() {
            Some(last) if last.stream == c.stream => {
                last.text.push_str(&c.text);
            }
            _ => out.push(c),
        }
    }
    out
}

struct Inner {
    state: Mutex<State>,
    cond: Condvar,
    tx: mpsc::Sender<StreamChunk>,
    config: CoalesceConfig,
}

#[derive(Default)]
struct State {
    pending: Vec<StreamChunk>,
    pending_bytes: usize,
    /// When the last flush happened (also armed when the first byte of a new
    /// batch arrives) — the timer fires `flush_interval` after it.
    last_flush: Option<Instant>,
    closed: bool,
}

/// A per-stream output coalescer: buffered, threshold- and timer-flushed.
pub struct OutputCoalescer {
    inner: Arc<Inner>,
    handle: Option<thread::JoinHandle<()>>,
}

impl OutputCoalescer {
    /// Create a coalescer that forwards merged batches to `tx`.
    pub fn new(tx: mpsc::Sender<StreamChunk>, config: CoalesceConfig) -> Self {
        let config = config.effective();
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
            tx,
            config,
        });
        let handle = thread::spawn({
            let inner = Arc::clone(&inner);
            move || flush_loop(inner)
        });
        OutputCoalescer {
            inner,
            handle: Some(handle),
        }
    }

    /// Buffer one decoded chunk. Flushes immediately when the threshold is
    /// reached; otherwise arms the timer.
    pub fn push(&self, stream: StreamKind, text: String) {
        if text.is_empty() {
            return;
        }
        let mut st = self.inner.state.lock().unwrap();
        let was_empty = st.pending.is_empty();
        st.pending_bytes += text.len();
        st.pending.push(StreamChunk { stream, text });
        if st.pending_bytes >= self.inner.config.max_buffered_bytes {
            // Threshold: flush now (while holding the lock so concurrent
            // timer flushes stay ordered). Sends are non-blocking on the
            // unbounded channel, so the lock is held only briefly.
            flush_locked(&self.inner, &mut st);
            self.inner.cond.notify_all(); // re-arm the timer after a big flush
        } else {
            if was_empty && st.last_flush.is_none() {
                // Arm the timer at the first byte of a new batch.
                st.last_flush = Some(Instant::now());
            }
            self.inner.cond.notify_one();
        }
    }

    /// Final flush: forward whatever is buffered and stop the timer thread.
    /// Must be called at EOF (from the reading thread) so no content is lost.
    pub fn close(mut self) {
        {
            let mut st = self.inner.state.lock().unwrap();
            st.closed = true;
        }
        self.inner.cond.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Forward the buffered batch (merged) while holding the state lock.
fn flush_locked(inner: &Inner, st: &mut State) {
    if st.pending.is_empty() {
        return;
    }
    let batch = std::mem::take(&mut st.pending);
    st.pending_bytes = 0;
    st.last_flush = Some(Instant::now());
    for merged in merge_adjacent(batch) {
        let _ = inner.tx.send(merged);
    }
}

fn remaining_wait(config: &CoalesceConfig, last_flush: Option<Instant>) -> Duration {
    let interval = Duration::from_millis(config.flush_interval_ms);
    match last_flush {
        Some(t) => interval.saturating_sub(t.elapsed()),
        None => Duration::ZERO,
    }
}

fn flush_loop(inner: Arc<Inner>) {
    loop {
        let mut st = inner.state.lock().unwrap();
        if st.closed {
            if st.pending.is_empty() {
                return;
            }
            flush_locked(&inner, &mut st);
            continue; // re-check: closed + empty → exit
        }
        if st.pending.is_empty() {
            // Nothing buffered: sleep until data or close arrives.
            st = inner.cond.wait(st).unwrap();
            continue;
        }
        let wait = remaining_wait(&inner.config, st.last_flush);
        if wait.is_zero() {
            flush_locked(&inner, &mut st);
            continue;
        }
        let (guard, _timed_out) = inner.cond.wait_timeout(st, wait).unwrap();
        st = guard;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A coalescer with a tiny timer and a huge threshold (so only the timer
    /// triggers), pushing through a channel we can poll.
    fn timer_only(interval_ms: u64) -> (OutputCoalescer, mpsc::Receiver<StreamChunk>) {
        let (tx, rx) = mpsc::channel();
        let config = CoalesceConfig {
            max_buffered_bytes: usize::MAX / 2,
            flush_interval_ms: interval_ms,
            coalesce_stderr: false,
        };
        (OutputCoalescer::new(tx, config), rx)
    }

    fn concat(rx: &mpsc::Receiver<StreamChunk>) -> String {
        rx.try_iter().map(|c| c.text).collect()
    }

    #[test]
    fn merge_adjacent_merges_same_stream_runs() {
        let chunks = vec![
            StreamChunk {
                stream: StreamKind::Stdout,
                text: "a".into(),
            },
            StreamChunk {
                stream: StreamKind::Stdout,
                text: "b".into(),
            },
            StreamChunk {
                stream: StreamKind::Stderr,
                text: "warn".into(),
            },
            StreamChunk {
                stream: StreamKind::Stdout,
                text: "c".into(),
            },
        ];
        let merged = merge_adjacent(chunks);
        assert_eq!(merged.len(), 3, "got: {:?}", merged);
        assert_eq!(merged[0].stream, StreamKind::Stdout);
        assert_eq!(merged[0].text, "ab");
        assert_eq!(merged[1].stream, StreamKind::Stderr);
        assert_eq!(merged[1].text, "warn");
        assert_eq!(merged[2].stream, StreamKind::Stdout);
        assert_eq!(merged[2].text, "c");
    }

    #[test]
    fn empty_text_is_ignored() {
        let (co, rx) = timer_only(50);
        co.push(StreamKind::Stdout, String::new());
        co.push(StreamKind::Stdout, "x".into());
        co.close();
        assert_eq!(concat(&rx), "x");
    }

    #[test]
    fn threshold_flush_merges_into_one_batch() {
        let (tx, rx) = mpsc::channel();
        let co = OutputCoalescer::new(
            tx,
            CoalesceConfig {
                max_buffered_bytes: 10,
                flush_interval_ms: 60_000, // effectively timer-disabled
                coalesce_stderr: false,
            },
        );
        // 3 × 4 bytes = 12 ≥ 10 → the third push must flush all three
        // adjacent stdout chunks as a single merged batch.
        co.push(StreamKind::Stdout, "aaaa".into());
        co.push(StreamKind::Stdout, "bbbb".into());
        co.push(StreamKind::Stdout, "cccc".into());
        let first = rx.try_recv().expect("threshold flush");
        assert_eq!(first.stream, StreamKind::Stdout);
        assert_eq!(first.text, "aaaabbbbcccc", "got: {:?}", first);
        co.close();
        assert!(rx.try_recv().is_err(), "nothing left after close");
    }

    #[test]
    fn timer_flush_fires_after_interval() {
        let (co, rx) = timer_only(30);
        co.push(StreamKind::Stdout, "tick".into());
        // No close: the timer alone must forward the pending chunk.
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let mut got = String::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(c) => {
                    got.push_str(&c.text);
                    break;
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        assert_eq!(got, "tick", "timer flush did not fire");
        co.close();
    }

    #[test]
    fn close_flushes_remaining_content_preserved() {
        let (co, rx) = timer_only(60_000);
        let mut pushed = String::new();
        for i in 0..50 {
            let s = format!("chunk-{:03}\n", i);
            pushed.push_str(&s);
            co.push(StreamKind::Stdout, s);
        }
        co.close();
        assert_eq!(concat(&rx), pushed, "content must be preserved exactly");
    }

    #[test]
    fn mixed_streams_stay_in_order_and_independent() {
        let (co, rx) = timer_only(60_000);
        co.push(StreamKind::Stdout, "o1".into());
        co.push(StreamKind::Stderr, "e1".into());
        co.push(StreamKind::Stdout, "o2".into());
        co.close();
        let chunks: Vec<StreamChunk> = rx.try_iter().collect();
        let stdout: String = chunks
            .iter()
            .filter(|c| c.stream == StreamKind::Stdout)
            .map(|c| c.text.as_str())
            .collect();
        let stderr: String = chunks
            .iter()
            .filter(|c| c.stream == StreamKind::Stderr)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(stdout, "o1o2");
        assert_eq!(stderr, "e1");
    }

    #[test]
    fn effective_normalizes_zero_fields() {
        let cfg = CoalesceConfig {
            max_buffered_bytes: 0,
            flush_interval_ms: 0,
            coalesce_stderr: true,
        }
        .effective();
        assert_eq!(cfg.max_buffered_bytes, DEFAULT_MAX_BUFFERED_BYTES);
        assert_eq!(cfg.flush_interval_ms, DEFAULT_FLUSH_INTERVAL_MS);
        assert!(cfg.coalesce_stderr);
    }
}
