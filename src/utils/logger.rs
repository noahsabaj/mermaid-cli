use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, error, info, warn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{
    EnvFilter, Layer,
    layer::{Context as LayerContext, SubscriberExt},
    util::SubscriberInitExt,
};

/// Rotate the log file when it reaches this size. Bounded: at most two
/// log files (`mermaid.log` current + `mermaid.log.old` previous), so
/// worst-case disk use is ~2x this value between restarts.
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

/// Events retained by the in-memory TRACE ring (`mermaid feedback`).
const RING_CAPACITY: usize = 2000;
/// Per-event byte clamp so one giant payload can't hog the ring
/// (~1 MiB worst-case total with [`RING_CAPACITY`]).
const RING_MAX_EVENT_BYTES: usize = 512;

/// Get the log file path (~/.mermaid/mermaid.log)
fn get_log_file_path() -> Option<PathBuf> {
    // Fall back to USERPROFILE on Windows where HOME is not conventionally
    // set; mirrors the pattern used in app::config::get_config_dir.
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(|home| PathBuf::from(home).join(".mermaid").join("mermaid.log"))
}

/// The log file path, if resolvable — `mermaid feedback` tails it.
pub fn log_file_path() -> Option<PathBuf> {
    get_log_file_path()
}

/// Always-on in-memory ring of recent trace events. Captures at TRACE for the
/// mermaid crates regardless of `RUST_LOG` (deps capped at INFO), so a bug
/// report carries the last ~2000 events without asking the user to reproduce
/// under elevated logging. Secrets are redacted AT CAPTURE, not at read time.
#[derive(Clone)]
pub struct TraceRing {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl TraceRing {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
        }
    }

    /// Append one formatted event, evicting the oldest past capacity.
    fn push(&self, line: String) {
        let Ok(mut ring) = self.inner.lock() else {
            return;
        };
        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    /// Copy of the ring contents, oldest first.
    pub fn snapshot(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|ring| ring.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Process-global ring, installed by [`init_logger`]. `None` before init
/// (unit tests, library embedding).
static TRACE_RING: OnceLock<TraceRing> = OnceLock::new();

/// The process-global trace ring, if logging was initialized.
pub fn trace_ring() -> Option<&'static TraceRing> {
    TRACE_RING.get()
}

/// `tracing` layer that mirrors every (filter-passing) event into a
/// [`TraceRing`] as one compact line: `ts LEVEL target: message k=v`.
struct RingLayer {
    ring: TraceRing,
}

/// Field visitor for [`RingLayer`]: the `message` field becomes the line body,
/// every other field is appended as ` k=v`.
struct RingVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for RingVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    // NOTE: never log from inside on_event — a tracing call here would
    // re-enter the subscriber.
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let mut visitor = RingVisitor {
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut visitor);
        let meta = event.metadata();
        let mut line = format!(
            "{} {} {}: {}{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
            meta.level(),
            meta.target(),
            visitor.message,
            visitor.fields
        );
        if line.len() > RING_MAX_EVENT_BYTES {
            line.truncate(line.floor_char_boundary(RING_MAX_EVENT_BYTES));
            line.push_str("...");
        }
        // Redact ON CAPTURE: the ring is read back by `mermaid feedback`, so a
        // key must never sit in memory waiting to be exported.
        self.ring.push(crate::utils::redact_secrets(&line));
    }
}

/// The ring's fixed filter: our crates at TRACE, dependencies capped at INFO
/// (hyper/h2 TRACE floods stay disabled via per-callsite interest caching).
/// Independent of `RUST_LOG`, which scopes only the file layer.
fn ring_filter() -> EnvFilter {
    EnvFilter::new("info,mermaid_cli=trace,mermaid_runtime=trace,mermaidd=trace")
}

/// If the log file exceeds MAX_LOG_SIZE, rename it to `.log.old`
/// (overwriting any prior `.log.old`). Best-effort — rotation failures
/// are silent because logging is non-critical. Runs once per startup.
fn rotate_if_large(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() >= MAX_LOG_SIZE {
        let rotated = path.with_extension("log.old");
        let _ = std::fs::rename(path, rotated);
    }
}

