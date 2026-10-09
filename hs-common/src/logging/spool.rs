use std::fs::{File, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::config::SpoolCaps;

/// Every process owns one `current-<uuid>.jsonl` in the spool directory and
/// holds an exclusive lock on it for as long as it writes to it. The spool
/// directory is per service name, and every `hs` invocation (the `hs serve`
/// units and each CLI command) is service `hs`: with one shared
/// `current.jsonl`, whichever process rotated first renamed the file out
/// from under the others, whose later lines went to a file the shipper had
/// already uploaded and deleted. The lock is also how a file whose writer
/// crashed is told from one that is merely quiet: see [`adopt_orphans`].
pub(crate) const CURRENT_PREFIX: &str = "current";

/// Whether `name` is a live-writer file (never shipped, never capped).
fn is_current_name(name: &str) -> bool {
    name.starts_with(CURRENT_PREFIX) && name.ends_with(".jsonl")
}

/// `<unix-ms>-<uuid>.jsonl`: the name a file gets when it is closed.
fn closed_name() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{ms}-{}.jsonl", uuid::Uuid::new_v4())
}

/// Create this process's `current-<uuid>.jsonl`, locked before it is visible
/// under that name: it is built as a dot-prefixed temp file, locked, then
/// renamed (a lock follows the file), so [`adopt_orphans`] in another
/// process can never see it unlocked and mistake it for an orphan.
fn open_current(dir: &Path) -> io::Result<(File, PathBuf)> {
    let id = uuid::Uuid::new_v4();
    let pending = dir.join(format!(".pending-{id}.tmp"));
    let path = dir.join(format!("{CURRENT_PREFIX}-{id}.jsonl"));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .append(true)
        .open(&pending)?;
    let locked = file.lock().and_then(|()| std::fs::rename(&pending, &path));
    if let Err(e) = locked {
        let _ = std::fs::remove_file(&pending);
        return Err(e);
    }
    Ok((file, path))
}

