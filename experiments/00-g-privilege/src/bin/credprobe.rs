//! `SO_PEERCRED` TOCTOU and pid-reuse harness.
//!
//! Modes:
//!   toctou --socket P --delay-ms N [--impostor]
//!       Fork a child that connects and `_exit(0)`s immediately without one
//!       byte of protocol. Sleep, *then* read `SO_PEERCRED`. With `--impostor`,
//!       force a different process to take over the dead client's pid (needs a
//!       private pid namespace so `ns_last_pid` is writable).
//!   same-tick --pid N --trials M
//!       Reuse one pid twice inside one `CLK_TCK` tick and count how often
//!       `(pid, starttime)` collides.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use li_privhelper::peer;
use serde_json::json;

fn main() {
    if let Err(e) = run() {
        eprintln!("credprobe: {e}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("usage: credprobe toctou|same-tick ...")?.clone();
    let get = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    match mode.as_str() {
        "toctou" => {
            let socket = get("--socket").ok_or("--socket required")?;
            let delay_ms: u64 = get("--delay-ms")
                .unwrap_or_else(|| "1000".into())
                .parse()
                .unwrap_or(1000);
            toctou(&socket, delay_ms, flag("--impostor"))
        }
        "same-tick" => {
            let pid: i32 = get("--pid")
                .ok_or("--pid required")?
                .parse()
                .map_err(|_| "bad pid")?;
            let trials: u32 = get("--trials")
                .unwrap_or_else(|| "20".into())
                .parse()
                .unwrap_or(20);
            same_tick(pid, trials)
        }
        other => Err(format!("unknown mode {other}")),
    }
}

fn clk_tck() -> i64 {
    // SAFETY: sysconf has no pointer arguments and is thread-safe.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v
    } else {
        100
    }
}

fn pidns_of(pid: i32) -> String {
    std::fs::read_link(format!("/proc/{pid}/ns/pid"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unreadable".into())
}

/// Write `/proc/sys/kernel/ns_last_pid` so the next fork receives `pid`.
fn force_next_pid(pid: i32) -> Result<(), String> {
    let path = "/proc/sys/kernel/ns_last_pid";
    std::fs::write(path, (pid - 1).to_string())
        .map_err(|e| format!("write {path}={} failed: {e}", pid - 1))
}

fn toctou(socket_path: &str, delay_ms: u64, impostor: bool) -> Result<(), String> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).map_err(|e| format!("bind: {e}"))?;

    // Fork the client BEFORE any thread exists, so the child is talking to a
    // single-threaded parent. SAFETY: single-threaded fork; the child only
    // calls connect/shutdown/_exit, all async-signal-safe.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        let mut err = 0;
        match UnixStream::connect(socket_path) {
            Ok(s) => {
                // Deliberately no write: SO_PEERCRED is established by connect().
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Err(_) => err = 1,
        }
        unsafe { libc::_exit(err) };
    }
    let client_pid = pid;

    let (conn, _addr) = listener.accept().map_err(|e| format!("accept: {e}"))?;
    let t_accept = li_privhelper::audit::now_mono_ns();

    // Snapshot #1: read the credential immediately at accept.
    let cred_at_accept = peer::peer_cred(conn.as_raw_fd()).map_err(|e| format!("peer_cred: {e}"))?;
    let st_at_accept = peer::starttime(cred_at_accept.pid);
    let cmd_at_accept = peer::cmdline_first_arg(cred_at_accept.pid);

    std::thread::sleep(Duration::from_millis(delay_ms));
    let t_probe = li_privhelper::audit::now_mono_ns();

    // Snapshot #2: the *same* socket, read again, much later.
    let cred_later = peer::peer_cred(conn.as_raw_fd()).map_err(|e| format!("peer_cred: {e}"))?;
    let live = peer::liveness(cred_later.pid, st_at_accept);

    let child_state = std::fs::read_to_string(format!("/proc/{client_pid}/stat"))
        .ok()
        .and_then(|s| {
            let close = s.rfind(')')?;
            s.get(close + 2..)?.chars().next()
        });

    println!(
        "{}",
        json!({
            "case": "toctou_snapshot",
            "delay_ms": delay_ms,
            "client_fork_pid": client_pid,
            "cred_at_accept": { "pid": cred_at_accept.pid, "uid": cred_at_accept.uid, "gid": cred_at_accept.gid },
            "cred_after_delay": { "pid": cred_later.pid, "uid": cred_later.uid, "gid": cred_later.gid },
            "cred_is_frozen": cred_at_accept == cred_later,
            "starttime_at_accept": st_at_accept,
            "cmdline_at_accept": cmd_at_accept,
            "elapsed_between_probes_ns": t_probe.saturating_sub(t_accept),
            "pid_liveness_after_delay": live.as_str(),
            "pid_dir_exists_after_delay": std::path::Path::new(&format!("/proc/{}", cred_later.pid)).exists(),
            "starttime_now": peer::starttime(cred_later.pid),
            "cmdline_now": peer::cmdline_first_arg(cred_later.pid),
            "client_zombie_state": child_state,
            "pid_namespace": pidns_of(cred_later.pid),
            "clk_tck": clk_tck(),
        })
    );
    let _ = std::io::stdout().flush();

    // --- impostor: force a *different* process to take over the dead pid.
    let mut impostor_report = json!({ "attempted": false });
    if impostor {
        let dead = cred_later.pid;
        match force_next_pid(dead) {
            Err(e) => {
                impostor_report = json!({
                    "attempted": true, "ok": false, "reason": e,
                    "note": "ns_last_pid is writable only inside a private pid namespace"
                });
            }
            Ok(()) => {
                let imp = unsafe { libc::fork() };
                if imp < 0 {
                    impostor_report = json!({
                        "attempted": true, "ok": false, "reason": "fork failed"
                    });
                } else if imp == 0 {
                    // Child: a different program identity holding the reused pid.
                    unsafe {
                        libc::prctl(libc::PR_SET_NAME, c"impostor".as_ptr(), 0, 0, 0);
                        loop {
                            libc::pause();
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(150));
                    let st_imp = peer::starttime(imp);
                    let cred_again =
                        peer::peer_cred(conn.as_raw_fd()).unwrap_or(peer::PeerCred { pid: -1, uid: 0, gid: 0 });
                    impostor_report = json!({
                        "attempted": true,
                        "ok": true,
                        "impostor_pid": imp,
                        "impostor_starttime": st_imp,
                        "pid_equals_dead_client": imp == dead,
                        "starttime_differs_from_client": st_imp != st_at_accept,
                        "cred_on_same_socket_still_says_pid": cred_again.pid,
                        "cred_pid_now_names_the_impostor": cred_again.pid == imp,
                        "naive_pid_only_identity_confusion": cred_again.pid == imp && st_imp != st_at_accept,
                        "starttime_source": "field 22 of /proc/<pid>/stat",
                    });
                    unsafe {
                        libc::kill(imp, libc::SIGKILL);
                        libc::waitpid(imp, std::ptr::null_mut(), 0);
                    }
                }
            }
        }
    }
    println!(
        "{}",
        json!({ "case": "toctou_impostor", "detail": impostor_report })
    );
    let _ = conn.shutdown(std::net::Shutdown::Both);
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn same_tick(target: i32, trials: u32) -> Result<(), String> {
    let mut collisions = 0u32;
    let mut did = 0u32;
    for i in 0..trials {
        // Force the pid twice in a row as fast as possible. If both forks land
        // in the same 10 ms jiffy, (pid, starttime) is identical for two
        // different processes.
        force_next_pid(target).map_err(|_| {
            "ns_last_pid not writable; run inside `unshare -Urpf --mount-proc`".to_string()
        })?;
        let a = unsafe { libc::fork() };
        if a == 0 {
            unsafe { libc::_exit(0) };
        }
        let st_a = peer::starttime(a);
        let mut status = 0;
        unsafe { libc::waitpid(a, &mut status, 0) };

        force_next_pid(target)?;
        let b = unsafe { libc::fork() };
        if b == 0 {
            unsafe { libc::_exit(0) };
        }
        let st_b = peer::starttime(b);
        unsafe { libc::waitpid(b, &mut status, 0) };

        let same_pid = a == b && a == target;
        let same_st = st_a == st_b && st_a.is_some();
        let collide = same_pid && same_st;
        if same_pid {
            did += 1;
        }
        if collide {
            collisions += 1;
        }
        println!(
            "{}",
            json!({
                "trial": i, "target_pid": target,
                "fork_a": a, "starttime_a": st_a,
                "fork_b": b, "starttime_b": st_b,
                "same_pid": same_pid, "same_starttime": same_st,
                "pid_starttime_key_collides": collide,
            })
        );
    }
    println!(
        "{}",
        json!({
            "case": "same_tick_summary",
            "target_pid": target, "trials": trials,
            "forks_at_target_pid": did, "collisions": collisions,
            "clk_tck": clk_tck(),
            "verdict": if collisions > 0 {
                "pid_starttime_is_not_a_unique_key_within_one_tick"
            } else {
                "no_collision_observed"
            },
        })
    );
    Ok(())
}
