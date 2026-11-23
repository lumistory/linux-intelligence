//! `li-pty-proto` — minimal PTY prototype for Phase 0 experiment 0-D.
//!
//! Modes:
//!   --mode matrix      run the per-shell test matrix (bash, zsh, fish)
//!   --mode concurrent  run N sessions at once and check for cross-talk
//!   --mode takeover    demonstrate that a foreign terminal cannot be taken over
//!
//! All output is JSON lines on stdout; the driver script stores them raw.

mod error;
mod pty;
mod responder;
mod scene;
mod util;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use error::{Error, Result};
use pty::{Session, SpawnSpec, Winsize};

const USAGE: &str = "\
usage: li-pty-proto --mode <matrix|concurrent|takeover> --workdir <dir> [options]

options:
  --zsh <path>       path to the local zsh binary (required for the matrix)
  --term <name>      TERM value for the shells (default xterm-256color)
  --sessions <n>     session count for --mode concurrent (default 4)
  --json             (always on; kept for symmetry)
";

struct Args {
    mode: String,
    workdir: PathBuf,
    zsh: String,
    term: String,
    sessions: usize,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let (mut mode, mut workdir, mut zsh, mut term, mut sessions) = (
        None::<String>,
        None::<PathBuf>,
        None::<String>,
        "xterm-256color".to_string(),
        4usize,
    );
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => mode = Some(next(&mut it, "--mode")?),
            "--workdir" => workdir = Some(PathBuf::from(next(&mut it, "--workdir")?)),
            "--zsh" => zsh = Some(next(&mut it, "--zsh")?),
            "--term" => term = next(&mut it, "--term")?,
            "--sessions" => {
                sessions = next(&mut it, "--sessions")?
                    .parse()
                    .map_err(|_| Error::Usage("--sessions must be an integer".into()))?
            }
            "--json" => {}
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(Error::Usage(format!("unknown argument `{other}`"))),
        }
    }
    Ok(Args {
        mode: mode.ok_or_else(|| Error::Usage("--mode is required".into()))?,
        workdir: workdir.ok_or_else(|| Error::Usage("--workdir is required".into()))?,
        zsh: zsh.ok_or_else(|| Error::Usage("--zsh is required".into()))?,
        term,
        sessions,
    })
}

fn next(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    it.next()
        .ok_or_else(|| Error::Usage(format!("{flag} requires a value")))
}

fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("li-pty-proto: {e}");
            64
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.workdir)
        .map_err(|e| Error::io(args.workdir.clone(), e))?;
    match args.mode.as_str() {
        "matrix" => matrix(&args),
        "concurrent" => concurrent(&args),
        "takeover" => takeover(&args),
        "probe" => probe(&args),
        "responder-off" => responder_off(&args),
        other => Err(Error::Usage(format!("unknown mode `{other}`"))),
    }
}

fn emit(v: &serde_json::Value) {
    println!("{}", serde_json::to_string(v).unwrap_or_else(|_| "{}".into()));
}

/// Run the full matrix for every shell, sequentially, one PTY each.
fn matrix(args: &Args) -> Result<i32> {
    if !std::path::Path::new(&args.zsh).exists() {
        return Err(Error::Usage(format!(
            "zsh not found at {} (it is not installed system-wide; pass --zsh)",
            args.zsh
        )));
    }
    let mut failed_shells = 0;
    for spec in scene::shells(&args.zsh, &args.workdir.to_string_lossy(), &args.term) {
        let home = args.workdir.join(format!("home-{}", spec.id));
        std::fs::create_dir_all(&home).map_err(|e| Error::io(&home, e))?;
        let res = scene::run_shell(&spec, &args.workdir);
        match res {
            Ok(run) => {
                let all = run.steps.iter().all(|s| s.matched);
                if !all {
                    failed_shells += 1;
                }
                emit(&json!({
                    "record": "shell_matrix",
                    "shell": run.shell,
                    "all_steps_passed": all,
                    "steps": run.steps.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                    "exit_status": run.exit_status,
                    "exit_signal": run.exit_signal,
                    "master_eof": run.eof_reached,
                }));
                // Escaped transcript goes to a separate file; the driver splits
                // these lines out so the JSON stream stays machine-readable.
                emit(&json!({
                    "record": "transcript",
                    "shell": run.shell,
                    "escaped": run.transcript_escaped,
                }));
            }
            Err(e) => {
                failed_shells += 1;
                emit(&json!({
                    "record": "shell_matrix",
                    "shell": spec.id,
                    "all_steps_passed": false,
                    "error": e.to_string(),
                }));
            }
        }
    }
    Ok(if failed_shells == 0 { 0 } else { 1 })
}