/// Close every `current*.jsonl` in `dir` (other than `own`) that no process
/// holds the lock of: its writer is gone (crash, `kill -9`), and without
/// this its last lines would never be shipped. A non-empty one is renamed to
/// a closed name for the shipper; an empty one is removed. A file locked by
/// a live process, or owned by another user, is left alone.
fn adopt_orphans(dir: &Path, own: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_current = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_current_name);
        if !is_current || path == own {
            continue;
        }
        let file = match OpenOptions::new().read(true).append(true).open(&path) {
            Ok(file) => file,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        let closed = if file.metadata()?.len() == 0 {
            std::fs::remove_file(&path)
        } else {
            std::fs::rename(&path, dir.join(closed_name()))
        };
        match closed {
            Ok(()) => {}
            // Another process adopted it first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

struct SpoolState {
    dir: PathBuf,
    /// This process's current file; the lock lives as long as this handle.
    file: File,
    path: PathBuf,
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
        let (file, path) = open_current(&dir)?;
        adopt_orphans(&dir, &path)?;
        Ok(Self {
            state: Arc::new(Mutex::new(SpoolState {
                dir,
                file,
                path,
                bytes_written: 0,
                opened_at: Instant::now(),
            })),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SpoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Close the files of writers that no longer exist (see
    /// [`adopt_orphans`]).
    pub fn adopt_orphans(&self) -> io::Result<()> {
        let (dir, own) = {
            let s = self.state();
            (s.dir.clone(), s.path.clone())
        };
        adopt_orphans(&dir, &own)
    }

    #[cfg(test)]
    pub fn current_path(&self) -> PathBuf {
        self.state().path.clone()
    }

    /// The current file's bytes, read through the lock-holding handle: on
    /// Windows the exclusive lock is mandatory, so any other handle is
    /// refused (os error 33). Appends always land at the end, so moving the
    /// cursor here cannot misplace a later write.
    #[cfg(test)]
    pub fn current_contents(&self) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let s = self.state();
        let mut file = &s.file;
        file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn dir(&self) -> PathBuf {
        self.state().dir.clone()
    }

    pub fn bytes_written(&self) -> u64 {
        self.state().bytes_written
    }

    pub fn age(&self) -> Duration {
        self.state().opened_at.elapsed()
    }

    /// Close the current file by renaming it to `<ms>-<uuid>.jsonl` and
    /// starting a fresh locked one. Returns the renamed path, or `None` if
    /// the current file was empty.
    pub fn rotate_now(&self) -> io::Result<Option<PathBuf>> {
        let mut s = self.state();
        if s.bytes_written == 0 {
            return Ok(None);
        }
        s.file.flush()?;

        // The new file exists (and is locked) before the old one is
        // renamed away, so a lock-holding current file is always present.
        let (new_file, new_path) = open_current(&s.dir)?;
        let closed_path = s.dir.join(closed_name());
        // Rename while the old fd is still open — the inode lives on via the
        // fd, so no data is lost. Replacing `s.file` below drops the old fd.
        if let Err(e) = std::fs::rename(&s.path, &closed_path) {
            let _ = std::fs::remove_file(&new_path);
            return Err(e);
        }
        s.file = new_file;
        s.path = new_path;
        s.bytes_written = 0;
        s.opened_at = Instant::now();
        Ok(Some(closed_path))
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
        let mut s = self.spool.state();
        let n = s.file.write(buf)?;
        s.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.spool.state();
        s.file.flush()
    }
}

/// Periodic rotate task. Ticks every `max_age / 4` (clamped to 1–5s).
/// Rotates when bytes_written >= max_bytes or age >= max_age.
pub(crate) async fn run_rotate_controller(
    spool: Spool,
    max_bytes: u64,
    max_age: Duration,
    caps: SpoolCaps,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let tick = (max_age / 4).clamp(Duration::from_secs(1), Duration::from_secs(5));
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let dir = spool.dir();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if spool.bytes_written() >= max_bytes || spool.age() >= max_age {
                    if let Err(e) = spool.rotate_now() {
                        tracing::warn!(error = %e, "spool rotate failed");
                    }
                }
                if let Err(e) = spool.adopt_orphans() {
                    tracing::warn!(error = %e, "closing the spool files of dead writers failed");
                }
                // Bounded whether or not a shipper is draining the spool.
                if let Err(e) = enforce_caps(&dir, caps).await {
                    tracing::warn!(error = %e, "spool cap enforcement failed");
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

/// List closed spool files (all `*.jsonl` except the live `current*.jsonl`
/// files), sorted by
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
                .is_some_and(|n| !is_current_name(&n.to_string_lossy()))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// When a closed spool file was rotated, from the `<unix-ms>-<uuid>.jsonl`
/// name `Spool::rotate_now` gave it. `fallback` supplies the file's mtime
/// for a name that doesn't follow the pattern.
pub(crate) fn closed_at(
    path: &Path,
    fallback: impl FnOnce() -> Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let ms: i64 = path
        .file_name()?
        .to_str()?
        .split_once('-')?
        .0
        .parse()
        .ok()?;
    chrono::DateTime::from_timestamp_millis(ms).or_else(fallback)
}

/// What [`enforce_caps`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Dropped {
    pub files: usize,
    pub bytes: u64,
}

/// Remove closed spool files that exceed `caps`: anything older than
/// `caps.max_age`, then the oldest of the rest until the total fits in
/// `caps.max_bytes`. Losing the oldest logs beats filling the disk; the
/// loss is logged once per call that drops anything. `current*.jsonl` is
/// never touched.
pub(crate) async fn enforce_caps(dir: &Path, caps: SpoolCaps) -> io::Result<Dropped> {
    let files = list_closed(dir).await?;
    let mut kept: Vec<(PathBuf, u64)> = Vec::with_capacity(files.len());
    let mut dropped = Dropped::default();
    let now = chrono::Utc::now();

    for path in files {
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let closed = closed_at(&path, || meta.modified().ok().map(Into::into));
        let too_old = closed
            .and_then(|t| (now - t).to_std().ok())
            .is_some_and(|age| age > caps.max_age);
        if too_old {
            remove_dropped(&path, meta.len(), &mut dropped).await?;
        } else {
            kept.push((path, meta.len()));
        }
    }

    let mut total: u64 = kept.iter().map(|(_, n)| n).sum();
    for (path, len) in kept {
        if total <= caps.max_bytes {
            break;
        }
        remove_dropped(&path, len, &mut dropped).await?;
        total -= len;
    }

    if dropped.files > 0 {
        tracing::warn!(
            dropped_files = dropped.files,
            dropped_bytes = dropped.bytes,
            max_bytes = caps.max_bytes,
            max_age_secs = caps.max_age.as_secs(),
            "log spool over its cap; dropped the oldest closed files (logs not shipped)"
        );
    }
    Ok(dropped)
}

async fn remove_dropped(path: &Path, len: u64, dropped: &mut Dropped) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {
            dropped.files += 1;
            dropped.bytes += len;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
        assert_eq!(spool.current_contents().unwrap(), b"world\n");
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
        assert!(!closed[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(CURRENT_PREFIX));
    }

    /// Every `hs` invocation is service `hs` and shares one spool directory:
    /// one process rotating (a short CLI command exiting) must not close the
    /// file another process is still writing to.
    #[tokio::test]
    async fn a_second_writer_never_rotates_the_first_writers_file() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Spool::new(tmp.path().to_path_buf()).unwrap();
        let mut daemon_writer = SpoolWriter::new(daemon.clone());
        daemon_writer.write_all(b"daemon-1\n").unwrap();

        let cli = Spool::new(tmp.path().to_path_buf()).unwrap();
        SpoolWriter::new(cli.clone()).write_all(b"cli\n").unwrap();
        let cli_closed = cli.rotate_now().unwrap().unwrap();

        daemon_writer.write_all(b"daemon-2\n").unwrap();
        daemon_writer.flush().unwrap();
        assert_eq!(std::fs::read(&cli_closed).unwrap(), b"cli\n");
        assert_eq!(
            daemon.current_contents().unwrap(),
            b"daemon-1\ndaemon-2\n",
            "the daemon's file is still its live current file"
        );
        let closed = list_closed(tmp.path()).await.unwrap();
        assert_eq!(closed, vec![cli_closed]);
    }

    /// A writer that died leaves an unlocked `current-*` file; its lines are
    /// closed for the shipper (and an empty one is removed) instead of being
    /// stranded.
    #[tokio::test]
    async fn the_file_of_a_dead_writer_is_closed_for_shipping() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("current-dead.jsonl"), b"last words\n").unwrap();
        std::fs::write(tmp.path().join("current-dead-empty.jsonl"), b"").unwrap();

        let spool = Spool::new(tmp.path().to_path_buf()).unwrap();

        let closed = list_closed(tmp.path()).await.unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(std::fs::read(&closed[0]).unwrap(), b"last words\n");
        assert!(!tmp.path().join("current-dead.jsonl").exists());
        assert!(!tmp.path().join("current-dead-empty.jsonl").exists());
        assert!(spool.current_path().exists());
    }

    fn closed_file(dir: &Path, age: Duration, size: usize) -> PathBuf {
        let ms = (SystemTime::now() - age)
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let p = dir.join(format!("{ms}-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&p, vec![b'x'; size]).unwrap();
        p
    }

    fn caps(max_bytes: u64, max_age_secs: u64) -> SpoolCaps {
        SpoolCaps {
            max_bytes,
            max_age: Duration::from_secs(max_age_secs),
        }
    }

    /// RA-80: closed files used to pile up without limit while the shipper
    /// could not reach storage.
    #[tokio::test]
    async fn over_byte_cap_drops_the_oldest_closed_files_first() {
        let tmp = tempfile::tempdir().unwrap();
        let oldest = closed_file(tmp.path(), Duration::from_secs(300), 400);
        let middle = closed_file(tmp.path(), Duration::from_secs(200), 400);
        let newest = closed_file(tmp.path(), Duration::from_secs(100), 400);
        let current = tmp.path().join("current-live.jsonl");
        std::fs::write(&current, vec![b'y'; 5000]).unwrap();

        let dropped = enforce_caps(tmp.path(), caps(900, 86_400)).await.unwrap();
        assert_eq!(dropped.files, 1);
        assert_eq!(dropped.bytes, 400);
        assert!(!oldest.exists());
        assert!(middle.exists() && newest.exists());
        assert!(current.exists(), "the live file is never a candidate");
    }

    #[tokio::test]
    async fn files_older_than_the_age_cap_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = closed_file(tmp.path(), Duration::from_secs(7200), 10);
        let fresh = closed_file(tmp.path(), Duration::from_secs(60), 10);

        let dropped = enforce_caps(tmp.path(), caps(1 << 30, 3600)).await.unwrap();
        assert_eq!(dropped.files, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[tokio::test]
    async fn within_caps_nothing_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let a = closed_file(tmp.path(), Duration::from_secs(60), 100);
        let dropped = enforce_caps(tmp.path(), caps(1 << 20, 3600)).await.unwrap();
        assert_eq!(dropped, Dropped::default());
        assert!(a.exists());
    }

    #[test]
    fn closed_at_reads_the_rotation_timestamp_from_the_file_name() {
        let p = PathBuf::from("/spool/1577934245000-8f0d.jsonl");
        let t = closed_at(&p, || None);
        assert_eq!(
            t.unwrap().format("%Y-%m-%d").to_string(),
            "2020-01-02",
            "the rotation instant, not the day it was shipped"
        );
        assert!(closed_at(&PathBuf::from("/spool/odd-name.jsonl"), || None).is_none());
    }
}
