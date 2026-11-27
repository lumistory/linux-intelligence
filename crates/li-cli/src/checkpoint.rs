//! `li checkpoint` — non-destructive checkpoint creation and listing (P1-24 / P1-25).

use std::path::Path;
use std::process::Command;

use li_journal::write::Checkpoint;
use li_journal::{Config, Journal, clock};
use li_proto::{Event, EventId, EventType, EvidenceStatus, Source};

use crate::client::{deliver_events, resolve_journal_path};

pub fn caveats_for(kind: &str) -> &'static str {
    match kind {
        "git" => "git provider: tracked files only up to recorded commit; untracked files excluded",
        "manual" => {
            "manual checkpoint: metadata anchor only; not an atomic filesystem snapshot and open-file consistency is not guaranteed"
        }
        _ => "checkpoint caveat: recovery semantics depend on external provider inspection",
    }
}

pub fn create(
    kind: &str,
    session: Option<&str>,
    note: Option<&str>,
    socket: Option<&Path>,
    journal_path: Option<&Path>,
) -> Result<String, String> {
    if !matches!(kind, "manual" | "git") {
        return Err(format!(
            "unsupported Phase 1 checkpoint kind `{kind}` (expected `manual` or `git`)"
        ));
    }
    let jpath = resolve_journal_path(journal_path)?;
    let mut journal = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
    journal.migrate().map_err(|e| e.to_string())?;

    let reference = if kind == "git" {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| format!("git rev-parse HEAD failed: {e}"))?;
        if !out.status.success() {
            return Err("not inside a git repository with a HEAD commit".into());
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    };

    let mono = clock::mono_ns().unwrap_or(0);
    let wall = clock::wall_now_rfc3339_ms();
    let cp_id = format!("cp-{kind}-{}-{mono}", std::process::id());
    let caveat = caveats_for(kind);
    let combined_note = match note {
        Some(n) if !n.is_empty() => format!("{n} | {caveat}"),
        _ => caveat.to_string(),
    };

    let cp = Checkpoint {
        checkpoint_id: cp_id.clone(),
        session_id: session.map(str::to_string),
        operation_id: None,
        created_wall: wall.clone(),
        kind: kind.to_string(),
        reference: reference.clone(),
        file_count: None,
        note: Some(combined_note),
    };
    journal.checkpoint(cp);
    let wm = journal.flush().map_err(|e| e.to_string())?;
    if wm.failed > 0 {
        return Err(format!(
            "checkpoint write failed: {:?}",
            journal.last_error()
        ));
    }

    let mut payload = serde_json::Map::new();
    payload.insert("provider".into(), serde_json::json!(kind));
    payload.insert("checkpoint".into(), serde_json::json!(cp_id));
    payload.insert("caveats".into(), serde_json::json!([caveat]));
    if let Some(ref r) = reference {
        payload.insert("reference".into(), serde_json::json!(r));
    }
    let sid = session
        .map(li_proto::SessionId::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    let ev = Event::new(
        EventId::new(format!("snap-{}-{mono}", std::process::id())).map_err(|e| e.to_string())?,
        Source::Recovery,
        sid,
        0,
        mono,
        wall,
        EventType::new("SnapshotCreated").map_err(|e| e.to_string())?,
        payload,
        EvidenceStatus::Observed,
    )
    .map_err(|e| e.to_string())?;
    let _ = deliver_events(socket, Some(&jpath), None, &[ev]);

    Ok(format!(
        "created checkpoint {cp_id} (kind={kind}, caveat={caveat})"
    ))
}

pub fn list(journal_path: Option<&Path>, limit: i64) -> Result<String, String> {
    let jpath = resolve_journal_path(journal_path)?;
    if !jpath.exists() {
        return Err(format!("journal does not exist: {}", jpath.display()));
    }
    let j = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
    let rows = j
        .reader()
        .map_err(|e| e.to_string())?
        .list_checkpoints(limit)
        .map_err(|e| e.to_string())?;
    serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())
}