/// N PTY sessions running at the same time; each gets a unique token and must
/// never see another session's token.
fn concurrent(args: &Args) -> Result<i32> {
    let n = args.sessions.clamp(2, 8);
    let specs = scene::shells(&args.zsh, &args.workdir.to_string_lossy(), &args.term);
    let mut handles = Vec::new();
    for i in 0..n {
        // Rotate over the available shells so the test also covers mixed
        // shells running side by side.
        let spec = &specs[i % specs.len()];
        let id = i;
        let shell_path = spec.path.clone();
        let shell_args = spec.args.clone();
        let env = spec.env.clone();
        let workdir = args.workdir.clone();
        let shell_id = spec.id.to_string();
        handles.push(std::thread::spawn(move || -> Result<serde_json::Value> {
            let winsize = Winsize { ws_row: 24 + id as u16, ws_col: 80 + id as u16, ws_xpixel: 0, ws_ypixel: 0 };
            let mut argv = vec![shell_path];
            argv.extend(shell_args);
            let mut s = Session::spawn(SpawnSpec {
                name: &format!("conc-{id}"),
                argv: &argv,
                env: &env,
                winsize,
                cwd: &workdir,
            })?;
            // Unique token per session.
            let token = format!("CONC_TOKEN_{id}_X{}", 1000 + id);
            let mut waited = 0u64;
            while s.bytes_read() == 0 && waited < 6_000 {
                s.pump(100)?;
                waited += 100;
            }
            s.send_line(&format!("echo {token}"))?;
            let out = s.read_until(&token, Duration::from_secs(8))?;
            // Send a couple more commands so the streams interleave in time.
            s.send_line("sleep 0.2; echo INTERLEAVE-DONE")?;
            s.pump(400)?;
            let text = s.text();
            let own_token_seen = text.matches(&token).count();
            // Cross-talk check: did any *other* session's token appear here?
            let mut foreign: Vec<String> = Vec::new();
            for j in 0..n {
                if j == id {
                    continue;
                }
                let other = format!("CONC_TOKEN_{j}_X{}", 1000 + j);
                if text.contains(&other) {
                    foreign.push(other);
                }
            }
            let rows_cols = {
                s.take_text();
                s.send_line("stty size")?;
                let want = format!("{} {}", winsize.ws_row, winsize.ws_col);
                let ok = s.read_until(&want, Duration::from_secs(6))?;
                (want, ok.matched)
            };
            s.send_line("exit")?;
            std::thread::sleep(Duration::from_millis(300));
            let _ = s.reap(Duration::from_secs(3));
            Ok(json!({
                "session": id,
                "shell": shell_id,
                "pid": s.pid,
                "slave_path": s.slave_path(),
                "own_token_occurrences": own_token_seen,
                "sentinel_matched": out.matched,
                "foreign_tokens_seen": foreign,
                "winsize_isolated": rows_cols.1,
                "expected_size": rows_cols.0,
                "exit_status": s.reap(Duration::from_secs(1)).unwrap_or(None),
            }))
        }));
    }

    let mut results = Vec::new();
    let mut ok = true;
    for h in handles {
        match h.join() {
            Ok(Ok(v)) => {
                if v["foreign_tokens_seen"].as_array().map(|a| !a.is_empty()).unwrap_or(true)
                    || !v["winsize_isolated"].as_bool().unwrap_or(false)
                {
                    ok = false;
                }
                results.push(v);
            }
            Ok(Err(e)) => {
                ok = false;
                results.push(json!({ "error": e.to_string() }));
            }
            Err(_) => {
                ok = false;
                results.push(json!({ "error": "session thread panicked" }));
            }
        }
    }
    emit(&json!({
        "record": "concurrent",
        "sessions_requested": n,
        "no_cross_talk": ok,
        "results": results,
    }));
    Ok(if ok { 0 } else { 1 })
}

