//! `li run -- <program> [args...]` (P1-20 / P1-21, 0-B lifecycle contract).
//!
//! Spawns the target in its own session (`setsid` when available, zero `unsafe`),
//! captures the leader record immediately after spawn so <3 ms scripts are not
//! lost, samples `/proc` lineage during execution, and emits two distinct
//! lifecycle events (`ScriptStarted` and `ScriptFinished`), never merged into one
//! event type and always declaring `internal_script_commands: false` (C8).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use li_journal::clock;
use li_journal::write::Session;
use li_proto::{Event, EventId, EventType, EvidenceStatus, Source};

use crate::client::{deliver_events, make_session_id};

#[derive(Debug, Clone)]
struct ProcInfo {
    pid: i32,
    ppid: i32,
    pgid: i32,
    sid: i32,
    comm: String,
    state: char,
    starttime: u64,
}

fn parse_stat(raw: &str) -> Option<ProcInfo> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: i32 = raw[..open].trim().parse().ok()?;
    let comm = raw[open + 1..close].to_string();
    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    let field = |n: usize| -> Option<&str> { rest.get(n.checked_sub(3)?).copied() };
    Some(ProcInfo {
        pid,
        state: field(3)?.chars().next()?,
        ppid: field(4)?.parse().ok()?,
        pgid: field(5)?.parse().ok()?,
        sid: field(6)?.parse().ok()?,
        starttime: field(22)?.parse().ok()?,
        comm,
    })
}

fn read_proc(pid: i32) -> Option<ProcInfo> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&raw)
}

fn scan_proc() -> BTreeMap<i32, ProcInfo> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
            && let Some(info) = read_proc(pid)
        {
            out.insert(pid, info);
        }
    }
    out
}

