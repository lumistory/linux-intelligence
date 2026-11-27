//! Phase 3 (P3-06 .. P3-09) 受限零-exec 特权辅助服务安全边界集成测试。

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use li_daemon::helper::{
    AuditLog, CODE_ARG_MISSING, CODE_ARG_TOO_LARGE, CODE_ARG_UNEXPECTED, CODE_BAD_OPCODE,
    CODE_DISCARD_EXCEEDED, CODE_FRAME_TOO_LARGE, CODE_NOT_AUTHORIZED, CODE_NOT_IMPLEMENTED,
    CODE_PARTIAL_TIMEOUT, CODE_PATH_ESCAPE, CODE_PATH_INVALID, CODE_UNKNOWN_OPCODE, ConfinedRoot,
    HELPER_DISCARD_CAP_BYTES, HELPER_MAX_ARG_BYTES, HELPER_MAX_FRAME_BYTES, HelperResponse,
    HelperState, OPCODE_BTRFS_SNAPSHOT, OPCODE_DESCRIBE, OPCODE_READ_BOUNDED, OPCODE_STAT_PATH,
    bind_helper_socket, dispatch_request, enable_no_new_privs, run_helper_loop,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("li-helper-test-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mkdir scratch");
    p
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn make_state(dir: &Path, allowed_uid: u32) -> (PathBuf, Arc<HelperState>) {
    let root_dir = dir.join("root");
    std::fs::create_dir_all(root_dir.join("sub")).expect("mkdir root/sub");
    std::fs::write(root_dir.join("sub/hello.txt"), b"hello-confined-world").expect("write sample");

    let outside = dir.join("outside-secret.txt");
    std::fs::write(&outside, b"top-secret").expect("write outside");
    let _ = symlink(&outside, root_dir.join("escape-link"));

    let root = ConfinedRoot::open(&root_dir).expect("open confined root");
    let audit = AuditLog::open(&dir.join("audit.jsonl")).expect("open audit");
    let state = Arc::new(HelperState::new(root, audit, allowed_uid));
    (dir.join("sock"), state)
}

async fn send_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    line: &[u8],
) -> HelperResponse {
    writer.write_all(line).await.expect("write line");
    writer.write_all(b"\n").await.expect("write nl");
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await.expect("read resp");
    serde_json::from_str(&resp_line).expect("decode HelperResponse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_opcodes_and_0600_dense_audit_log() {
    let dir = scratch_dir("valid");
    let (sock_dir, state) = make_state(&dir, current_uid());
    let listener = bind_helper_socket(&sock_dir).expect("bind helper socket");
    let sock_path = sock_dir.join("helper.sock");
    let sock_mode = std::fs::metadata(&sock_path)
        .expect("sock meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(sock_mode, 0o600, "helper socket must be 0600");

    let (tx, rx) = watch::channel(false);
    let srv = tokio::spawn(run_helper_loop(listener, Arc::clone(&state), rx));

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    let r1 = send_line(&mut w, &mut reader, br#"{"opcode":3,"args":{}}"#).await;
    assert!(r1.ok, "describe should succeed: {r1:?}");
    let d1 = r1.data.expect("describe data");
    assert_eq!(d1["execve_after_startup"], 0);

    let r2 = send_line(
        &mut w,
        &mut reader,
        br#"{"opcode":1,"args":{"rel":"sub/hello.txt"}}"#,
    )
    .await;
    assert!(r2.ok, "stat should succeed: {r2:?}");
    let d2 = r2.data.expect("stat data");
    assert_eq!(d2["resolved_under_root"], true);
    assert_eq!(d2["kind"], "file");
    assert_eq!(d2["size"], 20);

    let r3 = send_line(
        &mut w,
        &mut reader,
        br#"{"opcode":2,"args":{"rel":"sub/hello.txt","max_bytes":5}}"#,
    )
    .await;
    assert!(r3.ok, "read_bounded should succeed: {r3:?}");
    let d3 = r3.data.expect("read data");
    assert_eq!(d3["bytes"], 5);
    assert_eq!(d3["truncated"], true);
    assert_eq!(d3["preview_hex"], "68656c6c6f");

    drop(w);
    drop(reader);
    let _ = tx.send(true);
    let _ = srv.await;

    let audit_path = state.audit.path();
    let audit_mode = std::fs::metadata(audit_path)
        .expect("audit meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(audit_mode, 0o600, "audit log must stay 0600");

    let raw = std::fs::read_to_string(audit_path).expect("read audit");
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid audit json"))
        .collect();
    assert!(lines.len() >= 5, "expected start + 3 requests + close");
    for (idx, entry) in lines.iter().enumerate() {
        assert_eq!(entry["sequence"], (idx + 1) as u64);
        assert_eq!(
            entry["tamper_protection"],
            "mode_0600_owner_not_tamper_proof"
        );
    }
    let served_count = lines
        .iter()
        .filter(|e| e["event"] == "RequestServed")
        .count();
    assert_eq!(served_count, 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejection_matrix_escapes_metacharacters_and_opcodes() {
    let dir = scratch_dir("matrix");
    let (_sock_dir, state) = make_state(&dir, current_uid());

    let escapes = [
        "..",
        "../outside",
        "sub/../../etc/passwd",
        "/etc/passwd",
        "./sub/hello.txt",
        "sub/",
        "sub//hello.txt",
        "escape-link",
    ];
    for p in escapes {
        let req = json!({ "opcode": OPCODE_STAT_PATH, "args": { "rel": p } }).to_string();
        let resp = dispatch_request(&state, req.as_bytes());
        assert!(!resp.ok, "escape `{p}` must be rejected");
        assert!(
            matches!(
                resp.code.as_deref(),
                Some(CODE_PATH_ESCAPE | CODE_PATH_INVALID)
            ),
            "unexpected code for `{p}`: {resp:?}"
        );
    }

    let metachars = [
        "a;id", "a|cat", "a&id", "a$HOME", "a`id`", "a<in", "a>out", "a*glob", "a?glob", "a b",
        "a'q", "a\"q",
    ];
    for p in metachars {
        let req = json!({ "opcode": OPCODE_STAT_PATH, "args": { "rel": p } }).to_string();
        let resp = dispatch_request(&state, req.as_bytes());
        assert!(!resp.ok, "metachar `{p}` must be rejected");
        assert_eq!(resp.code.as_deref(), Some(CODE_PATH_INVALID));
    }

    let r_bad_op = dispatch_request(&state, br#"{"opcode":"StatPath","args":{}}"#);
    assert_eq!(r_bad_op.code.as_deref(), Some(CODE_BAD_OPCODE));

    let r_unk_op = dispatch_request(&state, br#"{"opcode":99,"args":{}}"#);
    assert_eq!(r_unk_op.code.as_deref(), Some(CODE_UNKNOWN_OPCODE));

    let r_not_impl = dispatch_request(
        &state,
        json!({ "opcode": OPCODE_BTRFS_SNAPSHOT, "args": {} })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r_not_impl.code.as_deref(), Some(CODE_NOT_IMPLEMENTED));

    let r_unexp = dispatch_request(
        &state,
        json!({ "opcode": OPCODE_DESCRIBE, "args": { "extra": 1 } })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r_unexp.code.as_deref(), Some(CODE_ARG_UNEXPECTED));

    let r_missing = dispatch_request(
        &state,
        json!({ "opcode": OPCODE_READ_BOUNDED, "args": {} })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r_missing.code.as_deref(), Some(CODE_ARG_MISSING));

    let huge_arg = "a".repeat(HELPER_MAX_ARG_BYTES + 1);
    let r_huge_arg = dispatch_request(
        &state,
        json!({ "opcode": OPCODE_STAT_PATH, "args": { "rel": huge_arg } })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r_huge_arg.code.as_deref(), Some(CODE_ARG_TOO_LARGE));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peercred_mismatch_rejected_and_audited() {
    let dir = scratch_dir("peercred");
    let (sock_dir, state) = make_state(&dir, current_uid().saturating_add(1));
    let listener = bind_helper_socket(&sock_dir).expect("bind");
    let sock_path = sock_dir.join("helper.sock");
    let (tx, rx) = watch::channel(false);
    let srv = tokio::spawn(run_helper_loop(listener, Arc::clone(&state), rx));

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read rejection");
    let resp: HelperResponse = serde_json::from_str(&line).expect("parse resp");
    assert!(!resp.ok);
    assert_eq!(resp.code.as_deref(), Some(CODE_NOT_AUTHORIZED));

    let _ = tx.send(true);
    let _ = srv.await;

    let audit_raw = std::fs::read_to_string(state.audit.path()).expect("read audit");
    assert!(audit_raw.contains("AuthRejected"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_frame_drain_keeps_connection_and_discard_cap_closes() {
    let dir = scratch_dir("frames");
    let (sock_dir, state) = make_state(&dir, current_uid());
    let listener = bind_helper_socket(&sock_dir).expect("bind");
    let sock_path = sock_dir.join("helper.sock");
    let (tx, rx) = watch::channel(false);
    let srv = tokio::spawn(run_helper_loop(listener, Arc::clone(&state), rx));

    // > 64 KiB frame with newline drains cleanly and keeps connection alive.
    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let big = vec![b'x'; HELPER_MAX_FRAME_BYTES + 256];
    let r_big = send_line(&mut w, &mut reader, &big).await;
    assert!(!r_big.ok);
    assert_eq!(r_big.code.as_deref(), Some(CODE_FRAME_TOO_LARGE));

    let r_after = send_line(&mut w, &mut reader, br#"{"opcode":3,"args":{}}"#).await;
    assert!(
        r_after.ok,
        "connection must recover after non-fatal overlong line"
    );
    drop(w);
    drop(reader);

    // > 8 MiB discard cap terminates connection.
    let stream2 = UnixStream::connect(&sock_path).await.expect("connect2");
    let (r2, mut w2) = stream2.into_split();
    let mut reader2 = BufReader::new(r2);
    let chunk = vec![b'y'; 64 * 1024];
    let needed = (HELPER_DISCARD_CAP_BYTES / chunk.len()) + 2;
    for _ in 0..needed {
        if w2.write_all(&chunk).await.is_err() {
            break;
        }
    }
    let mut line2 = String::new();
    let n = reader2.read_line(&mut line2).await.expect("read fatal");
    assert!(n > 0);
    let r_fatal: HelperResponse = serde_json::from_str(&line2).expect("parse fatal");
    assert_eq!(r_fatal.code.as_deref(), Some(CODE_DISCARD_EXCEEDED));

    let _ = tx.send(true);
    let _ = srv.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_frame_timeout_2s_returns_explicit_error() {
    let dir = scratch_dir("timeout");
    let (sock_dir, state) = make_state(&dir, current_uid());
    let listener = bind_helper_socket(&sock_dir).expect("bind");
    let sock_path = sock_dir.join("helper.sock");
    let (tx, rx) = watch::channel(false);
    let srv = tokio::spawn(run_helper_loop(listener, Arc::clone(&state), rx));

    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    w.write_all(br#"{"opcode":3"#).await.expect("partial write");
    let mut line = String::new();
    let res = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("helper must time out within 5s")
        .expect("read timeout resp");
    assert!(res > 0);
    let resp: HelperResponse = serde_json::from_str(&line).expect("parse timeout resp");
    assert!(!resp.ok);
    assert_eq!(resp.code.as_deref(), Some(CODE_PARTIAL_TIMEOUT));

    drop(w);
    let _ = tx.send(true);
    let _ = srv.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_new_privs_and_zero_command_exec_in_helper_sources() {
    enable_no_new_privs().expect("PR_SET_NO_NEW_PRIVS must succeed");
    let status =
        std::fs::read_to_string("/proc/thread-self/status").expect("read /proc/thread-self/status");
    assert!(
        status
            .lines()
            .any(|l| l.starts_with("NoNewPrivs:") && l.trim_end().ends_with('1')),
        "expected NoNewPrivs: 1 in /proc/thread-self/status, got:\n{}",
        status
            .lines()
            .filter(|l| l.starts_with("NoNewPrivs"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let helper_src = include_str!("../src/helper.rs");
    let bin_src = include_str!("../src/bin/li-helper.rs");
    for (name, src) in [("helper.rs", helper_src), ("bin/li-helper.rs", bin_src)] {
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            assert!(
                !trimmed.contains("Command::new"),
                "{name} must not invoke Command::new: {trimmed}"
            );
            assert!(
                !trimmed.contains("process::Command"),
                "{name} must not reference std::process::Command: {trimmed}"
            );
            assert!(
                !trimmed.contains("execve("),
                "{name} code line must not invoke execve: {trimmed}"
            );
        }
    }
}
