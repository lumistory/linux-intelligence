//! `li-run-proto` — minimal prototype of `li run -- <program> <args...>`.
//!
//! Contract (see README.md): start the program in its own session, observe
//! everything the kernel will tell a non-shell supervisor, and emit exactly
//! two JSON events on stdout (ScriptStarted, ScriptFinished). The finished
//! event carries the reconstructed descendant process list.

mod error;
mod events;
mod procfs;
mod sampler;

use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use error::Error;
use events::{now_mono_ns, Event};
use sampler::{cmdline_of, Sampler};

const USAGE: &str = "\
usage: li-run-proto --session <id> --out-dir <dir> [options] -- <program> [args...]

options:
  --session <id>        session id written into every event
  --out-dir <dir>       directory for stdout.txt / stderr.txt of the script
  --poll-us <n>         /proc sampling interval in microseconds (default 2000)
  --post-exit-ms <n>    keep sampling this long after the leader exits (default 250)
  --signal-after-ms <n> inject a signal into the child pgroup after n ms
  --signal <NAME>       signal to inject (INT, KILL, TERM, ...); both flags needed
  --cleanup-kill        after observing, SIGKILL surviving session members
  --print-procs         also print the collected process list as one JSON line
  --                   end of options; program follows
";

struct Args {
    session_id: String,
    out_dir: PathBuf,
    program: String,
    argv: Vec<String>,
    poll_us: u64,
    post_exit_ms: u64,
    signal_after_ms: u64,
    signal: Option<nix::sys::signal::Signal>,
    cleanup_kill: bool,
    print_procs: bool,
}

fn parse_args() -> Result<Args, Error> {
    let mut it = std::env::args().skip(1);
    let mut session_id: Option<String> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut poll_us = 2_000u64;
    let mut post_exit_ms = 250u64;
    let mut signal_after_ms: u64 = 0;
    let mut signal: Option<nix::sys::signal::Signal> = None;
    let mut cleanup_kill = false;
    let mut print_procs = false;
    let mut program: Option<String> = None;
    let mut argv: Vec<String> = Vec::new();
    let mut after_sep = false;

    while let Some(a) = it.next() {
        if after_sep {
            if program.is_none() {
                program = Some(a);
            } else {
                argv.push(a);
            }
            continue;
        }
        match a.as_str() {
            "--" => after_sep = true,
            "--session" => session_id = Some(next(&mut it, "--session")?),
            "--out-dir" => out_dir = Some(PathBuf::from(next(&mut it, "--out-dir")?)),
            "--poll-us" => {
                poll_us = next(&mut it, "--poll-us")?
                    .parse()
                    .map_err(|_| Error::Usage("--poll-us must be an integer".into()))?;
            }
            "--post-exit-ms" => {
                post_exit_ms = next(&mut it, "--post-exit-ms")?
                    .parse()
                    .map_err(|_| Error::Usage("--post-exit-ms must be an integer".into()))?;
            }
            "--signal-after-ms" => {
                signal_after_ms = next(&mut it, "--signal-after-ms")?
                    .parse()
                    .map_err(|_| Error::Usage("--signal-after-ms must be an integer".into()))?;
            }
            "--signal" => signal = Some(parse_signal(&next(&mut it, "--signal")?)?),
            "--cleanup-kill" => cleanup_kill = true,
            "--print-procs" => print_procs = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(Error::Usage(format!("unknown argument `{other}`\n{USAGE}"))),
        }
    }

    let program = program.ok_or_else(|| Error::Usage(format!("no program given\n{USAGE}")))?;
    Ok(Args {
        session_id: session_id.ok_or_else(|| Error::Usage("--session is required".into()))?,
        out_dir: out_dir.ok_or_else(|| Error::Usage("--out-dir is required".into()))?,
        program,
        argv,
        poll_us,
        post_exit_ms,
        signal_after_ms,
        signal,
        cleanup_kill,
        print_procs,
    })
}

fn next(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Error> {
    it.next()
        .ok_or_else(|| Error::Usage(format!("{flag} requires a value")))
}

fn parse_signal(name: &str) -> Result<nix::sys::signal::Signal, Error> {
    use nix::sys::signal::Signal;
    match name.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::SIGINT),
        "KILL" | "SIGKILL" => Ok(Signal::SIGKILL),
        "TERM" | "SIGTERM" => Ok(Signal::SIGTERM),
        "HUP" | "SIGHUP" => Ok(Signal::SIGHUP),
        "QUIT" | "SIGQUIT" => Ok(Signal::SIGQUIT),
        other => Err(Error::Usage(format!("unsupported signal `{other}`"))),
    }
}

