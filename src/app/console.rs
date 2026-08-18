//! Live compiler console.
//!
//! Long compile/edit operations used to render only a spinner and an elapsed
//! counter, leaving the user unable to tell "working fine" from "hung". A
//! [`CompileConsole`] is a shared line buffer the engine (and, via the
//! llm-layer tap, the compiler CLI's stdout/stderr) appends to while the
//! operation runs, and the chat view reads each frame. Every line is also
//! persisted to a per-compile log file under the app data dir, so a failed
//! or killed compile still leaves a trace on disk.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Called after every appended line so an immediate-mode UI can repaint.
/// The engine wires in `egui::Context::request_repaint`; headless consumers
/// pass `None`.
pub type ConsoleNotify = Arc<dyn Fn() + Send + Sync>;

/// In-memory scrollback bound. The log file keeps everything; the buffer
/// only feeds the live view, so old lines are dropped from the front.
const MAX_LINES: usize = 2_000;

/// Where per-compile log files live below the app data dir.
const LOG_DIR_NAME: &str = "compile-logs";

/// The conventional log directory for a data dir.
pub fn default_log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR_NAME)
}

/// Which stream a console line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleStream {
    /// The compiler CLI's stdout.
    Stdout,
    /// The compiler CLI's stderr.
    Stderr,
    /// Lifecycle notes written by the engine itself (attempt/retry/outcome).
    Info,
}

impl ConsoleStream {
    /// Fixed-width tag used in the persisted log file.
    fn tag(self) -> &'static str {
        match self {
            Self::Stdout => "out ",
            Self::Stderr => "err ",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    pub stream: ConsoleStream,
    pub text: String,
}

/// Point-in-time copy of a console for rendering. Cheap enough per frame:
/// the buffer is bounded and the view is only open while the user looks.
pub struct ConsoleSnapshot {
    pub lines: Vec<ConsoleLine>,
    /// Lines dropped from the front of the in-memory buffer (still on disk).
    pub dropped: usize,
    /// When the most recent line arrived — the "still alive?" heartbeat.
    pub last_output: Option<std::time::Instant>,
    /// Terminal note once the operation finished (success or failure).
    pub closed: Option<String>,
    pub log_path: Option<PathBuf>,
}

/// Shared, clonable console for one compile/edit operation.
#[derive(Clone)]
pub struct CompileConsole {
    inner: Arc<Inner>,
}

struct Inner {
    log_path: Option<PathBuf>,
    notify: Option<ConsoleNotify>,
    state: Mutex<ConsoleState>,
}

#[derive(Default)]
struct ConsoleState {
    lines: VecDeque<ConsoleLine>,
    dropped: usize,
    last_output: Option<std::time::Instant>,
    closed: Option<String>,
    log_file: Option<std::fs::File>,
}

impl std::fmt::Debug for CompileConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.state.lock().expect("console state poisoned");
        f.debug_struct("CompileConsole")
            .field("lines", &state.lines.len())
            .field("closed", &state.closed)
            .field("log_path", &self.inner.log_path)
            .finish()
    }
}

impl CompileConsole {
    /// Open a console, creating its log file under `log_dir` when given.
    /// Log file creation is best-effort: a read-only disk must never block
    /// the compile itself, so failures degrade to an in-memory-only console.
    pub fn new(label: &str, log_dir: Option<&Path>, notify: Option<ConsoleNotify>) -> Self {
        let (log_file, log_path) = match log_dir {
            Some(dir) => open_log_file(dir, label),
            None => (None, None),
        };
        let console = Self {
            inner: Arc::new(Inner {
                log_path,
                notify,
                state: Mutex::new(ConsoleState {
                    log_file,
                    ..ConsoleState::default()
                }),
            }),
        };
        console.info(format!("── {label} ──"));
        console
    }