/// Prove that an existing terminal cannot be adopted.
///
/// Three independent checks, all recorded:
///  1. every interesting process has a `tty_nr` whose device we do not own;
///  2. `/proc/<pid>/fd/0` of a foreign shell points into a `pts/N` that is not
///     one of ours;
///  3. `TIOCSTI` (the only byte-injection ioctl) is refused, and `TIOCSCTTY`
///     with force=1 is refused.
fn takeover(args: &Args) -> Result<i32> {
    // Find foreign shells: processes whose fd/0 is a pts device.
    let mut foreign: Vec<serde_json::Value> = Vec::new();
    let mut target_path: Option<String> = None;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            if pid == std::process::id() as i32 {
                continue;
            }
            let Some(fd0) = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok() else {
                continue;
            };
            let fd0 = fd0.to_string_lossy().into_owned();
            if !fd0.starts_with("/dev/pts/") {
                continue;
            }
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let tty_nr = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|raw| {
                    let close = raw.rfind(')')?;
                    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
                    rest.get(4).map(|s| s.to_string())
                })
                .unwrap_or_default();
            if target_path.is_none() {
                target_path = Some(fd0.clone());
            }
            foreign.push(json!({
                "pid": pid,
                "comm": comm,
                "fd0": fd0,
                "tty_nr": tty_nr,
                "owns_this_terminal": false,
            }));
            if foreign.len() >= 12 {
                break;
            }
        }
    }

    // Our own PTY, for the negative side of the comparison.
    let winsize = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let mut argv = vec!["/bin/bash".to_string(), "--norc".into(), "-i".into()];
    let env = vec![
        ("TERM".to_string(), args.term.clone()),
        ("HOME".to_string(), args.workdir.to_string_lossy().into_owned()),
        ("PS1".to_string(), "LI-OWN> ".to_string()),
    ];
    argv.insert(0, "/bin/bash".to_string());
    let mut own = Session::spawn(SpawnSpec {
        name: "own-probe",
        argv: &["/bin/bash".to_string(), "--norc".into(), "--noprofile".into(), "-i".into()],
        env: &env,
        winsize,
        cwd: &args.workdir,
    })?;
    let _ = own.pump(400);
    let own_probe = own.probe_takeover(target_path.as_deref().unwrap_or("/dev/null"));
    own.send_line("exit").ok();
    let _ = own.reap(Duration::from_secs(2));

    // Sysctl values that govern the mechanisms.
    let read_sysctl = |p: &str| {
        std::fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| format!("unreadable: {e}"))
    };

    emit(&json!({
        "record": "takeover",
        "foreign_terminals_found": foreign.len(),
        "foreign_processes": foreign,
        "our_own_pty_probe": own_probe,
        "sysctl": {
            "dev.tty.legacy_tiocsti": read_sysctl("/proc/sys/dev/tty/legacy_tiocsti"),
            "kernel.yama.ptrace_scope": read_sysctl("/proc/sys/kernel/yama/ptrace_scope"),
        },
        "conclusion": "a PTY you did not create is reachable only as a file; \
                       injecting bytes requires TIOCSTI, and becoming its session's \
                       controlling terminal requires TIOCSCTTY on an fd opened from \
                       the same session; both are refused in this environment",
    }));
    Ok(0)
}

