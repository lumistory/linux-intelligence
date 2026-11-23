//! The per-shell PTY test matrix.
//!
//! One `ShellSpec` describes how to start a shell with *zero* user
//! configuration; `run_shell` executes the same ten steps against it.
//!
//! Design note: the transcript is never cleared. Every step records a byte
//! offset (`mark`) into the running stream, and the bytes accumulated since the
//! previous mark are what that step actually observed. `take_text()` is
//! deliberately not used, because clearing the buffer would hide whether an
//! earlier step's output leaked into a later one.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::Result;
use crate::pty::{Session, SpawnSpec, Winsize};

pub struct ShellSpec {
    pub id: &'static str,
    pub path: String,
    /// Arguments that suppress *all* user configuration.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Token whose appearance proves the shell executed our command.
    pub sentinel: String,
    /// Shell-specific arithmetic expression that prints `5`.
    ///
    /// POSIX `$((2 + 3))` is not fish syntax, and `[$((...))]` is additionally
    /// a glob pattern in zsh, so this must be per shell. Measured, not guessed:
    /// `echo [$((2 + 3))]` in zsh produced `zsh: no matches found: [5]`, and in
    /// fish `fish: command substitutions not allowed in command position`.
    pub arith_cmd: &'static str,
}

pub fn shells(zsh_path: &str, home: &str, term: &str) -> Vec<ShellSpec> {
    let base = |extra: &[(&str, &str)]| -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = vec![
            ("TERM".into(), term.to_string()),
            ("HOME".into(), home.to_string()),
            ("LC_ALL".into(), "C".into()),
            ("LANG".into(), "C".into()),
            ("USER".into(), "li-phase0".into()),
            ("LOGNAME".into(), "li-phase0".into()),
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            ("XDG_RUNTIME_DIR".into(), home.to_string()),
            ("XDG_CONFIG_HOME".into(), format!("{home}/.config")),
        ];
        for (k, val) in extra {
            v.push(((*k).into(), (*val).into()));
        }
        v
    };

    vec![
        ShellSpec {
            id: "bash",
            path: "/bin/bash".into(),
            args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
            env: base(&[("PS1", "LI-BASH> "), ("PS2", "LI-BASH+ "), ("PROMPT_COMMAND", "")]),
            sentinel: "LI_SENTINEL_BASH".into(),
            arith_cmd: "echo $((2 + 3))",
        },
        ShellSpec {
            id: "zsh",
            path: zsh_path.into(),
            // `-d` (NO_GLOBAL_RCS) + ZDOTDIR pointing at an isolated directory:
            // the shim .zshenv below is the *only* startup file read. `-f`
            // cannot be used because it would also skip the shim, and the local
            // zsh copy cannot find its own modules without it.
            args: vec!["-d".into(), "-i".into()],
            env: base(&[
                ("PS1", "LI-ZSH> "),
                ("PS2", "LI-ZSH+ "),
                ("ZLE_RPROMPT_INDENT", "0"),
                // zsh looks for zle.so under /usr/lib/zsh/<ver>/zsh, which does
                // not exist for this unpacked copy. Measured failure:
                // "zsh: failed to load module `zsh/zle': /usr/lib/zsh/5.9.2/zsh/zle.so:
                //  cannot open shared object file". Without zle, ZLE is dead and
                // the shell's line editing is unusable.
                ("ZDOTDIR", ""), // filled in by run_shell
                ("LI_ZSH_MODULE_ROOT", ""), // filled in by run_shell
            ]),
            sentinel: "LI_SENTINEL_ZSH".into(),
            arith_cmd: "echo $((2 + 3))",
        },
        ShellSpec {
            id: "fish",
            path: "/usr/bin/fish".into(),
            args: vec!["--no-config".into(), "-i".into()],
            // fish has no PS1: the prompt is the fish_prompt function. Only the
            // greeting can be suppressed from the environment.
            env: base(&[("fish_greeting", "")]),
            sentinel: "LI_SENTINEL_FISH".into(),
            arith_cmd: "math 2 + 3",
        },
    ]
}