    /// Append one chunk of output. Multi-line text is split so the view and
    /// the log stay line-oriented.
    pub fn push(&self, stream: ConsoleStream, text: impl AsRef<str>) {
        let mut state = self.inner.state.lock().expect("console state poisoned");
        for line in text.as_ref().split('\n') {
            let line = line.trim_end_matches('\r');
            if let Some(file) = state.log_file.as_mut() {
                // Best-effort, like log creation: never fail the compile
                // over a log write.
                let _ = writeln!(
                    file,
                    "[{} {}] {line}",
                    chrono::Utc::now().format("%H:%M:%S%.3f"),
                    stream.tag()
                );
            }
            state.lines.push_back(ConsoleLine {
                stream,
                text: line.to_owned(),
            });
            if state.lines.len() > MAX_LINES {
                state.lines.pop_front();
                state.dropped += 1;
            }
        }
        state.last_output = Some(std::time::Instant::now());
        drop(state);
        if let Some(notify) = &self.inner.notify {
            notify();
        }
    }

    /// Append an engine lifecycle note.
    pub fn info(&self, text: impl AsRef<str>) {
        self.push(ConsoleStream::Info, text);
    }

    /// Mark the operation finished. The note is appended, kept as the
    /// terminal status for the post-mortem view, and the log file is
    /// flushed. Idempotent — the first close wins.
    pub fn close(&self, note: impl Into<String>) {
        let note = note.into();
        {
            let state = self.inner.state.lock().expect("console state poisoned");
            if state.closed.is_some() {
                return;
            }
        }
        self.push(ConsoleStream::Info, &note);
        let mut state = self.inner.state.lock().expect("console state poisoned");
        state.closed = Some(note);
        if let Some(file) = state.log_file.as_mut() {
            let _ = file.flush();
        }
    }

    /// Run `persist`, then close with the outcome that actually happened.
    ///
    /// The compiler succeeding is not the end of the operation — the plan
    /// still has to be written. Closing on the compiler's success alone
    /// leaves a "✓ compiled" console attached to an activity row that ends
    /// up `Failed` when the save fails, reporting an outcome that never
    /// happened and hiding the real error. Keeping both branches in one place
    /// makes the ordering structural: a caller cannot report success before
    /// the work is durable without deleting this call.
    pub fn close_after_persisting<T>(
        &self,
        persist: impl FnOnce() -> anyhow::Result<T>,
        failure_note: &str,
        success_note: impl FnOnce() -> String,
    ) -> anyhow::Result<T> {
        match persist() {
            Ok(persisted) => {
                self.close(success_note());
                Ok(persisted)
            }
            Err(error) => {
                self.close(format!("✗ {failure_note}: {error:#}"));
                Err(error)
            }
        }
    }

    pub fn snapshot(&self) -> ConsoleSnapshot {
        let state = self.inner.state.lock().expect("console state poisoned");
        ConsoleSnapshot {
            lines: state.lines.iter().cloned().collect(),
            dropped: state.dropped,
            last_output: state.last_output,
            closed: state.closed.clone(),
            log_path: self.inner.log_path.clone(),
        }
    }
}

/// `{utc timestamp}-{slug}.log` inside `dir`; a short random suffix keeps
/// two consoles opened in the same millisecond apart.
fn open_log_file(dir: &Path, label: &str) -> (Option<std::fs::File>, Option<PathBuf>) {
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!(%error, dir = %dir.display(), "could not create the compile-log directory");
        return (None, None);
    }
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let name = format!(
        "{}-{}-{suffix}.log",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        slug(label)
    );
    let path = dir.join(name);
    match std::fs::File::create(&path) {
        Ok(file) => (Some(file), Some(path)),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not create a compile log file");
            (None, None)
        }
    }
}

/// Raw CLI stdout/stderr lines from the compiler subprocess stream in here
/// via the task-local sink set around `compile_validate_normalize`.
impl crate::llm::CliLineSink for CompileConsole {
    fn cli_line(&self, stream: crate::llm::CliLineStream, text: &str) {
        self.push(
            match stream {
                crate::llm::CliLineStream::Stdout => ConsoleStream::Stdout,
                crate::llm::CliLineStream::Stderr => ConsoleStream::Stderr,
            },
            text,
        );
    }
}

