//! Noticing that `config.json` changed on disk.
//!
//! The watch is on the *directory*, not the file: [`Config::save_atomic`]
//! writes a sibling `.tmp` and renames it over the config, and a watch on the
//! file itself would follow the replaced inode. Editors do the same thing.
//!
//! [`Config::save_atomic`]: crate::config::Config::save_atomic

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

/// One save can produce several events (write, rename, chmod); nothing is
/// reported until the directory has been quiet for this long.
const QUIET: Duration = Duration::from_millis(300);

/// Watches `config_path`, sending `()` on every settled change.
///
/// The returned watcher must be kept alive: dropping it stops the watch.
pub fn watch(
    config_path: PathBuf,
    tx: UnboundedSender<()>,
) -> Result<RecommendedWatcher, notify::Error> {
    let dir = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let names = FileNames::of(&config_path);

    let (raw_tx, raw_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        // The receiving thread is gone only when the core has shut down.
        let _ = raw_tx.send(event);
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    std::thread::Builder::new()
        .name("relay-config-watch".to_string())
        .spawn(move || coalesce(&raw_rx, &names, &tx))
        .map_err(notify::Error::io)?;

    Ok(watcher)
}

/// The two file names a save touches.
struct FileNames {
    config: std::ffi::OsString,
    tmp: std::ffi::OsString,
}

impl FileNames {
    fn of(config_path: &Path) -> Self {
        let config = config_path
            .file_name()
            .unwrap_or(config_path.as_os_str())
            .to_os_string();
        let mut tmp = config.clone();
        tmp.push(".tmp");
        Self { config, tmp }
    }

    fn matches(&self, event: &notify::Event) -> bool {
        event.paths.iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == self.config || name == self.tmp)
        })
    }
}

/// Collapses a burst of raw events into one `()`, `QUIET` after the last one.
fn coalesce(
    raw_rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    names: &FileNames,
    tx: &UnboundedSender<()>,
) {
    loop {
        // Wait for something that concerns our file.
        match raw_rx.recv() {
            // The watcher was dropped: the core is shutting down.
            Err(_) => return,
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "config watch error");
                continue;
            }
            Ok(Ok(event)) if !names.matches(&event) => continue,
            Ok(Ok(_)) => {}
        }

        // Then let the burst finish before reporting it once.
        loop {
            match raw_rx.recv_timeout(QUIET) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if tx.send(()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::time::Instant;

    use tokio::sync::mpsc::unbounded_channel;

    /// Real files and a real watcher, so this test uses real time.
    #[tokio::test]
    async fn a_burst_of_writes_reports_one_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.json");
        fs::write(&config, "{}").expect("write");

        let (tx, mut rx) = unbounded_channel();
        let _watcher = watch(config.clone(), tx).expect("watcher");

        fs::write(&config, "{\"a\":1}").expect("write");
        fs::write(&config, "{\"a\":2}").expect("write");

        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a change within two seconds")
            .expect("the sender is alive");
        assert!(
            started.elapsed() >= QUIET,
            "the burst must settle before it is reported"
        );

        // The second write must not produce a second notification.
        assert!(
            tokio::time::timeout(Duration::from_millis(700), rx.recv())
                .await
                .is_err(),
            "a burst of writes is one change"
        );
    }

    #[tokio::test]
    async fn an_atomic_save_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.json");
        let tmp = dir.path().join("config.json.tmp");

        let (tx, mut rx) = unbounded_channel();
        let _watcher = watch(config.clone(), tx).expect("watcher");

        fs::write(&tmp, "{\"a\":1}").expect("write");
        fs::rename(&tmp, &config).expect("rename");

        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a change within two seconds")
            .expect("the sender is alive");
    }

    #[tokio::test]
    async fn another_file_in_the_directory_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.json");
        fs::write(&config, "{}").expect("write");

        let (tx, mut rx) = unbounded_channel();
        let _watcher = watch(config, tx).expect("watcher");

        fs::write(dir.path().join("notes.txt"), "hello").expect("write");

        assert!(
            tokio::time::timeout(Duration::from_millis(900), rx.recv())
                .await
                .is_err(),
            "only the config file matters"
        );
    }
}
