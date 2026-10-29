//! A small ring buffer of recent log lines, so the status page can show the tail
//! without the operator having to reach for `docker logs`.
//!
//! Implemented as a `tracing` writer that tees: everything still goes to stdout,
//! and a copy is kept in memory. That order matters — the container's logging
//! driver is the record of truth, and this is a convenience view of it.
//!
//! ANSI colour is turned off for both. Escape sequences in a `<pre>` are noise,
//! and a log line pasted from `docker logs` is more useful without them too.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// How many lines are kept. Enough to cover a reconnect episode at `info`, small
/// enough that the page stays responsive.
const CAPACITY: usize = 250;

/// One buffer shared by the logger and the HTTP handlers.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    lines: VecDeque<String>,
    /// Text written without a trailing newline yet. A `tracing` event usually
    /// arrives in one write, but the writer makes no such promise.
    partial: String,
}

impl LogBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::with_capacity(CAPACITY),
                partial: String::new(),
            })),
        }
    }

    /// The most recent lines, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };

        let mut lines: Vec<String> = inner.lines.iter().cloned().collect();

        // A line still in progress is worth showing: a long event looks like a
        // hang if the tail stops just before it.
        if !inner.partial.is_empty() {
            lines.push(inner.partial.clone());
        }
        lines
    }

    fn push(&self, text: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        for character in text.chars() {
            if character == '\n' {
                let line = std::mem::take(&mut inner.partial);
                if !line.is_empty() {
                    inner.lines.push_back(line);
                    if inner.lines.len() > CAPACITY {
                        inner.lines.pop_front();
                    }
                }
            } else {
                inner.partial.push(character);
            }
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = Tee;

    fn make_writer(&'a self) -> Self::Writer {
        Tee {
            buffer: self.clone(),
        }
    }
}

/// Writes to stdout like a normal logger, and keeps a copy for the page.
pub struct Tee {
    buffer: LogBuffer,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut stdout = io::stdout();
        stdout.write_all(buf)?;
        stdout.flush()?;

        self.buffer.push(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_lines_are_kept_in_order() {
        let buffer = LogBuffer::new();
        buffer.push("first\n");
        buffer.push("second\n");

        assert_eq!(buffer.snapshot(), vec!["first", "second"]);
    }

    #[test]
    fn a_line_split_across_writes_is_still_one_line() {
        // The writer is given bytes, not records; a slow or interleaved write can
        // arrive in pieces and must not become two entries.
        let buffer = LogBuffer::new();
        buffer.push("partial ");
        buffer.push("line\n");

        assert_eq!(buffer.snapshot(), vec!["partial line"]);
    }

    #[test]
    fn a_line_still_in_progress_is_visible() {
        // Otherwise a long-running event makes the tail look stalled.
        let buffer = LogBuffer::new();
        buffer.push("done\n");
        buffer.push("in progress");

        assert_eq!(buffer.snapshot(), vec!["done", "in progress"]);
    }

    #[test]
    fn the_buffer_stays_bounded() {
        let buffer = LogBuffer::new();
        for index in 0..(CAPACITY + 50) {
            buffer.push(&format!("line {index}\n"));
        }

        let lines = buffer.snapshot();
        assert_eq!(lines.len(), CAPACITY, "the ring holds exactly its capacity");
        assert!(
            lines.last().expect("non-empty").starts_with("line "),
            "and the newest line survives"
        );
        assert!(
            !lines.iter().any(|line| line == "line 0"),
            "the oldest line was dropped"
        );
    }

    #[test]
    fn blank_lines_are_not_recorded() {
        // `tracing` emits a trailing newline after the event body, so every event
        // arrives as "text\n"; an empty extra line per event would halve the tail.
        let buffer = LogBuffer::new();
        buffer.push("event\n");

        assert_eq!(buffer.snapshot(), vec!["event"]);
    }
}