fn lineage(table: &BTreeMap<i32, ProcInfo>, leader: i32) -> HashSet<i32> {
    let mut set = HashSet::new();
    if table.contains_key(&leader) {
        set.insert(leader);
    }
    for info in table.values() {
        if info.sid == leader && info.pid != leader && info.state != 'Z' {
            set.insert(info.pid);
        }
    }
    loop {
        let mut added = false;
        for info in table.values() {
            if info.state != 'Z'
                && info.pid != leader
                && !set.contains(&info.pid)
                && set.contains(&info.ppid)
            {
                set.insert(info.pid);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    set
}

#[derive(Debug, Clone, serde::Serialize)]
struct SeenProc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    sid: i32,
    starttime: u64,
    comm: String,
    observation_source: &'static str,
}

pub struct RunOptions<'a> {
    pub session: Option<&'a str>,
    pub socket: Option<&'a Path>,
    pub journal: Option<&'a Path>,
    pub post_exit_ms: u64,
    pub program: &'a str,
    pub args: &'a [String],
}

pub fn execute(opts: RunOptions<'_>) -> Result<i32, String> {
    let sid = match opts.session {
        Some(s) => li_proto::SessionId::new(s).map_err(|e| e.to_string())?,
        None => make_session_id("li-run")?,
    };
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let t_start_mono = clock::mono_ns().unwrap_or(0);
    let t_start_wall = clock::wall_now_rfc3339_ms();

    let use_setsid = Path::new("/usr/bin/setsid").exists();
    let mut cmd = if use_setsid {
        let mut c = Command::new("/usr/bin/setsid");
        c.arg("--wait").arg(opts.program).args(opts.args);
        c
    } else {
        let mut c = Command::new(opts.program);
        c.args(opts.args);
        c
    };
    cmd.env("LI_SESSION_ID", sid.as_str());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{}`: {e}", opts.program))?;
    let leader_pid = i32::try_from(child.id()).unwrap_or(0);
    let leader_at_spawn = read_proc(leader_pid);

    let mut start_payload = serde_json::Map::new();
    start_payload.insert("program".into(), serde_json::json!(opts.program));
    start_payload.insert("argv".into(), serde_json::json!(opts.args));
    start_payload.insert("cwd".into(), serde_json::json!(cwd));
    start_payload.insert("pid".into(), serde_json::json!(leader_pid));
    start_payload.insert("tier".into(), serde_json::json!("lifecycle"));
    if let Some(ref info) = leader_at_spawn {
        start_payload.insert("sid".into(), serde_json::json!(info.sid));
        start_payload.insert("pgid".into(), serde_json::json!(info.pgid));
        start_payload.insert("starttime".into(), serde_json::json!(info.starttime));
    }
    start_payload.insert(
        "capabilities".into(),
        serde_json::json!({
            "process_identity": true,
            "exit_status": true,
            "duration": true,
            "descendant_processes": "partial:sampled",
            "internal_script_commands": false,
        }),
    );

    let ev_start = Event::new(
        EventId::new(format!("{}-0", sid.as_str())).map_err(|e| e.to_string())?,
        Source::Script,
        Some(sid.clone()),
        0,
        t_start_mono,
        t_start_wall.clone(),
        EventType::new("ScriptStarted").map_err(|e| e.to_string())?,
        start_payload,
        EvidenceStatus::Observed,
    )
    .map_err(|e| e.to_string())?;

    // Background /proc lineage sampler while the child runs.
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(BTreeMap::<i32, SeenProc>::new()));
    let sampler_handle = {
        let stop = Arc::clone(&stop);
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let table = scan_proc();
                let members = lineage(&table, leader_pid);
                if let Ok(mut guard) = seen.lock() {
                    for pid in members {
                        if let Some(info) = table.get(&pid) {
                            guard.entry(pid).or_insert_with(|| SeenProc {
                                pid,
                                ppid: info.ppid,
                                pgid: info.pgid,
                                sid: info.sid,
                                starttime: info.starttime,
                                comm: info.comm.clone(),
                                observation_source: "sampled",
                            });
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let t_watch = Instant::now();
    let status = child
        .wait()
        .map_err(|e| format!("waiting for `{}`: {e}", opts.program))?;
    if opts.post_exit_ms > 0 {
        std::thread::sleep(Duration::from_millis(opts.post_exit_ms));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = sampler_handle.join();
    let duration_ns = u64::try_from(t_watch.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let t_end_mono = clock::mono_ns().unwrap_or(t_start_mono);
    let t_end_wall = clock::wall_now_rfc3339_ms();

    let mut procs: Vec<SeenProc> = seen
        .lock()
        .map(|g| g.values().cloned().collect())
        .unwrap_or_default();
    if !procs.iter().any(|p| p.pid == leader_pid)
        && let Some(info) = leader_at_spawn
    {
        // Fast (<3ms) scripts vanish before the sampler ticks; leader record
        // captured right after spawn keeps the process identity observable (0-B).
        procs.insert(
            0,
            SeenProc {
                pid: info.pid,
                ppid: info.ppid,
                pgid: info.pgid,
                sid: info.sid,
                starttime: info.starttime,
                comm: info.comm,
                observation_source: "leader_record",
            },
        );
    }

    let exit_code = status.code().unwrap_or(1);
    let mut end_payload = serde_json::Map::new();
    end_payload.insert("program".into(), serde_json::json!(opts.program));
    end_payload.insert("argv".into(), serde_json::json!(opts.args));
    end_payload.insert("exit_status".into(), serde_json::json!(exit_code));
    end_payload.insert("duration_ns".into(), serde_json::json!(duration_ns));
    end_payload.insert("tier".into(), serde_json::json!("lifecycle"));
    end_payload.insert("descendants".into(), serde_json::json!(procs));
    end_payload.insert("internal_script_commands".into(), serde_json::json!(false));

    let ev_end = Event::new(
        EventId::new(format!("{}-1", sid.as_str())).map_err(|e| e.to_string())?,
        Source::Script,
        Some(sid.clone()),
        1,
        t_end_mono,
        t_end_wall,
        EventType::new("ScriptFinished").map_err(|e| e.to_string())?,
        end_payload,
        EvidenceStatus::Observed,
    )
    .map_err(|e| e.to_string())?;

    let session_row = Session {
        session_id: sid.as_str().to_string(),
        parent_session: std::env::var("LI_SESSION_ID").ok(),
        adapter: "li-run".into(),
        adapter_version: Some(env!("CARGO_PKG_VERSION").into()),
        shell_pid: Some(i64::from(leader_pid)),
        started_wall: t_start_wall,
        started_mono_ns: i64::try_from(t_start_mono).unwrap_or(i64::MAX),
        cwd: Some(cwd),
        host: None,
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string()),
    };

    deliver_events(
        opts.socket,
        opts.journal,
        Some(&session_row),
        &[ev_start, ev_end],
    )?;
    Ok(exit_code)
}
