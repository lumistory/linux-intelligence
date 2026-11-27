//! `li-daemon`: user-level Tokio daemon hosting the UDS ingest server and
//! Phase 2+ observers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use li_daemon::observers::{fs as fs_obs, procfs as proc_obs, register_capabilities};
use li_daemon::server::{bind, boot_id, record_startup, run_accept_loop, socket_path};
use li_daemon::state::DaemonState;
use li_journal::{Config, Journal, clock};
use tokio::sync::watch;

struct CliArgs {
    runtime_dir: PathBuf,
    journal_path: PathBuf,
    proc_poll_ms: u64,
    watch_roots: Vec<PathBuf>,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut journal_path = std::env::var_os("LI_JOURNAL_PATH")
        .map(PathBuf::from)
        .or_else(li_journal::db::default_path)
        .ok_or_else(|| {
            "cannot resolve journal path (HOME unset and LI_JOURNAL_PATH unset)".to_string()
        })?;
    let mut proc_poll_ms: u64 = 200;
    let mut watch_roots: Vec<PathBuf> = std::env::var_os("LI_WATCH_ROOTS")
        .map(|s| std::env::split_paths(&s).collect())
        .unwrap_or_default();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix("--runtime-dir=") {
            runtime_dir = PathBuf::from(v);
        } else if arg == "--runtime-dir" {
            let v = it.next().ok_or("--runtime-dir requires a value")?;
            runtime_dir = PathBuf::from(v);
        } else if let Some(v) = arg.strip_prefix("--journal-path=") {
            journal_path = PathBuf::from(v);
        } else if arg == "--journal-path" {
            let v = it.next().ok_or("--journal-path requires a value")?;
            journal_path = PathBuf::from(v);
        } else if let Some(v) = arg.strip_prefix("--proc-poll-ms=") {
            proc_poll_ms = v
                .parse()
                .map_err(|_| "--proc-poll-ms must be an integer".to_string())?;
        } else if arg == "--proc-poll-ms" {
            let v = it.next().ok_or("--proc-poll-ms requires a value")?;
            proc_poll_ms = v
                .parse()
                .map_err(|_| "--proc-poll-ms must be an integer".to_string())?;
        } else if let Some(v) = arg.strip_prefix("--watch=") {
            watch_roots.push(PathBuf::from(v));
        } else if arg == "--watch" {
            let v = it.next().ok_or("--watch requires a path")?;
            watch_roots.push(PathBuf::from(v));
        } else {
            return Err(format!("unknown argument `{arg}`"));
        }
    }
    Ok(CliArgs {
        runtime_dir,
        journal_path,
        proc_poll_ms,
        watch_roots,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let mut journal = Journal::open(&args.journal_path, Config::default())?;
    journal.migrate()?;

    let boot = boot_id()?;
    let startup_id = format!(
        "daemon-{}-{}",
        std::process::id(),
        clock::mono_ns().unwrap_or(0)
    );
    let known = record_startup(&journal, &startup_id, &boot)?;
    register_capabilities(&journal);
    let journal = Arc::new(journal);
    let state = Arc::new(DaemonState::new(Arc::clone(&journal), boot));
    state.seed_known_sessions(known);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let owner = rustix::process::getuid().as_raw();
    let sock = socket_path(&args.runtime_dir);
    let proc_poll_ms = args.proc_poll_ms;
    let watch_roots = args.watch_roots;
    rt.block_on(async move {
        let listener = bind(&args.runtime_dir)?;
        let (tx, rx) = watch::channel(false);

        if proc_poll_ms > 0 {
            let j = Arc::clone(&journal);
            let r = rx.clone();
            tokio::spawn(proc_obs::run_proc_poller(
                j,
                Duration::from_millis(proc_poll_ms),
                r,
            ));
        }
        if !watch_roots.is_empty() {
            let j = Arc::clone(&journal);
            let r = rx.clone();
            tokio::spawn(async move {
                let _ = fs_obs::run_fs_watcher(j, watch_roots, r).await;
            });
        }

        tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if let Some(ref mut s) = sigterm {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
            let _ = tx.send(true);
        });
        let res = run_accept_loop(listener, state, owner, rx).await;
        let _ = std::fs::remove_file(&sock);
        res
    })?;

    Ok(())
}
