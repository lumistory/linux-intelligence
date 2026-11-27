// End-to-end properties of `li-hook` and the three adapters.
// Nothing is mocked: C1, C1/C2 and C10/P1-09 are syscall-boundary properties, and
// the shells run in a real pty as 0-A ran them (fish will not install hooks off
// one). One finding is pinned here rather than assumed: `max(sequence)+1-count`
// and only the sender's `dropped` covers it - which is why 0-C §8 point 7 asks

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

// Unique per call: the tests run in parallel and would share `in.txt`.
fn scratch(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "li-hook-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// A one-shot daemon. `read_frames` / `close_after` simulate a stalled and a dead
// peer, which is cheaper than `SIGSTOP` and needs no signal rights.
struct FakeDaemon {
    path: PathBuf,
    lines: Receiver<String>,
}

impl FakeDaemon {
    fn start(tag: &str, read_frames: Option<usize>, close_after: bool) -> Self {
        let path = scratch(tag).join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            let mut n = 0;
            loop {
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if buf.trim().is_empty() {
                    continue;
                }
                let _ = tx.send(buf.trim_end().to_string());
                n += 1;
                if read_frames.is_some_and(|lim| n >= lim) {
                    // Stay alive but stop reading: the peer's writes then fill the
                    // socket buffer and time out, instead of being reset.
                    if !close_after {
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    return;
                }
            }
        });
        FakeDaemon { path, lines }
    }

    fn collect(&self, n: usize) -> Vec<String> {
        let started = Instant::now();
        let mut got = Vec::new();
        while got.len() < n {
            let left = Duration::from_secs(5).saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match self.lines.recv_timeout(left) {
                Ok(l) => got.push(l),
                Err(_) => break,
            }
        }
        got
    }
}

fn counter(report: &str, key: &str) -> u64 {
    report
        .split_whitespace()
        .find_map(|t| t.strip_prefix(&format!("{key}="))?.parse().ok())
        .unwrap_or_else(|| panic!("no `{key}` in: {report}"))
}

fn sequences(frames: &[String]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|l| match li_proto::decode_client_message(l).ok()? {
            li_proto::ClientMessage::Event(e) => Some(e.sequence),
            li_proto::ClientMessage::Ping(_) | li_proto::ClientMessage::DropReport(_) => None,
        })
        .collect()
}

fn req(i: usize) -> String {
    format!(
        r#"{{"type":"CommandFinished","ts_wall":"2026-11-24T18:00:00.000Z","payload":{{"command":"echo {i}","exit_status":0}}}}"#
    )
}

fn run_hook(args: &[&str], stdin: &str) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_li-hook"))
        .args(args)
        .env("LI_HOOK_VERBOSE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let (code, err) = (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    (code, err)
}

