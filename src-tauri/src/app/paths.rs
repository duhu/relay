//! Where Relay keeps its files.

use std::path::PathBuf;

/// `~/Library/Application Support/Relay`.
pub fn data_dir() -> PathBuf {
    home()
        .join("Library")
        .join("Application Support")
        .join("Relay")
}

/// The configuration file, which is the single source of truth (invariant 7).
/// Same path as `relay_core::config::Config::default_path`.
pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// The unix socket the CLI talks to.
pub fn sock_path() -> PathBuf {
    data_dir().join("relay.sock")
}

/// The file whose `flock` makes sure only one resident app runs.
pub fn lock_path() -> PathBuf {
    data_dir().join("relay.lock")
}

/// `~/Library/Logs/Relay`.
pub fn log_dir() -> PathBuf {
    home().join("Library").join("Logs").join("Relay")
}

fn home() -> PathBuf {
    // Every macOS user process gets HOME, launchd agents included.
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is unset")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_path_is_the_one_the_core_reads() {
        assert_eq!(
            config_path(),
            relay_core::config::Config::default_path().expect("HOME is set")
        );
    }

    #[test]
    fn everything_relay_owns_lives_in_one_directory() {
        let dir = data_dir();
        for path in [config_path(), sock_path(), lock_path()] {
            assert_eq!(path.parent(), Some(dir.as_path()));
        }
    }
}
