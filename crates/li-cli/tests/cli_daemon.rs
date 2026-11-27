use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use li_daemon::server::{bind, record_startup, run_accept_loop, socket_path};
use li_daemon::state::DaemonState;
use li_journal::{Config, Journal};
use tokio::sync::watch;

fn tempdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("li-cli-e2e-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_commands_drive_live_daemon_uds_and_query_without_caused() {
    let root = tempdir("uds");
    let runtime_dir = root.join("run");
    let journal_path = root.join("state").join("journal.db");

    let mut journal = Journal::open(&journal_path, Config::default()).unwrap();
    journal.migrate().unwrap();
    let known = record_startup(&journal, "daemon-cli-1", "boot-cli").unwrap();
    let journal = Arc::new(journal);
    let state = Arc::new(DaemonState::new(Arc::clone(&journal), "boot-cli".into()));
    state.seed_known_sessions(known);

    let listener = bind(&runtime_dir).unwrap();
    let sock = socket_path(&runtime_dir);
    let (tx, rx) = watch::channel(false);
    let owner = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&root).unwrap());
    let server_task = tokio::spawn(run_accept_loop(listener, state, owner, rx));

    let li_bin = env!("CARGO_BIN_EXE_li");

    // 1. `li ping` against the live Tokio daemon
    let ping_out = tokio::task::spawn_blocking({
        let sock = sock.clone();
        move || {
            Command::new(li_bin)
                .args(["ping", "--socket"])
                .arg(&sock)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        ping_out.status.success(),
        "ping stderr: {}",
        String::from_utf8_lossy(&ping_out.stderr)
    );
    let ping_json: serde_json::Value =
        serde_json::from_slice(&ping_out.stdout).expect("valid pong json");
    assert_eq!(ping_json["server_schema_version"], 1);

    // 2. `li run` sending ScriptStarted + ScriptFinished over UDS
    let run_out = tokio::task::spawn_blocking({
        let sock = sock.clone();
        let jpath = journal_path.clone();
        move || {
            Command::new(li_bin)
                .args(["run", "--session", "cli-uds-1", "--socket"])
                .arg(&sock)
                .arg("--journal")
                .arg(&jpath)
                .args(["--", "/bin/sh", "-c", "exit 3"])
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert_eq!(run_out.status.code(), Some(3));

    // 3. `li checkpoint create`
    let cp_out = tokio::task::spawn_blocking({
        let sock = sock.clone();
        let jpath = journal_path.clone();
        move || {
            Command::new(li_bin)
                .args([
                    "checkpoint",
                    "create",
                    "--kind",
                    "manual",
                    "--session",
                    "cli-uds-1",
                    "--socket",
                ])
                .arg(&sock)
                .arg("--journal")
                .arg(&jpath)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        cp_out.status.success(),
        "cp stderr: {}",
        String::from_utf8_lossy(&cp_out.stderr)
    );

    // 4. `li status` and `li query --json`
    let status_out = tokio::task::spawn_blocking({
        let sock = sock.clone();
        let jpath = journal_path.clone();
        move || {
            Command::new(li_bin)
                .args(["status", "--socket"])
                .arg(&sock)
                .arg("--journal")
                .arg(&jpath)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(status_out.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["daemon"]["reachable"], true);
    assert_eq!(status_json["journal"]["integrity"], "ok");
    assert_eq!(status_json["journal"]["fk_violations"], 0);

    let query_out = tokio::task::spawn_blocking({
        let jpath = journal_path.clone();
        move || {
            Command::new(li_bin)
                .args(["query", "--session", "cli-uds-1", "--json", "--journal"])
                .arg(&jpath)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(query_out.status.success());
    let qtext = String::from_utf8_lossy(&query_out.stdout);
    assert!(!qtext.to_ascii_lowercase().contains("caused"));
    let qjson: serde_json::Value = serde_json::from_slice(&query_out.stdout).unwrap();
    let events = qjson["events"].as_array().unwrap();
    assert!(events.len() >= 3); // ScriptStarted, ScriptFinished, SnapshotCreated
    assert!(events.iter().all(|e| e["evidence_status"].is_string()));

    let _ = tx.send(true);
    server_task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}