fn main() {
    match run() {
        Ok(code) => {
            // The supervisor's own exit status mirrors the script's, so the
            // harness can assert on it with a plain `$?`.
            std::process::exit(code);
        }
        Err(e) => {
            eprintln!("li-run-proto: {e}");
            std::process::exit(64);
        }
    }
}

fn run() -> Result<i32, Error> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.out_dir).map_err(|e| Error::io(args.out_dir.clone(), e))?;

    let stdout_path = args.out_dir.join("script-stdout.txt");
    let stderr_path = args.out_dir.join("script-stderr.txt");
    let stdout_file =
        std::fs::File::create(&stdout_path).map_err(|e| Error::io(stdout_path.clone(), e))?;
    let stderr_file =
        std::fs::File::create(&stderr_path).map_err(|e| Error::io(stderr_path.clone(), e))?;

    let cwd = std::env::current_dir().map_err(Error::PlainIo)?;
    let snapshot_before = procfs::snapshot();
    let t_start = now_mono_ns();

    let mut cmd = Command::new(&args.program);
    cmd.args(&args.argv)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .env("LI_SESSION_ID", &args.session_id)
        .env("LC_ALL", "C")
        .env("LANG", "C");

    // SAFETY: `setsid` is async-signal-safe and touches no memory shared with
    // the parent. This is the only way to place the child in a new session
    // between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the supervisor's session and process group so that
            // (a) the script gets a clean signal group and (b) session id
            // becomes a reliable lineage marker.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| Error::Usage(format!("cannot exec `{}`: {e}", args.program)))?;
    let pid = child.id() as i32;
    let t_spawned = now_mono_ns();

    let info = procfs::read_one(pid);
    // Keep the spawn-time leader record: after `wait4` the process is gone and
    // /proc/<pid> can no longer be read.
    let leader_at_spawn = info.clone();
    let proc_starttime = info.as_ref().map(|i| i.starttime_ticks);
    let pgid = info.as_ref().map_or(pid, |i| i.pgid);
    let sid = info.as_ref().map_or(pid, |i| i.sid);
    let child_cwd = procfs::readlink_or_none(&format!("/proc/{pid}/cwd"));
    let child_exe = procfs::readlink_or_none(&format!("/proc/{pid}/exe"));
    let child_cmdline = cmdline_of(pid);

    let started = Event::new(1, &args.session_id, "ScriptStarted", "observed").with_payload(
        serde_json::json!({
            "program": args.program,
            "argv": args.argv,
            "cwd_at_start": cwd,
            "child_cwd_from_proc": child_cwd,
            "child_exe_from_proc": child_exe,
            "child_cmdline_from_proc": child_cmdline,
            "pid": pid,
            "pgid": pgid,
            "sid": sid,
            "proc_starttime_ticks": proc_starttime,
            "fork_to_exec_observed_ns": t_spawned.saturating_sub(t_start),
            "spawn_mono_ns": t_spawned,
            "capabilities": {
                "process_identity": true,
                "exit_status": true,
                "duration": true,
                "descendant_processes": "partial:sampled",
                "internal_script_commands": false,
                "builtins": false,
                "redirections": false,
                "output_attribution": false
            }
        }),
    );
    emit(&started)?;

    let sample = Sampler::start(pid, args.poll_us);

    // Optional scheduled signal injection into the script's process group.
    let mut injected: Vec<serde_json::Value> = Vec::new();
    if let Some(sig) = args.signal {
        let ms = args.signal_after_ms;
        std::thread::sleep(Duration::from_millis(ms));
        let t = now_mono_ns();
        let res = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), sig);
        injected.push(serde_json::json!({
            "signal": format!("{sig:?}"),
            "target_pgid": pgid,
            "after_ms": ms,
            "sent_mono_ns": t,
            "result": match &res {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            }
        }));
    }

    let (status, rusage_json) = wait4_status(pid)?;
    let t_exit = now_mono_ns();

    if args.post_exit_ms > 0 {
        std::thread::sleep(Duration::from_millis(args.post_exit_ms));
    }
    let (mut procs, sample_stats) = sample.finish(pid);

    // If the script was shorter than one sampling interval, the sampler never
    // saw even the leader. The leader is nonetheless an `observed` fact: we
    // forked it ourselves and we reaped it. Record it from parent-side data and
    // mark how it was obtained so the report can tell the two apart.
    //
    // NOTE: the info used here was captured immediately after spawn, *before*
    // `wait4` reaped the child. Re-reading /proc here would always fail for a
    // short script, because the zombie is already gone.
    if !procs.iter().any(|p| p.pid == pid) {
        if let Some(info) = leader_at_spawn.clone() {
            procs.insert(
                0,
                sampler::SeenProcess {
                    pid,
                    ppid_first: info.ppid,
                    pgid_first: info.pgid,
                    sid: info.sid,
                    comm_first: info.comm.clone(),
                    comm_last: info.comm.clone(),
                    exe_first: child_exe.clone(),
                    exe_last: child_exe.clone(),
                    cmdline_first: child_cmdline.clone(),
                    state_last: info.state,
                    tty_nr_last: info.tty_nr,
                    utime_ticks_last: info.utime_ticks,
                    stime_ticks_last: info.stime_ticks,
                    samples: 0,
                    first_seen_mono_ns: t_spawned,
                    last_seen_mono_ns: t_exit,
                    alive_after_exit: false,
                    observation_source: "leader_record",
                },
            );
        }
    }
    let snapshot_after = procfs::snapshot();
    let appeared = procfs::appeared(&snapshot_before, &snapshot_after);

    let out_bytes = std::fs::metadata(&stdout_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let err_bytes = std::fs::metadata(&stderr_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let (out_lines, out_first_line, out_crlf) = count_lines(&stdout_path);

    let (exit_code, term_signal, core_dumped) = match &status {
        ExitKind::Exited(c) => (Some(*c), None, false),
        ExitKind::Signaled(s, core) => (None, Some(signal_name(*s)), *core),
        ExitKind::Other => (None, None, false),
    };

    let mut survivors: Vec<i32> = procs
        .iter()
        .filter(|p| p.alive_after_exit)
        .map(|p| p.pid)
        .collect();
    survivors.sort_unstable();

    let mut cleanup: Vec<serde_json::Value> = Vec::new();
    if args.cleanup_kill {
        let me = std::process::id() as i32;
        for p in &procs {
            if !p.alive_after_exit || p.pid == me {
                continue;
            }
            let r = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p.pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            cleanup.push(serde_json::json!({
                "pid": p.pid,
                "comm": p.comm_last,
                "result": match r { Ok(()) => "killed".to_string(), Err(e) => format!("error: {e}") }
            }));
        }
    }

    let duration_ns = t_exit.saturating_sub(t_spawned);
    let proc_list: Vec<serde_json::Value> = procs
        .iter()
        .map(|p| {
            serde_json::json!({
                "pid": p.pid,
                "ppid": p.ppid_first,
                "pgid_at_first_seen": p.pgid_first,
                "sid": p.sid,
                "comm_first": p.comm_first,
                "comm_last": p.comm_last,
                "exe_first": p.exe_first,
                "exe_last": p.exe_last,
                "cmdline_first": p.cmdline_first,
                "state_last": p.state_last.to_string(),
                "tty_nr_last": p.tty_nr_last,
                "utime_ticks_last": p.utime_ticks_last,
                "stime_ticks_last": p.stime_ticks_last,
                "samples": p.samples,
                "first_seen_mono_ns": p.first_seen_mono_ns,
                "last_seen_mono_ns": p.last_seen_mono_ns,
                "lifetime_observed_ns": p.last_seen_mono_ns.saturating_sub(p.first_seen_mono_ns),
                "alive_after_leader_exit": p.alive_after_exit,
                "observation_source": p.observation_source,
                "evidence_status": "observed"
            })
        })
        .collect();

    let finished = Event::new(2, &args.session_id, "ScriptFinished", "observed").with_payload(
        serde_json::json!({
            "program": args.program,
            "argv": args.argv,
            "pid": pid,
            "pgid": pgid,
            "sid": sid,
            "exit_code": exit_code,
            "term_signal": term_signal,
            "core_dumped": core_dumped,
            "duration_ns": duration_ns,
            "duration_ms": duration_ns as f64 / 1.0e6,
            "start_mono_ns": t_start,
            "spawn_mono_ns": t_spawned,
            "exit_mono_ns": t_exit,
            "rusage": rusage_json,
            "io": {
                "stdout_bytes": out_bytes,
                "stderr_bytes": err_bytes,
                "stdout_lines": out_lines,
                "stdout_first_line": out_first_line,
                "stdout_has_cr": out_crlf,
                "attribution": "aggregate stream only; no per-command attribution"
            },
            "injected_signals": injected,
            "processes": proc_list,
            "process_count": proc_list.len(),
            "sampling": {
                "interval_us": args.poll_us,
                "reads": sample_stats.reads,
                "mean_read_ns": sample_stats.total_read_ns.checked_div(sample_stats.reads).unwrap_or(0),
                "max_read_ns": sample_stats.max_read_ns,
                "max_concurrent_session_members": sample_stats.max_concurrent,
                "post_exit_ms": args.post_exit_ms
            },
            "survivors_after_exit": survivors,
            "cleanup_kill": cleanup,
            "snapshot_diff": {
                "method": "full /proc table diff, pid-keyed, pre-spawn vs post-exit",
                "appeared_count": appeared.len(),
                "appeared": appeared.iter().map(|a| serde_json::json!({
                    "pid": a.pid, "ppid": a.ppid, "comm": a.comm, "sid": a.sid
                })).collect::<Vec<_>>()
            }
        }),
    );
    emit(&finished)?;

    if args.print_procs {
        let v = serde_json::json!({ "session_id": args.session_id, "processes": proc_list });
        println!("{}", serde_json::to_string(&v)?);
    }
    match &status {
        ExitKind::Exited(c) => Ok(*c),
        // Mirror shell convention: 128 + signal number. The harness records
        // the real fact (signal) from the JSON, not from this code.
        ExitKind::Signaled(s, _) => Ok(128 + s),
        ExitKind::Other => Ok(70),
    }
}

fn emit(e: &Event) -> Result<(), Error> {
    let line = serde_json::to_string(&e.to_json())?;
    let mut out = std::io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

enum ExitKind {
    Exited(i32),
    Signaled(i32, bool),
    Other,
}

fn signal_name(sig: i32) -> String {
    let names = [
        (1, "SIGHUP"),
        (2, "SIGINT"),
        (3, "SIGQUIT"),
        (4, "SIGILL"),
        (6, "SIGABRT"),
        (8, "SIGFPE"),
        (9, "SIGKILL"),
        (11, "SIGSEGV"),
        (13, "SIGPIPE"),
        (14, "SIGALRM"),
        (15, "SIGTERM"),
        (24, "SIGXCPU"),
        (25, "SIGXFSZ"),
    ];
    match names.iter().find(|(n, _)| *n == sig) {
        Some((_, s)) => format!("{s} ({sig})"),
        None => format!("signal {sig}"),
    }
}

/// `wait4` is used instead of `Child::wait` because it also returns the
/// child's `rusage` in the same syscall (no extra getrusage race, and it
/// describes the reaped child rather than whatever is current).
fn wait4_status(pid: i32) -> Result<(ExitKind, serde_json::Value), Error> {
    let mut status: libc::c_int = 0;
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `status` and `rusage` are valid, owned, properly sized locals.
    let r = unsafe { libc::wait4(pid, &mut status, 0, ru.as_mut_ptr()) };
    if r < 0 {
        return Err(Error::Sys {
            call: "wait4",
            source: nix::errno::Errno::last(),
        });
    }
    // SAFETY: wait4 succeeded, so it initialised the rusage struct.
    let ru = unsafe { ru.assume_init() };
    let rusage = serde_json::json!({
        "user_s": ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 / 1.0e6,
        "sys_s": ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 / 1.0e6,
        "maxrss_kb": ru.ru_maxrss,
        "minflt": ru.ru_minflt,
        "majflt": ru.ru_majflt,
        "nvcsw": ru.ru_nvcsw,
        "nivcsw": ru.ru_nivcsw,
        "note": "covers the reaped child only; descendants that outlive it are not included"
    });
    let kind = if libc::WIFEXITED(status) {
        ExitKind::Exited(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        ExitKind::Signaled(libc::WTERMSIG(status), libc::WCOREDUMP(status))
    } else {
        ExitKind::Other
    };
    Ok((kind, rusage))
}

fn count_lines(path: &Path) -> (u64, Option<String>, bool) {
    let Ok(bytes) = std::fs::read(path) else {
        return (0, None, false);
    };
    let has_cr = bytes.contains(&b'\r');
    let lines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    let first = bytes
        .split(|b| *b == b'\n')
        .next()
        .map(|s| String::from_utf8_lossy(s).chars().take(120).collect())
        .filter(|s: &String| !s.is_empty());
    (lines, first, has_cr)
}