/// Initialize the logging system with tracing.
///
/// Two layers, each with its OWN filter:
/// - the file layer (`~/.mermaid/mermaid.log`), scoped by `RUST_LOG` /
///   `--verbose` exactly as before;
/// - the always-on [`TraceRing`] with a fixed mermaid-at-TRACE filter, so
///   `mermaid feedback` can export recent events without a reproduce-under-
///   RUST_LOG round trip.
pub fn init_logger(verbose: bool) {
    // If --verbose flag is set, override to debug level
    // Otherwise use RUST_LOG environment variable, default to warn level
    // (quieter). This filter scopes ONLY the file layer.
    let filter = if verbose {
        EnvFilter::new("debug,mermaid=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,mermaid=info"))
    };
    let ring_layer = RingLayer {
        ring: TRACE_RING.get_or_init(TraceRing::new).clone(),
    }
    .with_filter(ring_filter());

    // Try to write logs to a file to avoid corrupting the TUI
    // Falls back to no logging if file creation fails (TUI takes priority)
    if let Some(log_path) = get_log_file_path() {
        // Ensure parent directory exists
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Rotate at startup if the previous session left a large file.
        rotate_if_large(&log_path);

        // Open the log for appending, owner-only (0o600): it can incidentally
        // capture secrets the model surfaced (a `read_file` of `.env`, an API
        // error echoing a key), so a shared temp/cwd must not expose it. Mirrors
        // recorder.rs; RedactingWriter additionally scrubs credential shapes.
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(file) = opts.open(&log_path) {
            // `mode` only applies on create; tighten an existing log too.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600));
            }
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(RedactingWriter::new(file))
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_ansi(false) // No ANSI colors in file
                .compact()
                .with_filter(filter);

            tracing_subscriber::registry()
                .with(fmt_layer)
                .with(ring_layer)
                .init();
            return;
        }
    }

    // Fallback: no file logging if creation fails (don't corrupt the TUI) —
    // but the trace ring still captures, so `mermaid feedback` keeps working.
    tracing_subscriber::registry().with(ring_layer).init();
}

/// A `MakeWriter` that scrubs credential-shaped strings out of every formatted
/// log event before it reaches disk. The log can incidentally capture secrets
/// the model surfaced (a `read_file` of `.env`, an API error echoing a key);
/// [`redact_secrets`](crate::utils::redact_secrets) removes the common shapes at
/// this single sink so no `tracing::warn!`/`error!` payload persists a key.
#[derive(Clone)]
struct RedactingWriter {
    file: Arc<Mutex<File>>,
}

impl RedactingWriter {
    /// Wrap an open log file; the handle is shared across concurrently-logging threads.
    fn new(file: File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

impl<'a> MakeWriter<'a> for RedactingWriter {
    type Writer = RedactingEvent;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingEvent {
            buf: Vec::new(),
            file: Arc::clone(&self.file),
        }
    }
}

/// One event's write buffer. The fmt layer formats a whole event and writes it
/// here; we accumulate the bytes and redact the *complete* text on drop (so a
/// secret split across writes can't slip through unredacted), then append the
/// scrubbed line to the shared file.
struct RedactingEvent {
    buf: Vec<u8>,
    file: Arc<Mutex<File>>,
}

impl Write for RedactingEvent {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RedactingEvent {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf);
        let redacted = crate::utils::redact_secrets(&text);
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(redacted.as_bytes());
        }
    }
}

/// Log an info message with category prefix (backward compatible)
pub fn log_info(category: &str, message: impl std::fmt::Display) {
    info!(category = %category, "{}", message);
}

/// Log a warning message with category prefix (backward compatible)
pub fn log_warn(category: &str, message: impl std::fmt::Display) {
    warn!(category = %category, "{}", message);
}

/// Log an error message with category prefix (backward compatible)
pub fn log_error(category: &str, message: impl std::fmt::Display) {
    error!(category = %category, "{}", message);
}

/// Log a debug message (backward compatible)
pub fn log_debug(message: impl std::fmt::Display) {
    debug!("{}", message);
}

