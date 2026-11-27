//! Non-blocking `inotify` filesystem observer (P2-04 .. P2-06, C4, C7).
//!
//! Uses `rustix::fs::inotify` with a >= 64 KiB read buffer and `AsyncFd` so the
//! reader drains unconditionally without blocking in business logic. All emitted
//! file events are `Observed` and strictly **unattributed** (never carry a
//! `pid` or `uid` claim). On `IN_Q_OVERFLOW`, emits an explicit overflow event
//! and triggers a subtree rescan to restore watch coverage.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use li_journal::{Journal, clock};
use li_proto::{Event, EventId, EventType, EvidenceStatus, Source};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, Reader as InotifyReader, WatchFlags};
use tokio::sync::watch;

pub const INOTIFY_BUFFER_BYTES: usize = 64 * 1024;

fn watch_mask() -> WatchFlags {
    WatchFlags::CREATE
        | WatchFlags::MODIFY
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::ATTRIB
        | WatchFlags::DELETE
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::MOVE_SELF
        | WatchFlags::DONT_FOLLOW
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainStats {
    pub events_emitted: u64,
    pub overflows: u64,
    pub rescans: u64,
    pub watches_added: usize,
}

/// Manages an `inotify` instance across one or more watched directory roots.
pub struct FsWatcher {
    fd: OwnedFd,
    roots: Vec<PathBuf>,
    wd_to_path: HashMap<i32, PathBuf>,
    path_to_wd: HashMap<PathBuf, i32>,
    moved_from_cookies: HashMap<u32, PathBuf>,
    seq: AtomicU64,
}

impl FsWatcher {
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, std::io::Error> {
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(std::io::Error::from)?;
        let mut watcher = Self {
            fd,
            roots,
            wd_to_path: HashMap::new(),
            path_to_wd: HashMap::new(),
            moved_from_cookies: HashMap::new(),
            seq: AtomicU64::new(0),
        };
        watcher.rescan_trees();
        Ok(watcher)
    }

    pub fn watch_count(&self) -> usize {
        self.wd_to_path.len()
    }

    /// Recursively walks all configured roots and installs any missing watches.
    pub fn rescan_trees(&mut self) -> usize {
        let mut added = 0usize;
        let mut stack: Vec<PathBuf> = self.roots.clone();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        while let Some(dir) = stack.pop() {
            if !visited.insert(dir.clone()) {
                continue;
            }
            if !self.path_to_wd.contains_key(&dir)
                && let Ok(wd) = inotify::add_watch(&self.fd, &dir, watch_mask())
            {
                self.wd_to_path.insert(wd, dir.clone());
                self.path_to_wd.insert(dir.clone(), wd);
                added += 1;
            }
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(entry.path());
                    }
                }
            }
        }
        added
    }

    fn add_subtree(&mut self, dir: &Path) -> usize {
        let mut added = 0usize;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(cur) = stack.pop() {
            if !self.path_to_wd.contains_key(&cur)
                && let Ok(wd) = inotify::add_watch(&self.fd, &cur, watch_mask())
            {
                self.wd_to_path.insert(wd, cur.clone());
                self.path_to_wd.insert(cur.clone(), wd);
                added += 1;
            }
            if let Ok(entries) = fs::read_dir(&cur) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(entry.path());
                    }
                }
            }
        }
        added
    }

    /// Drains all currently pending events from the kernel using a 64 KiB buffer.
    pub fn drain_now(&mut self, journal: &Journal) -> Result<DrainStats, std::io::Error> {
        let mut stats = DrainStats::default();
        let mut buf = vec![MaybeUninit::<u8>::uninit(); INOTIFY_BUFFER_BYTES];

        // Collect raw records first so `&self.fd` borrow ends before mutating watches.
        enum RawItem {
            Overflow,
            Ignored(i32),
            Event {
                wd: i32,
                flags: ReadFlags,
                cookie: u32,
                name: Option<String>,
            },
        }
        let mut items = Vec::new();
        {
            let mut reader = InotifyReader::new(&self.fd, &mut buf);
            loop {
                match reader.next() {
                    Err(rustix::io::Errno::WOULDBLOCK) => break,
                    Err(e) => return Err(std::io::Error::from(e)),
                    Ok(ev) => {
                        let flags = ev.events();
                        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                            items.push(RawItem::Overflow);
                        } else if flags.contains(ReadFlags::IGNORED) {
                            items.push(RawItem::Ignored(ev.wd()));
                        } else {
                            let name = ev
                                .file_name()
                                .map(|c| c.to_string_lossy().into_owned())
                                .filter(|s| !s.is_empty());
                            items.push(RawItem::Event {
                                wd: ev.wd(),
                                flags,
                                cookie: ev.cookie(),
                                name,
                            });
                        }
                    }
                }
            }
        }

        let mono = clock::mono_ns().unwrap_or(0);
        let wall = clock::wall_now_rfc3339_ms();

        for item in items {
            match item {
                RawItem::Overflow => {
                    stats.overflows += 1;
                    stats.rescans += 1;
                    let added = self.rescan_trees();
                    stats.watches_added += added;
                    self.emit_overflow(journal, added, mono, &wall);
                    stats.events_emitted += 1;
                }
                RawItem::Ignored(wd) => {
                    if let Some(p) = self.wd_to_path.remove(&wd) {
                        self.path_to_wd.remove(&p);
                    }
                }
                RawItem::Event {
                    wd,
                    flags,
                    cookie,
                    name,
                } => {
                    let Some(dir) = self.wd_to_path.get(&wd).cloned() else {
                        continue;
                    };
                    let full_path = match &name {
                        Some(n) => dir.join(n),
                        None => dir.clone(),
                    };
                    let is_dir = flags.contains(ReadFlags::ISDIR);

                    if is_dir
                        && (flags.contains(ReadFlags::CREATE)
                            || flags.contains(ReadFlags::MOVED_TO))
                    {
                        stats.watches_added += self.add_subtree(&full_path);
                    }

                    if flags.contains(ReadFlags::MOVED_FROM) && cookie != 0 {
                        self.moved_from_cookies.insert(cookie, full_path.clone());
                        // Bound cookie table against stale moves into unwatched dirs.
                        if self.moved_from_cookies.len() > 4096 {
                            self.moved_from_cookies.clear();
                        }
                        self.emit_fs_event(
                            journal,
                            "FileRenamed",
                            &full_path,
                            None,
                            flags,
                            cookie,
                            is_dir,
                            mono,
                            &wall,
                        );
                        stats.events_emitted += 1;
                        continue;
                    }

                    if flags.contains(ReadFlags::MOVED_TO) {
                        let from = (cookie != 0)
                            .then(|| self.moved_from_cookies.remove(&cookie))
                            .flatten();
                        self.emit_fs_event(
                            journal,
                            "FileRenamed",
                            &full_path,
                            from.as_deref(),
                            flags,
                            cookie,
                            is_dir,
                            mono,
                            &wall,
                        );
                        stats.events_emitted += 1;
                        continue;
                    }

                    let kind = if flags.contains(ReadFlags::CREATE) {
                        "FileCreated"
                    } else if flags.contains(ReadFlags::DELETE)
                        || flags.contains(ReadFlags::DELETE_SELF)
                    {
                        "FileDeleted"
                    } else if flags.contains(ReadFlags::MOVE_SELF) {
                        "FileRenamed"
                    } else {
                        "FileModified"
                    };

                    self.emit_fs_event(
                        journal, kind, &full_path, None, flags, cookie, is_dir, mono, &wall,
                    );
                    stats.events_emitted += 1;
                }
            }
        }
        Ok(stats)
    }

    fn emit_overflow(&self, journal: &Journal, rescanned_watches: usize, mono: u64, wall: &str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut payload = serde_json::Map::new();
        payload.insert("data_loss".into(), serde_json::json!(true));
        payload.insert("reason".into(), serde_json::json!("IN_Q_OVERFLOW"));
        payload.insert(
            "rescanned_watches_added".into(),
            serde_json::json!(rescanned_watches),
        );
        payload.insert(
            "watched_roots".into(),
            serde_json::json!(
                self.roots
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            ),
        );
        if let Ok(event_id) = EventId::new(format!("fs-overflow-{mono}-{seq}"))
            && let Ok(event_type) = EventType::new("InotifyOverflow")
            && let Ok(ev) = Event::new(
                event_id,
                Source::Fs,
                None,
                0,
                mono,
                wall,
                event_type,
                payload,
                EvidenceStatus::Observed,
            )
        {
            let _ = journal.event(&ev);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_fs_event(
        &self,
        journal: &Journal,
        kind: &str,
        path: &Path,
        from_path: Option<&Path>,
        flags: ReadFlags,
        cookie: u32,
        is_dir: bool,
        mono: u64,
        wall: &str,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut payload = serde_json::Map::new();
        // `payload.path` matches `idx_events_payload_path` (0-H §7.4).
        // C4 / P2-06: strictly NO `pid` or `uid` attribution fields.
        payload.insert("path".into(), serde_json::json!(path.to_string_lossy()));
        payload.insert("is_dir".into(), serde_json::json!(is_dir));
        payload.insert("mask".into(), serde_json::json!(flags.bits()));
        if cookie != 0 {
            payload.insert("cookie".into(), serde_json::json!(cookie));
        }
        if let Some(from) = from_path {
            payload.insert("from".into(), serde_json::json!(from.to_string_lossy()));
            payload.insert("to".into(), serde_json::json!(path.to_string_lossy()));
        }
        if let Ok(event_id) = EventId::new(format!("fs-{mono}-{seq}"))
            && let Ok(event_type) = EventType::new(kind)
            && let Ok(ev) = Event::new(
                event_id,
                Source::Fs,
                None,
                0,
                mono,
                wall,
                event_type,
                payload,
                EvidenceStatus::Observed,
            )
        {
            let _ = journal.event(&ev);
        }
    }
}

/// Runs the non-blocking `inotify` observer loop on a dedicated blocking thread
/// using `rustix::event::poll` (0 `unsafe`, unconditional 64 KiB drain).
pub async fn run_fs_watcher(
    journal: Arc<Journal>,
    roots: Vec<PathBuf>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), std::io::Error> {
    if roots.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let mut watcher = FsWatcher::new(roots)?;
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        loop {
            if *shutdown.borrow() {
                break;
            }
            let ready = {
                let mut fds = [PollFd::new(&watcher.fd, PollFlags::IN)];
                poll(&mut fds, Some(&timeout)).map_err(std::io::Error::from)? > 0
                    && fds[0].revents().contains(PollFlags::IN)
            };
            if ready {
                let _ = watcher.drain_now(&journal);
            }
        }
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)?
}