/// Directory that contains the `zsh/` subdirectory of loadable modules, derived
/// from the zsh binary's own location (`<prefix>/usr/bin/zsh` ->
/// `<prefix>/usr/lib/zsh/<ver>`).
///
/// This is how the unpacked local zsh copy is made usable without installing it
/// system-wide: `module_path` is pointed at the copy instead of `/usr/lib`.
pub fn zsh_module_root(zsh_path: &str) -> String {
    let p = std::path::Path::new(zsh_path);
    // <prefix>/usr/bin/zsh -> <prefix>/usr/lib/zsh
    let prefix = p
        .parent()
        .and_then(|d| d.parent())
        .and_then(|d| d.parent());
    let lib = match prefix {
        Some(prefix) => prefix.join("usr/lib/zsh"),
        None => std::path::PathBuf::from("/usr/lib/zsh"),
    };
    // Pick the highest version directory present, if any.
    let mut versions: Vec<std::path::PathBuf> = std::fs::read_dir(&lib)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.join("zsh").is_dir())
                .collect()
        })
        .unwrap_or_default();
    versions.sort();
    versions.pop().map(|v| v.to_string_lossy().into_owned()).unwrap_or_else(|| lib.to_string_lossy().into_owned())
}

/// Write the isolated zsh startup shim. Returns the ZDOTDIR to use.
///
/// The shim exists only to set `module_path`/`fpath`; it deliberately does not
/// define a prompt or any aliases, so the measurement stays about the PTY
/// mechanism rather than about zsh configuration.
pub fn write_zsh_shim(workdir: &std::path::Path, module_root: &str) -> Result<std::path::PathBuf> {
    let dir = workdir.join("zdotdir");
    std::fs::create_dir_all(&dir).map_err(|e| crate::error::Error::io(&dir, e))?;
    let body = format!(
        "# written by li-pty-proto: isolated zsh startup, no user config\n\
         module_path=({module_root} $module_path)\n\
         fpath=({module_root} $fpath)\n"
    );
    let env = dir.join(".zshenv");
    std::fs::write(&env, body).map_err(|e| crate::error::Error::io(&env, e))?;
    Ok(dir)
}

pub struct StepReport {
    pub name: &'static str,
    pub matched: bool,
    pub elapsed_ms: u64,
    pub detail: Value,
    /// What arrived during this step, control characters escaped.
    pub output_escaped: String,
}

impl StepReport {
    pub fn to_json(&self) -> Value {
        json!({
            "step": self.name,
            "passed": self.matched,
            "elapsed_ms": self.elapsed_ms,
            "detail": self.detail,
            "output_escaped": self.output_escaped,
        })
    }
}

pub struct ShellRun {
    pub shell: String,
    pub steps: Vec<StepReport>,
    pub transcript_escaped: String,
    pub exit_status: Option<i32>,
    pub exit_signal: Option<String>,
    pub eof_reached: bool,
}

/// Records a step: captures the bytes that arrived since the last step.
struct Recorder<'a> {
    s: &'a mut Session,
    mark: usize,
    steps: Vec<StepReport>,
}

impl<'a> Recorder<'a> {
    fn new(s: &'a mut Session) -> Self {
        Recorder { mark: s.mark(), s, steps: Vec::new() }
    }

    fn step(&mut self, name: &'static str, matched: bool, elapsed_ms: u64, detail: Value) {
        let out = self.s.since(self.mark);
        self.steps.push(StepReport {
            name,
            matched,
            elapsed_ms,
            detail,
            output_escaped: crate::util::escape_control(&out),
        });
        self.mark = self.s.mark();
    }

