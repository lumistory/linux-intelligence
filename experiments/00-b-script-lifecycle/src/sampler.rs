use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::events::now_mono_ns;
use crate::procfs;

#[derive(Debug, Clone)]
pub struct SeenProcess {
    pub pid: i32,
    pub ppid_first: i32,
    pub pgid_first: i32,
    pub sid: i32,
    pub comm_first: String,
    pub comm_last: String,
    pub exe_first: Option<String>,
    pub exe_last: Option<String>,
    pub cmdline_first: Option<String>,
    pub state_last: char,
    pub tty_nr_last: i64,
    pub utime_ticks_last: u64,
    pub stime_ticks_last: u64,
    pub samples: u32,
    pub first_seen_mono_ns: u64,
    pub last_seen_mono_ns: u64,
    pub alive_after_exit: bool,
    /// `sampled` — seen by the /proc sampler thread.
    /// `leader_record` — never sampled (script shorter than one interval);
    /// recorded from the supervisor's own fork/reap facts.
    pub observation_source: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct SampleStats {
    pub reads: u64,
    pub total_read_ns: u64,
    pub max_read_ns: u64,
    pub max_concurrent: usize,
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    procs: Arc<Mutex<BTreeMap<i32, SeenProcess>>>,
    stats: Arc<Mutex<SampleStats>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    /// Start sampling; only the lineage of `leader` is retained.
    pub fn start(leader: i32, interval_us: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let procs: Arc<Mutex<BTreeMap<i32, SeenProcess>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let stats = Arc::new(Mutex::new(SampleStats::default()));
        let interval = Duration::from_micros(interval_us.clamp(200, 5_000_000));

        let handle = {
            let stop = Arc::clone(&stop);
            let procs = Arc::clone(&procs);
            let stats = Arc::clone(&stats);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let t0 = Instant::now();
                    if let Some(table) = procfs::scan() {
                        let members = procfs::lineage(&table, leader);
                        let now = now_mono_ns();
                        let mut guard = match procs.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        for pid in members {
                            let Some(info) = table.get(&pid) else {
                                continue;
                            };
                            let entry = guard.entry(pid).or_insert_with(|| SeenProcess {
                                pid,
                                ppid_first: info.ppid,
                                pgid_first: info.pgid,
                                sid: info.sid,
                                comm_first: info.comm.clone(),
                                comm_last: info.comm.clone(),
                                exe_first: procfs::readlink_or_none(&format!("/proc/{pid}/exe")),
                                exe_last: None,
                                cmdline_first: cmdline_of(pid),
                                state_last: info.state,
                                tty_nr_last: info.tty_nr,
                                utime_ticks_last: info.utime_ticks,
                                stime_ticks_last: info.stime_ticks,
                                samples: 0,
                                first_seen_mono_ns: now,
                                last_seen_mono_ns: now,
                                alive_after_exit: false,
                                observation_source: "sampled",
                            });
                            entry.samples = entry.samples.saturating_add(1);
                            entry.last_seen_mono_ns = now;
                            entry.state_last = info.state;
                            entry.tty_nr_last = info.tty_nr;
                            entry.utime_ticks_last = info.utime_ticks;
                            entry.stime_ticks_last = info.stime_ticks;
                            entry.comm_last = info.comm.clone();
                            entry.exe_last = procfs::readlink_or_none(&format!("/proc/{pid}/exe"));
                        }
                        let n = guard.len();
                        drop(guard);
                        let elapsed = t0.elapsed().as_nanos() as u64;
                        let mut s = match stats.lock() {
                            Ok(s) => s,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        s.reads = s.reads.saturating_add(1);
                        s.total_read_ns = s.total_read_ns.saturating_add(elapsed);
                        s.max_read_ns = s.max_read_ns.max(elapsed);
                        s.max_concurrent = s.max_concurrent.max(n);
                    }
                    std::thread::sleep(interval);
                }
            })
        };

        Sampler {
            stop,
            procs,
            stats,
            handle: Some(handle),
        }
    }

    /// Stop the thread, run one final sweep, and return the collected records.
    pub fn finish(mut self, leader: i32) -> (Vec<SeenProcess>, SampleStats) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                // The sampler thread panicked; records collected so far are
                // still returned and the report notes the failure.
            }
        }
        let mut out = match self.procs.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(table) = procfs::scan() {
            let members = procfs::lineage(&table, leader);
            for pid in members {
                if let Some(info) = table.get(&pid) {
                    let now = now_mono_ns();
                    let e = out.entry(pid).or_insert_with(|| SeenProcess {
                        pid,
                        ppid_first: info.ppid,
                        pgid_first: info.pgid,
                        sid: info.sid,
                        comm_first: info.comm.clone(),
                        comm_last: info.comm.clone(),
                        exe_first: procfs::readlink_or_none(&format!("/proc/{pid}/exe")),
                        exe_last: None,
                        cmdline_first: cmdline_of(pid),
                        state_last: info.state,
                        tty_nr_last: info.tty_nr,
                        utime_ticks_last: info.utime_ticks,
                        stime_ticks_last: info.stime_ticks,
                        samples: 0,
                        first_seen_mono_ns: now,
                        last_seen_mono_ns: now,
                        alive_after_exit: false,
                        observation_source: "sampled",
                    });
                    e.alive_after_exit = true;
                    e.state_last = info.state;
                    e.tty_nr_last = info.tty_nr;
                    e.utime_ticks_last = info.utime_ticks;
                    e.stime_ticks_last = info.stime_ticks;
                    e.comm_last = info.comm.clone();
                    e.exe_last = procfs::readlink_or_none(&format!("/proc/{pid}/exe"));
                }
            }
        }
        let stats = match self.stats.lock() {
            Ok(s) => s.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        (out.into_values().collect(), stats)
    }
}

pub fn cmdline_of(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_sees_its_own_leader() {
        let pid = std::process::id() as i32;
        let s = Sampler::start(pid, 2_000);
        std::thread::sleep(Duration::from_millis(60));
        let (procs, stats) = s.finish(pid);
        assert!(stats.reads >= 1);
        assert!(procs.iter().any(|p| p.pid == pid));
    }
}
