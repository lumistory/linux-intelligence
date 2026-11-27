use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use li_daemon::pty::{ManagedPty, PtyCapabilities};

fn shell_present(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn zsh_missing_module_path_fails_loudly_instead_of_stalling_zle() {
    let err = ManagedPty::spawn(
        "sh",
        &["-c", "true"],
        &[],
        24,
        80,
        Some(Path::new("/nonexistent/li-zsh-modules")),
    )
    .err()
    .expect("missing zsh module path must return explicit error");
    assert!(err.contains("zsh module path missing"));
}

#[test]
fn pty_distinguishes_owned_from_foreign_terminals() {
    assert!(PtyCapabilities::managed().owns_this_terminal);
    assert!(!PtyCapabilities::foreign_terminal().owns_this_terminal);
    assert!(!PtyCapabilities::foreign_terminal().foreign_terminal_takeover_supported);
}

#[test]
fn pty_responder_starts_fish_fast_and_handles_resize_without_crosstalk() {
    if shell_present("fish") {
        let t0 = Instant::now();
        let mut pty = ManagedPty::spawn(
            "fish",
            &["--no-config", "-i"],
            &[("TERM", "xterm-256color")],
            24,
            80,
            None,
        )
        .unwrap();
        pty.pump(Duration::from_millis(800), Some(Duration::from_millis(120)))
            .unwrap();
        let startup = t0.elapsed();
        assert!(
            startup < Duration::from_secs(2),
            "fish with responder stalled: {startup:?}"
        );
        assert!(
            !pty.answered_queries.is_empty(),
            "fish under xterm-256color should emit terminal queries answered by responder"
        );
        pty.write_input(b"echo LI_FISH_OK\nexit 0\n").unwrap();
        let (code, out, _) = pty.wait().unwrap();
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("LI_FISH_OK"));
    }

    // Four concurrent PTY sessions with distinct tags and resize check: zero crosstalk.
    let mut handles = Vec::new();
    for idx in 0..4usize {
        handles.push(thread::spawn(move || {
            let mut pty =
                ManagedPty::spawn("bash", &["--norc", "--noprofile", "-i"], &[], 24, 80, None)
                    .unwrap();
            pty.pump(Duration::from_millis(250), Some(Duration::from_millis(60)))
                .unwrap();
            let rows = 30 + idx as u16;
            let cols = 100 + idx as u16;
            pty.resize(rows, cols).unwrap();
            let cmd = format!("stty size; echo SESSION_TAG_{idx}\nexit {}\n", 10 + idx);
            pty.write_input(cmd.as_bytes()).unwrap();
            let (code, out, _) = pty.wait().unwrap();
            let text = String::from_utf8_lossy(&out).into_owned();
            assert_eq!(code, 10 + idx as i32);
            assert!(
                text.contains(&format!("{rows} {cols}")),
                "session {idx} resize missed: {text}"
            );
            assert!(text.contains(&format!("SESSION_TAG_{idx}")));
            for other in 0..4usize {
                if other != idx {
                    assert!(
                        !text.contains(&format!("SESSION_TAG_{other}")),
                        "crosstalk in session {idx}"
                    );
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}
