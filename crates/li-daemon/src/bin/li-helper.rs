//! `li-helper`: confined, zero-exec privilege helper binary (Phase 3: P3-06 .. P3-09).

use std::path::PathBuf;
use std::sync::Arc;

use li_daemon::helper::{
    AuditLog, ConfinedRoot, HelperState, bind_helper_socket, enable_no_new_privs, run_helper_loop,
};
use tokio::sync::watch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_no_new_privs()?;

    let mut root_dir = PathBuf::from("/tmp");
    let mut audit_path = PathBuf::from("/tmp/li-helper-audit.jsonl");
    let mut runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("linux-intelligence");

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--root" => root_dir = PathBuf::from(it.next().ok_or("--root requires path")?),
            "--audit" => audit_path = PathBuf::from(it.next().ok_or("--audit requires path")?),
            "--socket-dir" => {
                runtime_dir = PathBuf::from(it.next().ok_or("--socket-dir requires path")?)
            }
            other => return Err(format!("unknown arg `{other}`").into()),
        }
    }

    let root = ConfinedRoot::open(&root_dir)?;
    let audit = AuditLog::open(&audit_path)?;
    let uid = rustix::process::getuid().as_raw();
    let state = Arc::new(HelperState::new(root, audit, uid));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = bind_helper_socket(&runtime_dir)?;
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(true);
        });
        run_helper_loop(listener, state, rx).await
    })?;

    Ok(())
}
