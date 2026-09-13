//! Single-instance guard (docs/resident-ingest.md §1).
//!
//! Two writers on one WAL database is the failure that corrupts data quietly,
//! so the daemon takes an exclusive lock *before* it opens the store.
//!
//! The mechanism is an atomically created lock file: `create_new(true)`, which
//! is `O_EXCL` on Unix and `CREATE_NEW` on Windows, with the same "fail if it
//! already exists" semantics on both. Deliberately **not** a file lock --
//! `flock` and `LockFileEx` disagree about advisory vs mandatory locking and
//! about what happens across processes, and §1 rules that out.
//!
//! The file carries the holder's pid and start time. It does not survive a
//! hard kill (nothing runs `Drop`), so a stale file is possible; the pid is
//! there for a human to read, and the "previous run died without shutdown"
//! check in §8 reads the same file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "another item-ingest is already running: {holder} (lock file {path}); \
         stop it, or delete the file if that process is really gone"
    )]
    Held { path: PathBuf, holder: String },
    #[error("could not create the lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Holds the lock for as long as it is alive; `Drop` releases it, so a clean
/// exit never leaves a file the operator has to delete by hand.
#[derive(Debug)]
pub struct SingleInstance {
    path: PathBuf,
}

impl SingleInstance {
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| LockError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                let _ = writeln!(file, "started_at={}", Utc::now().to_rfc3339());
                Ok(Self {
                    path: path.to_path_buf(),
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(path).unwrap_or_default();
                let holder = holder.split_whitespace().collect::<Vec<_>>().join(" ");
                Err(LockError::Held {
                    path: path.to_path_buf(),
                    holder: if holder.is_empty() {
                        "(no pid recorded)".to_string()
                    } else {
                        holder
                    },
                })
            }
            Err(source) => Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_lock() -> PathBuf {
        std::env::temp_dir().join(format!(
            "item-ingest-lock-{}-{}.lock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_second_acquire_names_the_holder_and_leaves_the_file_alone() {
        let path = temp_lock();
        let first = SingleInstance::acquire(&path).expect("first acquire succeeds");

        let err = SingleInstance::acquire(&path).expect_err("second acquire must be refused");
        let message = err.to_string();
        assert!(
            message.contains(&std::process::id().to_string()),
            "the loser must be told which pid holds it: {message}"
        );
        assert!(message.contains(&path.display().to_string()));

        // The loser must not have truncated or removed the winner's file.
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(&format!("pid={}", std::process::id())));
        assert!(body.contains("started_at="));

        drop(first);
        assert!(!path.exists(), "a clean exit releases the lock");
        let _ = SingleInstance::acquire(&path).expect("the lock is reusable after release");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_lock_file_creates_its_own_directory() {
        let dir = std::env::temp_dir().join(format!("item-ingest-lockdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("ingest.lock");

        let guard = SingleInstance::acquire(&path).expect("creates parent directories");
        assert!(path.exists());
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