/// Filesystem-safe, bounded slug of a console label.
fn slug(label: &str) -> String {
    let slug: String = label
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .take(24)
        .collect();
    let trimmed = slug.trim_matches('-');
    match trimmed.is_empty() {
        true => "compile".to_owned(),
        false => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_splits_lines_and_caps_the_buffer() {
        let console = CompileConsole::new("compile", None, None);
        console.push(ConsoleStream::Stdout, "one\ntwo\r\nthree");
        let snapshot = console.snapshot();
        // The constructor's header line plus the three pushed lines.
        assert_eq!(snapshot.lines.len(), 4);
        assert_eq!(snapshot.lines[2].text, "two");
        assert!(snapshot.last_output.is_some());

        for i in 0..(MAX_LINES + 10) {
            console.push(ConsoleStream::Stdout, format!("line {i}"));
        }
        let snapshot = console.snapshot();
        assert_eq!(snapshot.lines.len(), MAX_LINES);
        assert!(snapshot.dropped > 0, "overflow must be counted, not silent");
    }

    #[test]
    fn close_is_idempotent_and_keeps_the_first_note() {
        let console = CompileConsole::new("compile", None, None);
        console.close("finished");
        console.close("finished again");
        let snapshot = console.snapshot();
        assert_eq!(snapshot.closed.as_deref(), Some("finished"));
        assert_eq!(
            snapshot
                .lines
                .iter()
                .filter(|line| line.text.contains("finished"))
                .count(),
            1
        );
    }

    /// The console must still be open while the plan is being written: the
    /// terminal note has to describe the outcome of the *whole* operation,
    /// persistence included.
    #[test]
    fn close_after_persisting_reports_success_only_once_the_work_is_durable() {
        let console = CompileConsole::new("compile", None, None);
        let open_during_persist = std::cell::Cell::new(false);
        let result = console.close_after_persisting(
            || {
                open_during_persist.set(console.snapshot().closed.is_none());
                Ok(7)
            },
            "saving the plan failed",
            || "✓ compiled “demo” — 1 step, validated".to_owned(),
        );

        assert_eq!(result.unwrap(), 7);
        assert!(
            open_during_persist.get(),
            "the console must still be open while the plan is persisted,              otherwise the note precedes the outcome it claims"
        );
        assert_eq!(
            console.snapshot().closed.as_deref(),
            Some("✓ compiled “demo” — 1 step, validated")
        );
    }

    /// A failing save must not leave a success note behind. `close` keeps the
    /// first note, so closing before persisting would pin "✓ compiled" onto an
    /// operation that actually failed.
    #[test]
    fn close_after_persisting_reports_the_save_failure_instead_of_success() {
        let console = CompileConsole::new("compile", None, None);
        let result = console.close_after_persisting(
            || -> anyhow::Result<()> { anyhow::bail!("read-only file system") },
            "compiled, but saving the plan failed",
            || "✓ compiled “demo” — 1 step, validated".to_owned(),
        );

        let error = result.expect_err("a failing persist must propagate");
        assert!(error.to_string().contains("read-only file system"));

        let snapshot = console.snapshot();
        let closed = snapshot.closed.expect("a failed persist still closes");
        assert_eq!(
            closed,
            "✗ compiled, but saving the plan failed: read-only file system"
        );
        assert!(
            !snapshot.lines.iter().any(|line| line.text.contains("✓")),
            "no success note may be reported for a failed save: {:?}",
            snapshot.lines
        );
    }

    #[test]
    fn lines_are_persisted_to_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let console = CompileConsole::new("mcp compile", Some(dir.path()), None);
        console.push(ConsoleStream::Stderr, "something went wrong");
        console.close("failed");
        let path = console.snapshot().log_path.expect("log file should exist");
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("[") && content.contains("err ]"));
        assert!(content.contains("something went wrong"));
        assert!(content.contains("failed"));
    }

    #[test]
    fn notify_fires_for_every_append() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let console = CompileConsole::new(
            "compile",
            None,
            Some(Arc::new(move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
        );
        console.push(ConsoleStream::Stdout, "hello");
        assert!(count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }
}