/// Progress indicator for startup sequence
pub fn log_progress(step: usize, total: usize, message: impl std::fmt::Display) {
    info!(step = step, total = total, "{}", message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_small_file_is_noop() {
        let tmp = std::env::temp_dir().join("mermaid_logger_small.log");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("log.old"));
        std::fs::write(&tmp, b"hello world").unwrap();

        rotate_if_large(&tmp);

        assert!(tmp.exists(), "small file should NOT be rotated");
        assert!(
            !tmp.with_extension("log.old").exists(),
            "no .log.old should be created for small files"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rotate_large_file_renames_to_old() {
        let tmp = std::env::temp_dir().join("mermaid_logger_large.log");
        let _ = std::fs::remove_file(&tmp);
        let old = tmp.with_extension("log.old");
        let _ = std::fs::remove_file(&old);

        let file = std::fs::File::create(&tmp).unwrap();
        file.set_len(MAX_LOG_SIZE + 1).unwrap();
        drop(file);

        rotate_if_large(&tmp);

        assert!(!tmp.exists(), "oversized file should be rotated away");
        assert!(old.exists(), ".log.old should now exist");

        let _ = std::fs::remove_file(&old);
    }

    #[test]
    fn rotate_overwrites_prior_old() {
        let tmp = std::env::temp_dir().join("mermaid_logger_overwrite.log");
        let _ = std::fs::remove_file(&tmp);
        let old = tmp.with_extension("log.old");
        std::fs::write(&old, b"stale previous rotation").unwrap();

        let file = std::fs::File::create(&tmp).unwrap();
        file.set_len(MAX_LOG_SIZE + 1).unwrap();
        drop(file);

        rotate_if_large(&tmp);

        // Previous .old should have been replaced by the freshly rotated file.
        let rotated_size = std::fs::metadata(&old).unwrap().len();
        assert!(
            rotated_size >= MAX_LOG_SIZE,
            "the rotated file should be the large one, not the stale old"
        );

        let _ = std::fs::remove_file(&old);
    }

    /// Local (non-global) subscriber for ring tests — never touches the
    /// `TRACE_RING` OnceLock, so tests can't interfere with each other.
    fn with_ring_subscriber(ring: TraceRing, f: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry()
            .with(RingLayer { ring }.with_filter(ring_filter()));
        tracing::subscriber::with_default(subscriber, f);
    }

    #[test]
    fn ring_captures_trace_events_from_mermaid_targets() {
        let ring = TraceRing::new();
        with_ring_subscriber(ring.clone(), || {
            tracing::trace!(target: "mermaid_cli::probe", step = 3, "ring probe fired");
        });
        let lines = ring.snapshot();
        assert_eq!(lines.len(), 1, "TRACE from our crates must be captured");
        assert!(lines[0].contains("TRACE"));
        assert!(lines[0].contains("mermaid_cli::probe"));
        assert!(lines[0].contains("ring probe fired"));
        assert!(lines[0].contains("step=3"));
    }

    #[test]
    fn ring_caps_dependencies_at_info() {
        let ring = TraceRing::new();
        with_ring_subscriber(ring.clone(), || {
            tracing::trace!(target: "hyper::client", "dep noise");
            tracing::info!(target: "hyper::client", "dep signal");
        });
        let lines = ring.snapshot();
        assert_eq!(lines.len(), 1, "dep TRACE dropped, dep INFO kept");
        assert!(lines[0].contains("dep signal"));
    }

    #[test]
    fn ring_evicts_oldest_past_capacity() {
        let ring = TraceRing::new();
        for i in 0..(RING_CAPACITY + 10) {
            ring.push(format!("event {i}"));
        }
        let lines = ring.snapshot();
        assert_eq!(lines.len(), RING_CAPACITY);
        assert_eq!(lines[0], "event 10", "oldest evicted first");
        assert_eq!(lines[RING_CAPACITY - 1], format!("event {}", RING_CAPACITY + 9));
    }

    #[test]
    fn ring_redacts_secrets_on_capture() {
        let ring = TraceRing::new();
        with_ring_subscriber(ring.clone(), || {
            tracing::warn!(target: "mermaid_cli::auth", "key OPENAI_API_KEY=sk-abcdefghijklmnop1234 seen");
        });
        let lines = ring.snapshot();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[REDACTED]"), "got: {}", lines[0]);
        assert!(!lines[0].contains("sk-abcdefghijklmnop1234"));
    }

    #[test]
    fn ring_truncates_oversized_events() {
        let ring = TraceRing::new();
        let huge = "x".repeat(4 * RING_MAX_EVENT_BYTES);
        with_ring_subscriber(ring.clone(), || {
            tracing::info!(target: "mermaid_cli::big", "{huge}");
        });
        let lines = ring.snapshot();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].len() <= RING_MAX_EVENT_BYTES + 8,
            "event must be clamped, got {} bytes",
            lines[0].len()
        );
        assert!(lines[0].ends_with("..."));
    }

    #[test]
    fn log_writer_redacts_secrets_per_event() {
        let tmp =
            std::env::temp_dir().join(format!("mermaid_log_redact_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let file = std::fs::File::create(&tmp).unwrap();
        let mw = RedactingWriter::new(file);
        {
            let mut w = mw.make_writer();
            writeln!(w, "startup OPENAI_API_KEY=sk-abcdefghijklmnop1234 ready").unwrap();
        } // drop flushes + redacts the complete event
        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert!(
            contents.contains("[REDACTED]"),
            "secret must be redacted in the log: {contents}"
        );
        assert!(
            !contents.contains("sk-abcdefghijklmnop1234"),
            "raw key must not reach disk: {contents}"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
