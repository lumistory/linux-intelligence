use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    pub tty_nr: i64,
    pub comm: String,
    pub state: char,
    pub starttime_ticks: u64,
    pub utime_ticks: u64,
    pub stime_ticks: u64,
}

/// Parse `/proc/<pid>/stat`. The `comm` field may contain spaces and
/// parentheses, so parsing is anchored on the *last* `)`.
pub fn parse_stat(raw: &str) -> Option<ProcInfo> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: i32 = raw[..open].trim().parse().ok()?;
    let comm = raw[open + 1..close].to_string();
    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state); index = absolute field number - 3.
    let field = |n: usize| -> Option<&str> { rest.get(n.checked_sub(3)?).copied() };
    Some(ProcInfo {
        pid,
        state: field(3)?.chars().next()?,
        ppid: field(4)?.parse().ok()?,
        pgid: field(5)?.parse().ok()?,
        sid: field(6)?.parse().ok()?,
        tty_nr: field(7)?.parse().ok()?,
        starttime_ticks: field(22)?.parse().ok()?,
        utime_ticks: field(14)?.parse().ok()?,
        stime_ticks: field(15)?.parse().ok()?,
        comm,
    })
}

pub fn read_one(pid: i32) -> Option<ProcInfo> {
    // `read_to_string` on procfs is fine: each file is one short line.
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&raw)
}

/// Read the whole process table. Processes that vanish mid-scan are skipped,
/// which is the correct behaviour for a snapshot.
pub fn scan() -> Option<BTreeMap<i32, ProcInfo>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut out = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(info) = read_one(pid) {
            out.insert(pid, info);
        }
    }
    Some(out)
}

pub fn readlink_or_none(path: &str) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// pid -> (ppid, comm, sid), used for the before/after diff.
pub type Snapshot = HashMap<i32, (i32, String, i32)>;

pub fn snapshot() -> Snapshot {
    match scan() {
        Some(table) => table
            .into_iter()
            .map(|(pid, i)| (pid, (i.ppid, i.comm, i.sid)))
            .collect(),
        None => HashMap::new(),
    }
}

pub struct Appeared {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub sid: i32,
}

/// Processes present after the run but absent from the pre-run snapshot.
pub fn appeared(before: &Snapshot, after: &Snapshot) -> Vec<Appeared> {
    let mut v: Vec<Appeared> = after
        .iter()
        .filter(|(pid, _)| !before.contains_key(pid))
        .map(|(pid, (ppid, comm, sid))| Appeared {
            pid: *pid,
            ppid: *ppid,
            comm: comm.clone(),
            sid: *sid,
        })
        .collect();
    v.sort_by_key(|x| x.pid);
    v
}

/// Lineage of one script: the leader, everything sharing its session id, and
/// the closure over parent links (covers a descendant that called `setsid`).
///
/// The leader is always included, even as a zombie: it is a first-hand fact
/// that we forked it, and the leader is usually still a zombie at the moment
/// the supervisor finishes waiting.  Non-leader zombies are excluded because
/// they are mid-reap and their pid may already be reused.
pub fn lineage(table: &BTreeMap<i32, ProcInfo>, leader: i32) -> HashSet<i32> {
    let mut set: HashSet<i32> = HashSet::new();
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
    set.retain(|pid| *pid == leader || table.get(pid).is_none_or(|i| i.state != 'Z'));
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comm_with_parens_and_spaces() {
        // Field layout: pid (comm) state ppid pgrp session tty_nr tpgid flags
        // minflt cminflt majflt cmajflt utime stime cutime cstime priority
        // nice num_threads itrealvalue starttime ...
        let raw =
            "1234 (a) b (c) S 12 34 56 34816 -1 4194304 100 0 0 0 7 8 0 0 20 0 1 0 999888 123 456";
        let p = parse_stat(raw).expect("parse");
        assert_eq!(p.pid, 1234);
        assert_eq!(p.comm, "a) b (c");
        assert_eq!(p.state, 'S');
        assert_eq!(p.ppid, 12);
        assert_eq!(p.pgid, 34);
        assert_eq!(p.sid, 56);
        assert_eq!(p.tty_nr, 34816);
        assert_eq!(p.utime_ticks, 7);
        assert_eq!(p.stime_ticks, 8);
        assert_eq!(p.starttime_ticks, 999888);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("nope").is_none());
        assert!(parse_stat("12 (x) S 1").is_none());
    }

    #[test]
    fn own_process_is_visible() {
        let me = std::process::id() as i32;
        assert!(read_one(me).is_some());
    }
}
