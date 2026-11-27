//! 200 ms `procfs` process observer (P2-01 .. P2-03, C5).
//!
//! Reads `/proc/<pid>/stat` (`Stat` depth by default; reads `/proc/<pid>/cmdline`
//! only on newly observed creations). Tracks `(pid, starttime)` with
//! `incarnation_seq` and a sticky `ambiguous` flag on same-tick PID reuse or
//! metadata conflict so downstream correlation degrades to `Unknown` instead of
//! guessing.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use li_journal::write::Process;
use li_journal::{Journal, clock};
use li_proto::{Event, EventId, EventType, EvidenceStatus, Source};
use tokio::sync::watch;

pub const DEFAULT_PROC_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i32,
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub starttime: u64,
}

/// Parses `/proc/<pid>/stat`, anchoring on the last `)` so `comm` may contain
/// spaces or parentheses.
pub fn parse_stat(raw: &str) -> Option<ProcStat> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: i32 = raw[..open].trim().parse().ok()?;
    let comm = raw[open + 1..close].to_string();
    let f: Vec<&str> = raw[close + 1..].split_ascii_whitespace().collect();
    // Field 3 (state) is f[0]; field 22 (starttime) is f[19].
    if f.len() <= 19 {
        return None;
    }
    let state = f[0].chars().next()?;
    let ppid: i32 = f[1].parse().ok()?;
    let starttime: u64 = f[19].parse().ok()?;
    Some(ProcStat {
        pid,
        comm,
        state,
        ppid,
        starttime,
    })
}

fn read_cmdline(pid: i32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let joined = bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// Scans `/proc` at `Stat` depth. Processes disappearing mid-scan are skipped.
pub fn scan_proc_table() -> BTreeMap<i32, ProcStat> {
    let mut table = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(st) = parse_stat(&raw) {
            table.insert(pid, st);
        }
    }
    table
}

#[derive(Debug, Clone)]
struct ActiveProc {
    stat: ProcStat,
    incarnation_seq: i64,
    ambiguous: bool,
}

#[derive(Debug, Clone)]
struct KeyState {
    incarnation_seq: i64,
    ambiguous: bool,
    last_comm: String,
    last_ppid: i32,
    currently_active: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollStepStats {
    pub total_visible: usize,
    pub created: usize,
    pub exited: usize,
    pub same_tick_collisions: usize,
}

/// Stateful `procfs` delta detector. Purely deterministic given a snapshot,
/// making same-tick `(pid, starttime)` collision rules directly testable.
pub struct ProcPoller {
    active: HashMap<i32, ActiveProc>,
    keys: HashMap<(i32, u64), KeyState>,
    primed: bool,
    seq: AtomicU64,
    emit_initial_baseline: bool,
}

impl Default for ProcPoller {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ProcPoller {
    /// When `emit_initial_baseline` is `false`, the first scan primes the active
    /// table without flooding `ProcessCreated` for pre-existing host processes
    /// (while still upserting `processes` rows).
    pub fn new(emit_initial_baseline: bool) -> Self {
        Self {
            active: HashMap::new(),
            keys: HashMap::new(),
            primed: false,
            seq: AtomicU64::new(0),
            emit_initial_baseline,
        }
    }

    pub fn poll_host(&mut self, journal: &Journal) -> PollStepStats {
        let table = scan_proc_table();
        self.apply_snapshot(&table, journal, true)
    }