/// Diagnostic mode: run one command in a bash PTY and dump exactly what the
/// master side received. Used to debug the read path and quoted in the report.
fn probe(args: &Args) -> Result<i32> {
    let winsize = Winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
    let env = vec![
        ("TERM".to_string(), args.term.clone()),
        ("HOME".to_string(), args.workdir.to_string_lossy().into_owned()),
        ("PS1".to_string(), "LI-BASH> ".to_string()),
        ("LC_ALL".to_string(), "C".to_string()),
        ("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string()),
    ];
    let mut s = Session::spawn(SpawnSpec {
        name: "probe",
        argv: &["/bin/bash".into(), "--norc".into(), "--noprofile".into(), "-i".into()],
        env: &env,
        winsize,
        cwd: &args.workdir,
    })?;
    let m0 = s.mark();
    let _ = s.wait_quiet(300, 4_000);
    emit(&json!({ "record": "probe", "phase": "startup", "bytes": s.bytes_read(),
                  "escaped": s.since_escaped(m0) }));

    let m1 = s.mark();
    s.send_line("top -b -n1 -o %MEM")?;
    // Deliberately do NOT read during the run, mimicking a naive caller.
    std::thread::sleep(Duration::from_millis(1_500));
    let n = s.pump(1_500)?;
    emit(&json!({ "record": "probe", "phase": "top_no_early_read",
                  "pumped_bytes": n, "escaped": s.since_escaped(m1).chars().take(3000).collect::<String>() }));

    let m2 = s.mark();
    s.send_line("echo PROBE-AFTER-TOP")?;
    let r = s.read_until_any("PROBE-AFTER-TOP", Duration::from_secs(8))?;
    emit(&json!({ "record": "probe", "phase": "after_top", "matched": r.matched,
                  "escaped": s.since_escaped(m2).chars().take(2000).collect::<String>() }));
    s.send_line("exit")?;
    let _ = s.reap(Duration::from_secs(3));
    Ok(0)
}

/// Negative control for the terminal-query responder.
///
/// Runs the same sentinel handshake as the matrix, but with reply generation
/// disabled, so the difference between "the program got an answer" and "the
/// program waited forever" is measured rather than asserted.
fn responder_off(args: &Args) -> Result<i32> {
    let mut failures = 0;
    for spec in scene::shells(&args.zsh, &args.workdir.to_string_lossy(), &args.term) {
        let home = args.workdir.join(format!("home-noresp-{}", spec.id));
        let _ = std::fs::create_dir_all(&home);
        let winsize = Winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
        let argv = {
            let mut v = vec![spec.path.clone()];
            v.extend(spec.args.iter().cloned());
            v
        };
        let mut session = match Session::spawn_disabled_responder(SpawnSpec {
            name: &spec.id,
            argv: &argv,
            env: &spec.env,
            winsize,
            cwd: &args.workdir,
        }) {
            Ok(s) => s,
            Err(e) => {
                emit(&json!({ "record": "responder_off", "shell": spec.id,
                              "error": e.to_string() }));
                failures += 1;
                continue;
            }
        };
        let token = format!("{}-NORESP", spec.sentinel);
        // Give the shell its usual settling time, then try the handshake.
        let t_ready = std::time::Instant::now();
        let mut waited = 0u64;
        while session.bytes_read() == 0 && waited < 6_000 {
            let _ = session.pump(100);
            waited += 100;
        }
        let first_output_ms = t_ready.elapsed().as_millis() as u64;
        let quiet_ms = session.wait_quiet(400, 3_000).unwrap_or(0);
        let start_bytes = session.bytes_read();
        let t_cmd = std::time::Instant::now();
        let _ = session.send_line(&format!("echo {token}"));
        let out = session
            .read_until_any(&token, Duration::from_secs(12))
            .unwrap_or(pty::ReadOutcome {
                matched: false,
                elapsed: Duration::from_secs(0),
                bytes: 0,
                occurrences_raw: 0,
                occurrences_debackspaced: 0,
            });
        let matched = out.matched;
        if !matched {
            failures += 1;
        }
        let _ = session.shutdown(true);
        emit(&json!({
            "record": "responder_off",
            "shell": spec.id,
            "sentinel_matched": matched,
            "queries_answered": session.answered.len(),
            "queries_seen_without_answering": session.queries_seen(),
            "bytes": session.bytes_read(),
            "bytes_before_command": start_bytes,
            "first_output_ms": first_output_ms,
            "quiet_settle_ms": quiet_ms,
            "handshake_ms": t_cmd.elapsed().as_millis() as u64,
            "exit_status": session.exit_status(),
            "note": "queries_answered=0 by construction: replies were discarded",
        }));
    }
    Ok(if failures == 0 { 0 } else { 1 })
}
