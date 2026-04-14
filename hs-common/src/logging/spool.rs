use std::fs::{File, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const CURRENT_FILE: &str = "current.jsonl";

struct SpoolState {
    dir: PathBuf,
    file: File,
    bytes_written: u64,
    opened_at: Instant,
}

/// Shared handle to the spool's current file + metadata. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Spool {
    state: Arc<Mutex<SpoolState>>,
}

impl Spool {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let file_path = dir.join(CURRENT_FILE);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            state: Arc::new(Mutex::new(SpoolState {
                dir,
                file,
                bytes_written,
                opened_at: Instant::now(),
            })),
        })
    }

    pub fn dir(&self) -> PathBuf {
        self.state.lock().unwrap().dir.clone()
    }

    pub fn bytes_written(&self) -> u64 {
        self.state.lock().unwrap().bytes_written
    }

    pub fn age(&self) -> Duration {
        self.state.lock().unwrap().opened_at.elapsed()
    }

    /// Atomically close the current file by renaming it to `<ms>-<uuid>.jsonl`
    /// and opening a fresh `current.jsonl`. Returns the renamed path, or
    /// `None` if the current file was empty.
    pub fn rotate_now(&self) -> io::Result<Option<PathBuf>> {
        let mut s = self.state.lock().unwrap();
        if s.bytes_written == 0 {
            return Ok(None);
        }
        s.file.flush()?;

        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let uuid = uuid::Uuid::new_v4();
        let new_name = format!("{ms}-{uuid}.jsonl");
        let current_path = s.dir.join(CURRENT_FILE);
        let new_path = s.dir.join(&new_name);

        // Rename while the old fd is still open — Linux/macOS keep the inode
        // alive via the open fd, so no data is lost. We then drop the old fd
        // by replacing `s.file` below.
        std::fs::rename(&current_path, &new_path)?;

        let new_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_path)?;
        s.file = new_file;
        s.bytes_written = 0;
        s.opened_at = Instant::now();
        Ok(Some(new_path))
    }
}

/// `io::Write` adapter passed to `tracing_appender::non_blocking`. Forwards
/// every write to the spool's current file under a mutex.
pub(crate) struct SpoolWriter {
    spool: Spool,
}

impl SpoolWriter {
    pub fn new(spool: Spool) -> Self {
        Self { spool }
    }
}

impl IoWrite for SpoolWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.spool.state.lock().unwrap();
        let n = s.file.write(buf)?;
        s.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.spool.state.lock().unwrap();
        s.file.flush()
    }
}

/// Periodic rotate task. Ticks every `max_age / 4` (clamped to 1–5s).
/// Rotates when bytes_written >= max_bytes or age >= max_age.
pub(crate) async fn run_rotate_controller(
    spool: Spool,
    max_bytes: u64,
    max_age: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let tick = (max_age / 4).clamp(Duration::from_secs(1), Duration::from_secs(5));
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if spool.bytes_written() >= max_bytes || spool.age() >= max_age {
                    if let Err(e) = spool.rotate_now() {
                        tracing::warn!(error = %e, "spool rotate failed");
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

/// List closed spool files (all `*.jsonl` except `current.jsonl`), sorted by
/// filename (which is `<unix-ms>-<uuid>.jsonl`, so chronological).
pub(crate) async fn list_closed(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy() != CURRENT_FILE)
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_renames_and_opens_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path().to_path_buf()).unwrap();

        let mut writer = SpoolWriter::new(spool.clone());
        writer.write_all(b"hello\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(spool.bytes_written(), 6);

        let rotated = spool.rotate_now().unwrap().unwrap();
        assert!(rotated.exists());
        assert_eq!(std::fs::read(&rotated).unwrap(), b"hello\n");
        assert_eq!(spool.bytes_written(), 0);

        writer.write_all(b"world\n").unwrap();
        writer.flush().unwrap();
        let current = tmp.path().join(CURRENT_FILE);
        assert_eq!(std::fs::read(&current).unwrap(), b"world\n");
    }

    #[test]
    fn rotate_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path().to_path_buf()).unwrap();
        assert!(spool.rotate_now().unwrap().is_none());
    }

    #[tokio::test]
    async fn list_closed_excludes_current() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path().to_path_buf()).unwrap();
        let mut writer = SpoolWriter::new(spool.clone());
        writer.write_all(b"x\n").unwrap();
        writer.flush().unwrap();
        spool.rotate_now().unwrap();

        writer.write_all(b"y\n").unwrap();
        writer.flush().unwrap();

        let closed = list_closed(tmp.path()).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert!(!closed[0].to_string_lossy().ends_with(CURRENT_FILE));
    }
}