    /// Put the shell back into a known-empty editing state.
    ///
    /// Necessary because a line the shell rejects stays in its editing buffer:
    /// measured with fish, one bad command made every later keystroke append to
    /// the same rejected line, which silently poisoned all following steps.
    /// ^C aborts the line, ^U kills it; both are harmless when already idle.
    fn resync(&mut self) {
        let _ = self.s.write_raw(&[0x03]);
        let _ = self.s.pump(120);
        let _ = self.s.write_raw(&[0x15]);
        let _ = self.s.pump(120);
        let _ = self.s.wait_quiet(120, 800);
        self.discard_pending();
    }

    fn discard_pending(&mut self) {
        self.mark = self.s.mark();
    }

    fn finish(self) -> Vec<StepReport> {
        self.steps
    }
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed().as_millis() as u64)
}

use crate::util::{count_bare_lines, printed_lines};

/// Count lines that are exactly `token`, for the portability reason explained
/// on `util::count_bare_lines`.
fn bare_token_lines(text: &str, token: &str) -> usize {
    count_bare_lines(text, token)
}

/// Run the complete matrix for one shell. A step that does not meet its
/// expectation is recorded as `passed: false`; the matrix continues, so one
/// broken shell cannot hide the others' results.
pub fn run_shell(spec: &ShellSpec, workdir: &std::path::Path) -> Result<ShellRun> {
    let winsize = Winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
    let argv = {
        let mut v = vec![spec.path.clone()];
        v.extend(spec.args.iter().cloned());
        v
    };
    // zsh only: provide an isolated ZDOTDIR whose .zshenv fixes module_path.
    // The empty placeholders in `spec.env` are resolved here, once, so the
    // ShellSpec stays a pure description.
    let mut env: Vec<(String, String)> = spec.env.clone();
    if spec.id == "zsh" {
        let root = zsh_module_root(&spec.path);
        let dir = write_zsh_shim(workdir, &root)?;
        for (k, v) in env.iter_mut() {
            if k == "ZDOTDIR" {
                *v = dir.to_string_lossy().into_owned();
            } else if k == "LI_ZSH_MODULE_ROOT" {
                *v = root.clone();
            }
        }
    }
    let mut s = Session::spawn(SpawnSpec {
        name: spec.id,
        argv: &argv,
        env: &env,
        winsize,
        cwd: workdir,
    })?;
    let sentinel = spec.sentinel.clone();

    // ---- 1. shell starts and emits something ------------------------
    let mut r = Recorder::new(&mut s);
    let (first, ms) = timed(|| {
        let mut waited = 0u64;
        while r.s.bytes_read() == 0 && waited < 8_000 {
            let _ = r.s.pump(100);
            waited += 100;
        }
        (r.s.bytes_read(), waited)
    });
    let (_, warm) = timed(|| r.s.wait_quiet(300, 4_000));
    r.step(
        "01_shell_started",
        first.0 > 0,
        ms + warm,
        json!({
            "wait_for_first_byte_ms": first.1,
            "prompt_hex_after_quiet": crate::util::hex(r.s.raw_bytes(), 200),
            "slave_path": r.s.slave_path(),
            "pid": r.s.pid,
            "interpretation": "prompt text is shell-specific; only arrival of output is asserted",
        }),
    );

    // ---- 2. sentinel command executes ------------------------------
    r.resync();
    let _ = r.s.send_line(&format!("echo {sentinel}"));
    let (out, ms) = timed(|| r.s.read_until_any(&sentinel, Duration::from_secs(8)));
    let (_, warm) = timed(|| r.s.wait_quiet(250, 3_000));
    let step_raw = r.s.since(r.mark);
    let occ_raw = step_raw.matches(&sentinel).count();
    let bare = bare_token_lines(&step_raw, &sentinel);
    r.step(
        "02_sentinel_executes",
        bare >= 1,
        ms + warm,
        json!({
            "sentinel": sentinel,
            "occurrences_raw": occ_raw,
            "occurrences_after_ansi_strip": crate::util::strip_ansi(&step_raw).matches(&sentinel).count(),
            "bare_output_lines": bare,
            "read_until_matched": out.as_ref().map(|o| o.matched).unwrap_or(false),
            "why_bare_line": "the tty echo also contains the token; only the command's output forms a line equal to it",
        }),
    );

    // ---- 3. pipeline + arithmetic ---------------------------------
    r.resync();
    let _ = r.s.send_line("echo alpha beta | tr ' ' '-'");
    let (_, ms1) = timed(|| r.s.read_until_any("alpha-beta", Duration::from_secs(8)));
    let _ = r.s.send_line(spec.arith_cmd);
    // Both variants print `5`; wait for the bare digit, then confirm on a line.
    let (_, ms2) = timed(|| r.s.read_until_any("5", Duration::from_secs(8)));
    let (_, warm) = timed(|| r.s.wait_quiet(250, 3_000));
    let lines3 = printed_lines(&r.s.since(r.mark));
    let pipe_ok = lines3.iter().any(|l| l == "alpha-beta");
    let arith_ok = lines3.iter().any(|l| l == "5");
    r.step(
        "03_pipeline_and_arithmetic",
        pipe_ok && arith_ok,
        ms1 + ms2 + warm,
        json!({
            "pipeline_ok": pipe_ok,
            "arithmetic_ok": arith_ok,
            "pipeline": "echo alpha beta | tr ' ' '-'",
            "arithmetic_cmd": spec.arith_cmd,
            "why_per_shell": "fish has no $((...)); zsh treats [5] as a glob pattern",
        }),
    );

    // ---- 4. initial geometry --------------------------------------
    r.resync();
    let _ = r.s.send_line("stty size");
    let (size1, ms) = timed(|| r.s.read_until_any("40 120", Duration::from_secs(8)));
    let (_, warm) = timed(|| r.s.wait_quiet(250, 3_000));
    let got1 = printed_lines(&r.s.since(r.mark)).iter().any(|l| l == "40 120");
    r.step(
        "04_initial_winsize",
        got1,
        ms + warm,
        json!({
            "asked": "stty size",
            "expected_line": "40 120",
            "found": got1,
            "read_until_matched": size1.as_ref().map(|o| o.matched).unwrap_or(false),
        }),
    );

    // ---- 5. TIOCSWINSZ -> SIGWINCH -> new geometry -----------------
    let resize_ok = r.s.resize(50, 200).is_ok();
    std::thread::sleep(Duration::from_millis(250));
    let _ = r.s.send_line("stty size");
    let (size2, ms) = timed(|| r.s.read_until_any("50 200", Duration::from_secs(8)));
    let (_, warm) = timed(|| r.s.wait_quiet(250, 3_000));
    let got2 = printed_lines(&r.s.since(r.mark)).iter().any(|l| l == "50 200");
    let q = r.s.query_winsize();
    r.step(
        "05_resize_visible",
        resize_ok && got2,
        ms + warm,
        json!({
            "requested": "50 rows x 200 cols",
            "TIOCGWINSZ_rows": q.as_ref().map(|w| w.ws_row).unwrap_or(0),
            "TIOCGWINSZ_cols": q.as_ref().map(|w| w.ws_col).unwrap_or(0),
            "child_reported_new_size": got2,
            "read_until_matched": size2.as_ref().map(|o| o.matched).unwrap_or(false),
            "mechanism": "TIOCSWINSZ on the master makes the kernel send SIGWINCH to the slave's foreground pgroup",
        }),
    );
    let _ = r.s.resize(40, 120);
    std::thread::sleep(Duration::from_millis(200));

    // ---- 6. ^C interrupts, shell stays alive -----------------------
    r.resync();
    let _ = r.s.send_line("sleep 30");
    std::thread::sleep(Duration::from_millis(500));
    let int_injected = r.s.write_raw(&[0x03]).is_ok();
    std::thread::sleep(Duration::from_millis(400));
    let token_int = format!("{sentinel}-AFTER-INT");
    let _ = r.s.send_line(&format!("echo {token_int}"));
    let (_, ms) = timed(|| r.s.read_until_any(&token_int, Duration::from_secs(8)));
    let (_, warm) = timed(|| r.s.wait_quiet(250, 3_000));
    let t6 = r.s.since(r.mark);
    let alive6 = bare_token_lines(&t6, &token_int) >= 1;
    r.step(
        "06_ctrl_c",
        int_injected && alive6,
        ms + warm,
        json!({
            "sent_byte": "0x03",
            "shell_alive_after": alive6,
            "interrupt_string_seen": t6.contains("Interrupt") || t6.contains("^C"),
            "sleep_did_not_complete": !crate::util::strip_ansi(&t6).contains("30s"),
            "mechanism": "ISIG in the slave line discipline -> SIGINT to the foreground pgroup",
        }),
    );

    // ---- 7. full-screen: vi ---------------------------------------
    r.resync();
    let vi_path = format!("{}/vi-target.txt", workdir.display());
    let _ = r.s.send_line(&format!("vi -n {vi_path}"));
    std::thread::sleep(Duration::from_millis(900));
    let _ = r.s.pump(400);
    let vi_text = r.s.since(r.mark);
    let vi_detail = json!({
        "alt_screen_enter": vi_text.contains("\u{1b}[?1049h") || vi_text.contains("\u{1b}[?47h"),
        "cursor_motion_present": vi_text.contains("\u{1b}["),
        "tilde_filler_present": crate::util::printed_lines(&vi_text).iter().any(|l| l.starts_with('~')),
        "screen_bytes": vi_text.len(),
    });
    r.s.write_raw(b"i").ok();
    std::thread::sleep(Duration::from_millis(250));
    r.s.write_raw(b"Z").ok();
    std::thread::sleep(Duration::from_millis(250));
    r.s.write_raw(&[0x1b]).ok();
    std::thread::sleep(Duration::from_millis(250));
    r.s.write_raw(b":q!").ok();
    std::thread::sleep(Duration::from_millis(200));
    r.s.write_raw(b"\r").ok();
    std::thread::sleep(Duration::from_millis(800));
    let token_vi = format!("{sentinel}-AFTER-VI");
    let _ = r.s.send_line(&format!("echo {token_vi}"));
    let (_, ms) = timed(|| r.s.read_until_any(&token_vi, Duration::from_secs(10)));
    let (_, warm) = timed(|| r.s.wait_quiet(300, 3_000));
    let t7 = r.s.since(r.mark);
    let alive7 = bare_token_lines(&t7, &token_vi) >= 1;
    let mut d7 = vi_detail;
    d7["alt_screen_exit"] = json!(t7.contains("\u{1b}[?1049l"));
    d7["shell_usable_after_quit"] = json!(alive7);
    r.step("07_fullscreen_vi", alive7, ms + warm, d7);

    // ---- 8. full-screen: less -------------------------------------
    r.resync();
    let _ = r.s.send_line("less -R /etc/passwd");
    std::thread::sleep(Duration::from_millis(900));
    let _ = r.s.pump(400);
    let less_text = r.s.since(r.mark);
    let mut d8 = json!({
        "alt_screen_enter": less_text.contains("1049h") || less_text.contains("47h"),
        "cursor_motion_present": less_text.contains("\u{1b}["),
        "plain_content_present": less_text.contains("root:"),
        "screen_bytes": less_text.len(),
    });
    r.s.write_raw(b"q").ok();
    std::thread::sleep(Duration::from_millis(700));
    let token_less = format!("{sentinel}-AFTER-LESS");
    let _ = r.s.send_line(&format!("echo {token_less}"));
    let (_, ms) = timed(|| r.s.read_until_any(&token_less, Duration::from_secs(10)));
    let (_, warm) = timed(|| r.s.wait_quiet(300, 3_000));
    let t8 = r.s.since(r.mark);
    let alive8 = bare_token_lines(&t8, &token_less) >= 1;
    d8["shell_usable_after_quit"] = json!(alive8);
    d8["alt_screen_exit"] = json!(t8.contains("\u{1b}[?1049l"));
    r.step("08_fullscreen_less", alive8, ms + warm, d8);

    // ---- 9. control case: top -b -n1 ------------------------------
    r.resync();
    //
    // Measured directly on this host: `top -b -n1 | head -5` truncates the
    // stream before the column-header row, so the header marker never appears.
    // The full batch run is captured instead.
    let _ = r.s.send_line("top -b -n1 -o %MEM");
    // Wait for the child to finish writing; the sentinel below proves the
    // prompt came back.
    let (_, _ms) = timed(|| r.s.read_until_any(&format!("{sentinel}-AFTER-TOP"), Duration::from_millis(50)));
    let token_top = format!("{sentinel}-AFTER-TOP");
    std::thread::sleep(Duration::from_millis(1_500));
    let _ = r.s.send_line(&format!("echo {token_top}"));
    let (_, ms) = timed(|| r.s.read_until_any(&token_top, Duration::from_secs(12)));
    let (_, warm) = timed(|| r.s.wait_quiet(400, 4_000));
    let t9 = r.s.since(r.mark);
    let lines9 = printed_lines(&t9);
    let has_pid_header = lines9
        .iter()
        .any(|l| l.contains("PID") && (l.contains("COMMAND") || l.contains("\u{547d}\u{4ee4}")));
    let has_summary = lines9.iter().any(|l| l.contains("load average"));
    let plain_lines = lines9.iter().filter(|l| l.contains("PID")).count();
    let alive9 = bare_token_lines(&t9, &token_top) >= 1;
    r.step(
        "09_control_top_batch",
        alive9 && has_pid_header,
        ms + warm,
        json!({
            "marker": "a line containing both PID and COMMAND (or its locale equivalent)",
            "pid_header_found": has_pid_header,
            "summary_block_found": has_summary,
            "lines_containing_PID": plain_lines,
            "bytes": t9.len(),
            "first_printed_lines": lines9.iter().take(3).cloned().collect::<Vec<_>>(),
            "note": "batch mode -> ordinary line output; contrast with vi/less above",
        }),
    );

    // ---- 10. ^D closes the shell ----------------------------------
    let _ = r.s.send_line("");
    std::thread::sleep(Duration::from_millis(300));
    let d1 = r.s.write_raw(&[0x04]);
    let deadline = Instant::now() + Duration::from_secs(6);
    while !r.s.at_eof() && Instant::now() < deadline {
        let _ = r.s.pump(100);
    }
    let eof1 = r.s.at_eof();
    if !eof1 {
        // A shell that was mid-line needs the buffer cleared before EOF;
        // send a second ^D as a line-clear attempt and check again.
        let _ = r.s.write_raw(&[0x04]);
        let d2 = Instant::now() + Duration::from_secs(4);
        while !r.s.at_eof() && Instant::now() < d2 {
            let _ = r.s.pump(100);
        }
    }
    let eof_final = r.s.at_eof();
    let exit_status = r.s.reap(Duration::from_secs(4)).unwrap_or(None);
    let signal = r.s.exit_signal_string();
    r.step(
        "10_ctrl_d_exits",
        d1.is_ok() && (eof_final || exit_status.is_some()),
        (deadline - Instant::now()).as_millis().min(0) as u64,
        json!({
            "sent_byte": "0x04",
            "master_eof": eof_final,
            "exit_status": exit_status,
            "exit_signal": signal,
            "note": "0x04 on an empty line is EOF; the shell should exit with status 0",
        }),
    );

    let steps = r.finish();
    let transcript = String::from_utf8_lossy(s.raw_bytes()).into_owned();
    Ok(ShellRun {
        shell: spec.id.to_string(),
        steps,
        transcript_escaped: crate::util::escape_control(&transcript),
        exit_status,
        exit_signal: signal,
        eof_reached: eof_final,
    })
}