fn shell_present(shell: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {shell}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// What the child gets on fd 3.
enum SinkSpec<'a> {
    // fd 3 closed: "no launcher, no sink".
    Closed,
    File(&'a PathBuf),
    // fd 3 to a pipe whose reader is then killed - 0-A's exact SIGPIPE setup
    // (`probe-bash-sigpipe.log`: `sig 13` killed interactive bash).
    DeadPipe,
}

// One interactive shell in a real pty, adapter sourced first. fd 3 is opened by a
// wrapping `sh`: that is the launcher role C10 assigns to `li run`, because 0-A
fn run_shell(
    shell: &str,
    agent: &str,
    sink: SinkSpec<'_>,
    commands: &str,
) -> (Option<i32>, String) {
    let dir = scratch(shell);
    let stdin_path = dir.join("in.txt");
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("shell")
        .join(shell)
        .join(agent);
    assert!(adapter.exists(), "{} must exist", adapter.display());
    std::fs::write(
        &stdin_path,
        format!("source {}\n{commands}\n", adapter.display()),
    )
    .unwrap();
    let shell_cmd = match shell {
        "bash" => "bash --norc -i",
        "zsh" => "zsh -f -i",
        "fish" => "fish --no-config -i",
        other => panic!("unknown shell {other}"),
    };
    // `exec` is a special builtin, so a failed redirect exits the wrapper; every
    // branch below must succeed.
    let redirect = match sink {
        SinkSpec::Closed => String::new(),
        SinkSpec::File(p) => format!("exec 3>{};", p.display()),
        SinkSpec::DeadPipe => format!(
            "mkfifo {f} 2>/dev/null; sleep 5 < {f} & r=$!; exec 3>{f}; kill $r 2>/dev/null; wait $r 2>/dev/null;",
            f = dir.join("dead.pipe").display()
        ),
    };
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{redirect} exec script -qec {shell_cmd:?} /dev/null < {}",
            stdin_path.display()
        ))
        // TERM=dumb: 0-A measured fish blocking on terminal queries in a bare pty
        // (probe-fish-term.log), which would hang this test.
        .env("TERM", "dumb")
        .env("LI_SINK_FD", "3")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn frames_in(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ------------------------------------------------------------------- C1/C2

#[test]
fn a_clean_stream_is_dense_and_gap_free() {
    let d = FakeDaemon::start("dense", None, false);
    let input: String = (0..32).map(req).collect::<Vec<_>>().join("\n");
    let (code, report) = run_hook(
        &[
            "--adapter=bash",
            "--session=s",
            "--sink=socket",
            &format!("--socket={}", d.path.display()),
        ],
        &(input + "\n"),
    );
    assert_eq!(code, Some(0), "{report}");
    let got = d.collect(33); // 32 events + the probe Ping
    assert!(got.len() >= 32, "expected 32 frames, got {}", got.len());
    assert_eq!(sequences(&got), (0..32).collect::<Vec<u64>>());
    assert_eq!(counter(&report, "dropped"), 0);
}

#[test]
fn default_sink_uses_the_launchers_inherited_socket() {
    use std::os::fd::OwnedFd;

    let d = FakeDaemon::start("inherited", None, false);
    let client = UnixStream::connect(&d.path).unwrap();
    let (code, report) = run_hook_with_stdout(
        &["--adapter=bash", "--session=s"],
        &(req(0) + "\n"),
        Stdio::from(OwnedFd::from(client)),
    );
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(counter(&report, "delivered"), 1);
    let got = d.collect(2); // probe Ping + one event
    assert_eq!(sequences(&got), vec![0]);
}

fn run_hook_with_stdout(args: &[&str], stdin: &str, stdout: Stdio) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_li-hook"))
        .args(args)
        .env("LI_HOOK_VERBOSE", "1")
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_daemon_that_stops_or_dies_leaves_only_countable_loss() {
    // Two shapes of the same failure. 0-C T4: a plain blocking write against a
    // SIGSTOPed peer had not returned after 2 s, so both must finish fast.
    for (tag, read, close, n) in [
        ("stall", Some(3), false, 400usize),
        ("dies", Some(5), true, 60),
    ] {
        let d = FakeDaemon::start(tag, read, close);
        let input: String = (0..n).map(req).collect::<Vec<_>>().join("\n");
        let started = Instant::now();
        let (code, report) = run_hook(
            &[
                "--adapter=zsh",
                "--session=s",
                "--sink=socket",
                &format!("--socket={}", d.path.display()),
            ],
            &(input + "\n"),
        );
        assert_eq!(code, Some(0), "{report}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{tag}: C1 {:?}",
            started.elapsed()
        );

        let (a, dr, de) = (
            counter(&report, "accepted"),
            counter(&report, "dropped"),
            counter(&report, "delivered"),
        );
        assert_eq!(a, n as u64, "{tag}: every request took a number");
        assert_eq!(a, de + dr, "{tag}: nothing vanished uncounted");
        assert!(dr > 0, "{tag}: lost frames must be counted");

        // The receiver's formula sees a hole only when a *later* frame arrives, so
        // a contiguous prefix reports 0 and only `dropped` covers the tail. That is
        // the finding 0-C §8 point 7 asks a DropReport for, and it is asserted here
        // rather than described.
        let seqs = sequences(&d.collect(read.unwrap_or(usize::MAX)));
        let gap = seqs.iter().max().map_or(0, |&m| m + 1 - seqs.len() as u64);
        eprintln!(
            "{tag}: arrived={} receiver_gap={gap} dropped={dr}",
            seqs.len()
        );
        assert_eq!(
            gap, 0,
            "{tag}: a contiguous prefix shows no receiver-side hole"
        );
    }
}

#[test]
fn a_dead_sink_still_advances_the_sequence_for_every_frame() {
    let (code, r) = run_hook(
        &[
            "--adapter=bash",
            "--session=s",
            "--sink=none",
            "--queue-cap=1",
        ],
        &((0..5).map(req).collect::<Vec<_>>().join("\n") + "\n"),
    );
    assert_eq!(code, Some(0));
    assert_eq!((counter(&r, "accepted"), counter(&r, "delivered")), (5, 0));
    assert_eq!(counter(&r, "dropped"), 5, "each drop burns its number");
    assert_eq!(counter(&r, "receiver_gap"), 5);
}

#[test]
fn a_missing_daemon_is_fast_silent_and_fatal_to_nothing() {
    let started = Instant::now();
    let (code, stderr) = run_hook(
        &[
            "--adapter=bash",
            "--session=s",
            "--sink=socket",
            "--socket=/nonexistent/li-hook/daemon.sock",
        ],
        &(req(0) + "\n"),
    );
    assert_eq!(code, Some(0), "a hook must exit 0 with no daemon: {stderr}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    // "tried and could not" must be visible, or a receiver cannot tell it from
    // "nothing happened".
    assert!(
        stderr.contains("connect failed") && stderr.contains("staying silent"),
        "{stderr}"
    );

    // And a hook that was never configured correctly must also stay harmless.
    for args in [vec![], vec!["--adapter=nope"], vec!["--adapter=bash"]] {
        assert_eq!(
            run_hook(&args, "").0,
            Some(0),
            "{args:?} must not fail the shell"
        );
    }
}

// --------------------------------------------------------- P1-09 / C10

// It exited on its own, with the status asked for, having run both commands -
fn assert_survived(shell: &str, case: &str, want: i32, tag: &str, got: (Option<i32>, String)) {
    let (code, stdout) = got;
    assert_eq!(
        code,
        Some(want),
        "{shell} ({case}): status lost; stdout: {stdout}"
    );
    for n in ["ONE", "TWO"] {
        assert!(
            stdout.contains(&format!("{tag}-{n}")),
            "{shell} ({case}): `{tag}-{n}` missing; stdout: {stdout}"
        );
    }
}

fn survives(shell: &str, adapter: &str) {
    if !shell_present(shell) {
        eprintln!("NOT MEASURED: {shell} dead-sink survival: not installed on this host");
        return;
    }
    // A: no sink at all - the load-time probe must latch emission off.
    assert_survived(
        shell,
        "no sink",
        7,
        "SURV",
        run_shell(
            shell,
            adapter,
            SinkSpec::Closed,
            "echo SURV-ONE\nfalse\necho SURV-TWO\nexit 7",
        ),
    );
    // B: the sink opens, its reader then dies - 0-A's setup. The probe succeeds,
    // so a write really is attempted and really meets EPIPE.
    assert_survived(
        shell,
        "dead pipe",
        9,
        "DEAD",
        run_shell(
            shell,
            adapter,
            SinkSpec::DeadPipe,
            "echo DEAD-ONE\nfalse\necho DEAD-TWO\nexit 9",
        ),
    );
}

/// One test over all three shells rather than three wrappers: the failure this
/// guards (a dead sink killing an interactive shell) is the same property, and
/// the only per-shell difference is the adapter path.
#[test]
fn every_installed_shell_survives_a_dead_sink() {
    for (shell, adapter) in [
        ("bash", "bash.sh"),
        ("zsh", "zsh.zsh"),
        ("fish", "fish.fish"),
    ] {
        survives(shell, adapter);
    }
}

#[test]
fn an_adapter_with_a_working_sink_emits_valid_frames() {
    // Positive control: same harness, live sink. A suite where everything
    // "survives" because nothing works proves nothing.
    if !shell_present("bash") {
        eprintln!("NOT MEASURED: bash frame emission: not installed");
        return;
    }
    let sink = scratch("sink").join("f.jsonl");
    let (code, stdout) = run_shell(
        "bash",
        "bash.sh",
        SinkSpec::File(&sink),
        "echo LI-POS\nfalse",
    );
    // `false` is last, so 1 is correct: it exited on its own and still delivered.
    assert_eq!(code, Some(1), "stdout: {stdout}");
    let frames = frames_in(&sink);
    assert!(
        frames.len() >= 2,
        "expected >=2 frames, got {}; stdout: {stdout}",
        frames.len()
    );
    let cmds: Vec<&str> = frames
        .iter()
        .filter_map(|f| f["payload"]["command"].as_str())
        .collect();
    assert!(cmds.contains(&"echo LI-POS"), "commands seen: {cmds:?}");
    for f in &frames {
        assert_eq!(f["payload"]["shell"], "bash");
        assert!(f["payload"]["pid"].is_number(), "pid must be numeric: {f}");
        assert!(f["payload"]["sid"].is_number(), "sid must be numeric: {f}");
    }
}

// ------------------------------------------------- the reduced 13-case matrix

// The four cases this harness cannot assert honestly. Reported, not dropped.
const NOT_MEASURED: &[&str] = &[
    "t02 backslash continuation (needs a multi-line driver)",
    "t08 background job (0-A: completion is never observed, by design)",
    "t11 shell SIGKILLed (needs an external killer and a pty transcript)",
    "t12 large output 20 000 lines (a throughput measure, not a hook property)",
];

// `(name, bash/zsh command, fish command, expected status)`. Two entries differ per
// shell because the *language* does, as 0-A also recorded (t04 "and/or", t09
type Case = (&'static str, &'static str, &'static str, i64);

fn cases_for(fish: bool) -> Vec<Case> {
    vec![
        ("t01 normal", "true", "true", 0),
        ("t03 pipeline", "echo a | wc -l", "echo a | wc -l", 0),
        (
            "t04 and-or",
            "true && true; false || true",
            if fish {
                "true; and echo ok; or echo no"
            } else {
                "true && true; false || true"
            },
            0,
        ),
        ("t05 builtin cd", "cd /tmp", "cd /tmp", 0),
        (
            "t06 function",
            "f() { echo fn; }; f",
            if fish {
                "function g; echo fn; end; g"
            } else {
                "f() { echo fn; }; f"
            },
            0,
        ),
        (
            "t07 alias-shaped",
            "echo alias_target",
            "echo alias_target",
            0,
        ),
        (
            "t09 subshell",
            "( echo s1; echo s2 )",
            if fish {
                "begin; echo s1; echo s2; end"
            } else {
                "( echo s1; echo s2 )"
            },
            0,
        ),
        (
            "t10 status 130",
            "sh -c 'exit 130'",
            "sh -c 'exit 130'",
            130,
        ),
        (
            "t13 five sequential",
            "for i in 1 2 3 4 5; do echo $i; done",
            if fish {
                "for i in 1 2 3 4 5; echo $i; end"
            } else {
                "for i in 1 2 3 4 5; do echo $i; done"
            },
            0,
        ),
    ]
}

fn matrix_for(shell: &str, adapter: &str) {
    let fish = shell == "fish";
    let cases = cases_for(fish);
    if !shell_present(shell) {
        for (case, ..) in &cases {
            eprintln!("NOT MEASURED: {shell} / {case}: not installed on this host");
        }
        return;
    }
    for c in NOT_MEASURED {
        eprintln!("NOT MEASURED: {shell} / {c}");
    }
    // One session, every case in sequence: how 0-A drove them, and it also proves
    // the hook survives a long session rather than one command.
    let script: String = cases
        .iter()
        .map(|(_, bz, f, _)| {
            format!(
                "{}\necho LI-ST-{}\n",
                if fish { f } else { bz },
                if fish { "$status" } else { "$?" }
            )
        })
        .collect();
    let sink = scratch("matrix").join(format!("{shell}.jsonl"));
    let (code, stdout) = run_shell(shell, adapter, SinkSpec::File(&sink), &script);
    assert!(
        code.is_some(),
        "{shell}: shell did not exit cleanly; stdout: {stdout}"
    );
    let frames = frames_in(&sink);
    assert!(
        !frames.is_empty(),
        "{shell}: no frames at all; stdout: {stdout}"
    );
    let statuses: Vec<i64> = frames
        .iter()
        .filter_map(|f| f["payload"]["exit_status"].as_i64())
        .collect();
    for (case, .., want) in &cases {
        // A non-zero status is distinctive, so its absence proves the hook missed a
        // command rather than merely reporting it differently.
        if *want != 0 {
            assert!(
                statuses.contains(want),
                "{shell} / {case}: no frame with status {want}; saw {statuses:?}"
            );
        }
    }
    for f in &frames {
        assert!(
            f["payload"]["cwd"].is_string(),
            "{shell}: frame without cwd: {f}"
        );
        assert!(
            f["payload"]["pid"].is_number(),
            "{shell}: frame without pid: {f}"
        );
        if f["type"] == "CommandFinished" {
            assert!(
                f["payload"]["exit_status"].is_i64(),
                "{shell}: no numeric status: {f}"
            );
        }
    }
    assert!(
        frames.len() >= cases.len(),
        "{shell}: {} frames for {} cases",
        frames.len(),
        cases.len()
    );
}

#[test]
fn the_reduced_case_matrix_on_every_installed_shell() {
    for (shell, adapter) in [
        ("bash", "bash.sh"),
        ("zsh", "zsh.zsh"),
        ("fish", "fish.fish"),
    ] {
        matrix_for(shell, adapter);
    }
}

// ---------------------------------------------------------------- overhead

// Adapter cost per command, as a **difference**: same script, same pty, with and
// without the adapter. Not comparable to 0-A §4.2's body-only figures (bash 3 us,
// zsh 2348 us, fish 1085 us): this includes the pty round trip, which 0-A §6 warns
#[test]
fn the_adapter_cost_per_command_is_measured_and_reported() {
    for (shell, adapter) in [
        ("bash", "bash.sh"),
        ("zsh", "zsh.zsh"),
        ("fish", "fish.fish"),
    ] {
        if !shell_present(shell) {
            eprintln!("NOT MEASURED: {shell} adapter overhead: not installed");
            continue;
        }
        let n = 30;
        let body: String = (0..n).map(|i| format!("echo c{i}\n")).collect();
        let script = format!("echo LI-NOW-$(date +%s%N)\n{body}echo LI-NOW-$(date +%s%N)\n");
        let time_for = |with_adapter: bool| -> Option<u128> {
            let sink = scratch("cost").join("s.jsonl");
            let (_, stdout) = if with_adapter {
                run_shell(shell, adapter, SinkSpec::File(&sink), &script)
            } else {
                run_bare_shell(shell, &script)
            };
            let stamps: Vec<u128> = stdout
                .split("LI-NOW-")
                .skip(1)
                .filter_map(|t| t.lines().next()?.trim().parse().ok())
                .collect();
            (stamps.len() >= 2).then(|| stamps[1].saturating_sub(stamps[0]) / n as u128)
        };
        match (time_for(true), time_for(false)) {
            (Some(w), Some(b)) => eprintln!(
                "adapter cost [{shell}]: {w} ns/cmd with adapter, {b} without (delta {} ns, n={n}, sink=file)",
                w as i128 - b as i128
            ),
            _ => eprintln!("PARTIALLY MEASURED: {shell} adapter cost"),
        }
    }
}

fn run_bare_shell(shell: &str, commands: &str) -> (Option<i32>, String) {
    let stdin_path = scratch("bare").join("in.txt");
    std::fs::write(&stdin_path, format!("{commands}\nexit\n")).unwrap();
    let shell_cmd = match shell {
        "bash" => "bash --norc -i",
        "zsh" => "zsh -f -i",
        "fish" => "fish --no-config -i",
        other => panic!("unknown shell {other}"),
    };
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "exec script -qec {shell_cmd:?} /dev/null < {}",
            stdin_path.display()
        ))
        .env("TERM", "dumb")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}