    pub fn apply_snapshot(
        &mut self,
        table: &BTreeMap<i32, ProcStat>,
        journal: &Journal,
        read_cmdlines: bool,
    ) -> PollStepStats {
        let mono = clock::mono_ns().unwrap_or(0);
        let wall = clock::wall_now_rfc3339_ms();
        let mut stats = PollStepStats {
            total_visible: table.len(),
            ..Default::default()
        };

        let emit_events = self.primed || self.emit_initial_baseline;

        // 1. Detect exited pids.
        let gone: Vec<i32> = self
            .active
            .keys()
            .filter(|pid| !table.contains_key(pid))
            .copied()
            .collect();
        for pid in gone {
            if let Some(prev) = self.active.remove(&pid) {
                if let Some(ks) = self.keys.get_mut(&(prev.stat.pid, prev.stat.starttime)) {
                    ks.currently_active = false;
                }
                if emit_events {
                    stats.exited += 1;
                    self.emit_process_event(
                        journal,
                        "ProcessExited",
                        &prev.stat,
                        prev.incarnation_seq,
                        prev.ambiguous,
                        None,
                        mono,
                        &wall,
                    );
                }
            }
        }

        // 2. Detect newly seen pids or same-pid identity changes.
        for (&pid, st) in table {
            if let Some(cur) = self.active.get(&pid) {
                if cur.stat.starttime == st.starttime {
                    // Same incarnation; update if comm changed (exec) without changing identity.
                    continue;
                }
                // Same PID, different starttime: old process exited and pid was reused.
                let old = cur.clone();
                if let Some(ks) = self.keys.get_mut(&(old.stat.pid, old.stat.starttime)) {
                    ks.currently_active = false;
                }
                if emit_events {
                    stats.exited += 1;
                    self.emit_process_event(
                        journal,
                        "ProcessExited",
                        &old.stat,
                        old.incarnation_seq,
                        old.ambiguous,
                        None,
                        mono,
                        &wall,
                    );
                }
                self.active.remove(&pid);
            }

            let key = (st.pid, st.starttime);
            let (incarnation_seq, ambiguous, collision) = match self.keys.get_mut(&key) {
                Some(ks) if !ks.currently_active => {
                    // C5 / 0-E A6b: `(pid, starttime)` seen again after exit -> same-tick reuse!
                    ks.incarnation_seq += 1;
                    ks.ambiguous = true;
                    ks.last_comm = st.comm.clone();
                    ks.last_ppid = st.ppid;
                    ks.currently_active = true;
                    (ks.incarnation_seq, true, true)
                }
                Some(ks) => {
                    let conflict = ks.last_comm != st.comm || ks.last_ppid != st.ppid;
                    if conflict {
                        ks.ambiguous = true;
                    }
                    ks.currently_active = true;
                    (ks.incarnation_seq, ks.ambiguous, conflict)
                }
                None => {
                    self.keys.insert(
                        key,
                        KeyState {
                            incarnation_seq: 0,
                            ambiguous: false,
                            last_comm: st.comm.clone(),
                            last_ppid: st.ppid,
                            currently_active: true,
                        },
                    );
                    (0, false, false)
                }
            };
            if collision {
                stats.same_tick_collisions += 1;
            }

            journal.submit(li_journal::write::Write::Process(Process {
                pid: i64::from(st.pid),
                starttime: i64::try_from(st.starttime).unwrap_or(i64::MAX),
                incarnation_seq,
                comm: Some(st.comm.clone()),
                ppid: Some(i64::from(st.ppid)),
                session_id: None,
                first_seen_mono_ns: i64::try_from(mono).unwrap_or(i64::MAX),
                ambiguous,
            }));

            self.active.insert(
                pid,
                ActiveProc {
                    stat: st.clone(),
                    incarnation_seq,
                    ambiguous,
                },
            );

            if emit_events {
                stats.created += 1;
                let cmdline = if read_cmdlines {
                    read_cmdline(pid)
                } else {
                    None
                };
                self.emit_process_event(
                    journal,
                    "ProcessCreated",
                    st,
                    incarnation_seq,
                    ambiguous,
                    cmdline,
                    mono,
                    &wall,
                );
            }
        }

        // Bound key history against long-running memory growth while preserving recent collision memory.
        if self.keys.len() > 16_384 {
            self.keys.retain(|_, v| v.currently_active || v.ambiguous);
        }

        self.primed = true;
        stats
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_process_event(
        &self,
        journal: &Journal,
        kind: &str,
        st: &ProcStat,
        incarnation_seq: i64,
        ambiguous: bool,
        cmdline: Option<String>,
        mono: u64,
        wall: &str,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut payload = serde_json::Map::new();
        payload.insert("pid".into(), serde_json::json!(st.pid));
        payload.insert("ppid".into(), serde_json::json!(st.ppid));
        payload.insert("comm".into(), serde_json::json!(st.comm));
        payload.insert("starttime".into(), serde_json::json!(st.starttime));
        payload.insert("incarnation_seq".into(), serde_json::json!(incarnation_seq));
        payload.insert("ambiguous".into(), serde_json::json!(ambiguous));
        if let Some(cmd) = cmdline {
            payload.insert("cmdline".into(), serde_json::json!(cmd));
        }
        let eid = format!("proc-{}-{}-{seq}", st.pid, st.starttime);
        if let Ok(event_id) = EventId::new(eid)
            && let Ok(event_type) = EventType::new(kind)
            && let Ok(ev) = Event::new(
                event_id,
                Source::Proc,
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

/// Runs the 200 ms `procfs` polling loop until `shutdown` fires.
pub async fn run_proc_poller(
    journal: Arc<Journal>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut poller = ProcPoller::new(false);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                let j = Arc::clone(&journal);
                let res = tokio::task::spawn_blocking(move || {
                    poller.poll_host(&j);
                    poller
                }).await;
                match res {
                    Ok(p) => poller = p,
                    Err(_) => break,
                }
            }
        }
    }
}
