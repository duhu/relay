//! Tracing setup: a daily file in `~/Library/Logs/Relay` plus a ring buffer
//! the settings window's Log view reads (spec §5).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Keeps the non-blocking file writer's worker alive for the whole process;
/// dropping the guard would stop flushing to `relay.log`.
static APPENDER_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// One `tracing` event, flattened for the UI.
#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    /// Unix milliseconds.
    pub ts_ms: u64,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// The last `cap` events, oldest first. Cheap to clone; every clone reads the
/// same buffer.
#[derive(Clone)]
pub struct LogBuffer {
    entries: Arc<Mutex<VecDeque<LogEntry>>>,
    cap: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        // A capacity of 0 would make `push`'s "drop until under cap" loop spin
        // forever popping from an already-empty deque, so the buffer always
        // holds at least one entry.
        let cap = cap.max(1);
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(cap.min(1024)))),
            cap,
        }
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn push(&self, entry: LogEntry) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while entries.len() >= self.cap {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// The layer that feeds this buffer.
    fn layer(&self) -> BufferLayer {
        BufferLayer {
            buffer: self.clone(),
        }
    }
}

struct BufferLayer {
    buffer: LogBuffer,
}

impl<S: tracing::Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut message = MessageVisitor::default();
        event.record(&mut message);

        self.buffer.push(LogEntry {
            ts_ms: now_ms(),
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            message: message.0,
        });
    }
}

/// Renders an event as its `message` followed by its other fields.
#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.write(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.write(field, &format!("{value:?}"));
    }
}

impl MessageVisitor {
    fn write(&mut self, field: &Field, value: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, "{}={value}", field.name());
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// Installs the process-wide subscriber and returns the buffer it feeds.
///
/// A process can only install one subscriber, so a second call leaves the
/// existing one in place (and its buffer keeps collecting); the runtime calls
/// this once at startup.
///
/// `log_dir` may be missing, unwritable, or otherwise unusable (an unwritable
/// parent, a path component that is actually a file, ...): this must not
/// panic the caller, so file logging is skipped in that case. The buffer
/// layer (and, in debug builds, stderr) still receive events, and a warning
/// names the failure once the subscriber is up.
pub fn init(log_dir: &Path, cap: usize) -> LogBuffer {
    let buffer = LogBuffer::new(cap);

    let file_layer_setup = std::fs::create_dir_all(log_dir)
        .map_err(|err| err.to_string())
        .and_then(|()| {
            tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("relay.log")
                .build(log_dir)
                .map_err(|err| err.to_string())
        });

    let (file_layer, log_dir_error) = match file_layer_setup {
        Ok(appender) => {
            let (file_writer, guard) = tracing_appender::non_blocking(appender);
            let _ = APPENDER_GUARD.set(guard);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer);
            (Some(layer), None)
        }
        Err(err) => (None, Some(err)),
    };

    let filter = EnvFilter::try_from_env("RELAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(buffer.layer());

    #[cfg(debug_assertions)]
    let subscriber = subscriber.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

    let _ = subscriber.try_init();

    if let Some(error) = log_dir_error {
        tracing::warn!(
            log_dir = %log_dir.display(),
            error = %error,
            "log directory unusable, file logging disabled"
        );
    }

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_every_event_it_sees() {
        let buffer = LogBuffer::new(200);
        let subscriber = tracing_subscriber::registry().with(buffer.layer());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            tracing::warn!("second");
            tracing::error!("third");
        });

        let entries = buffer.snapshot();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message, "first");
        assert_eq!(entries[1].level, "WARN");
        assert_eq!(entries[2].message, "third");
        assert!(entries[0].target.starts_with("relay_core"));
        assert!(entries[0].ts_ms > 0, "timestamps are filled in");
    }

    #[test]
    fn past_the_cap_the_oldest_event_falls_out() {
        let buffer = LogBuffer::new(2);
        let subscriber = tracing_subscriber::registry().with(buffer.layer());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            tracing::info!("second");
            tracing::info!("third");
        });

        let messages: Vec<String> = buffer
            .snapshot()
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert_eq!(messages, vec!["second".to_string(), "third".to_string()]);
    }

    #[test]
    fn cap_zero_does_not_hang_and_holds_at_most_one_entry() {
        let buffer = LogBuffer::new(0);

        buffer.push(LogEntry {
            ts_ms: 1,
            level: "INFO".to_string(),
            target: "test".to_string(),
            message: "hello".to_string(),
        });

        let entries = buffer.snapshot();
        assert!(entries.len() <= 1, "got {entries:?}");
    }

    #[test]
    fn event_fields_follow_the_message() {
        let buffer = LogBuffer::new(200);
        let subscriber = tracing_subscriber::registry().with(buffer.layer());

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(device = "MX Master 3", "switching");
        });

        let message = buffer.snapshot().remove(0).message;
        assert!(message.contains("switching"), "got {message:?}");
        assert!(message.contains("device=MX Master 3"), "got {message:?}");
    }
}
