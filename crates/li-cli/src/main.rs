//! `li`: user-facing CLI for Linux Intelligence (`run`, `query`, `checkpoint`,
//! `purge`, `status`, `ping`, `shell-init`).

mod checkpoint;
mod client;
mod query;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use li_journal::{Config, Journal};

use crate::client::{default_runtime_dir, default_socket_path, resolve_journal_path, send_ping};

#[derive(Parser)]
#[command(name = "li", version, about = "Linux Intelligence CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a command in an isolated session and record ScriptStarted/ScriptFinished.
    Run {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        journal: Option<PathBuf>,
        #[arg(long, default_value_t = 50)]
        post_exit_ms: u64,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Query recorded events and relations (read-only).
    Query {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long = "type")]
        event_type: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    /// Create or list non-destructive checkpoints.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointCmd,
    },
    /// Audited retention purge of events older than an RFC 3339 UTC timestamp.
    Purge {
        #[arg(long)]
        before: String,
        #[arg(long, default_value = "manual_retention_purge")]
        reason: String,
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    /// Run two-level deduplication and R0-R7 correlation over recent events.
    Correlate {
        #[arg(long, default_value_t = 500)]
        limit: i64,
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    /// Probe the running daemon with a Ping flush barrier.
    Ping {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Inspect journal health and latest daemon reconciliation status.
    Status {
        #[arg(long)]
        journal: Option<PathBuf>,
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Print shell integration snippet for sourcing an interactive adapter.
    ShellInit {
        #[arg(value_enum)]
        shell: ShellKind,
    },
}

#[derive(Subcommand)]
enum CheckpointCmd {
    Create {
        #[arg(long, default_value = "manual")]
        kind: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        journal: Option<PathBuf>,
    },
    List {
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        journal: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.cmd) {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(err) => {
            eprintln!("li: {err}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cmd: Command) -> Result<i32, String> {
    let default_sock = default_socket_path(&default_runtime_dir());
    match cmd {
        Command::Run {
            session,
            socket,
            journal,
            post_exit_ms,
            command,
        } => {
            let (program, args) = command
                .split_first()
                .ok_or_else(|| "li run requires a program after `--`".to_string())?;
            let sock = socket.unwrap_or(default_sock);
            run::execute(run::RunOptions {
                session: session.as_deref(),
                socket: Some(&sock),
                journal: journal.as_deref(),
                post_exit_ms,
                program,
                args,
            })
        }
        Command::Query {
            session,
            path,
            event_type,
            limit,
            json,
            journal,
        } => {
            let out = query::render_query(
                journal.as_deref(),
                session.as_deref(),
                path.as_deref(),
                event_type.as_deref(),
                limit,
                json,
            )?;
            print!("{out}");
            Ok(0)
        }
        Command::Checkpoint { action } => match action {
            CheckpointCmd::Create {
                kind,
                session,
                note,
                socket,
                journal,
            } => {
                let sock = socket.unwrap_or(default_sock);
                let msg = checkpoint::create(
                    &kind,
                    session.as_deref(),
                    note.as_deref(),
                    Some(&sock),
                    journal.as_deref(),
                )?;
                println!("{msg}");
                Ok(0)
            }
            CheckpointCmd::List { limit, journal } => {
                let out = checkpoint::list(journal.as_deref(), limit)?;
                println!("{out}");
                Ok(0)
            }
        },
        Command::Purge {
            before,
            reason,
            journal,
        } => {
            let jpath = resolve_journal_path(journal.as_deref())?;
            let mut j = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
            j.migrate().map_err(|e| e.to_string())?;
            let stats = j
                .purge_before(&before, &reason)
                .map_err(|e| e.to_string())?;
            println!(
                "purged {} event(s); audit_event_id={}",
                stats.deleted_events, stats.audit_event_id
            );
            Ok(0)
        }
        Command::Correlate { limit, journal } => {
            let jpath = resolve_journal_path(journal.as_deref())?;
            let mut j = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
            j.migrate().map_err(|e| e.to_string())?;
            let stats = li_daemon::correlation::engine::correlate_recent(&j, limit)?;
            println!(
                "scanned={} operations={} edges={} relations={} corroborations={}",
                stats.events_scanned,
                stats.operations_created,
                stats.edges_attached,
                stats.relations_written,
                stats.corroborations_linked
            );
            Ok(0)
        }
        Command::Ping { socket } => {
            let sock = socket.unwrap_or(default_sock);
            let pong = send_ping(&sock, u64::from(std::process::id()))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&pong).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        Command::Status { journal, socket } => {
            let sock = socket.unwrap_or(default_sock);
            let daemon_info = match send_ping(&sock, 1) {
                Ok(p) => serde_json::json!({
                    "reachable": true,
                    "schema_version": p.server_schema_version,
                    "counts": p.counts,
                    "watermarks": p.watermarks,
                }),
                Err(e) => serde_json::json!({ "reachable": false, "error": e }),
            };
            let jpath = resolve_journal_path(journal.as_deref())?;
            let journal_info = if jpath.exists() {
                let j = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
                let r = j.reader().map_err(|e| e.to_string())?;
                serde_json::json!({
                    "path": jpath,
                    "integrity": r.integrity_check().map_err(|e| e.to_string())?,
                    "fk_violations": r.foreign_key_violations().map_err(|e| e.to_string())?,
                    "events": r.count("events").map_err(|e| e.to_string())?,
                    "sessions": r.count("sessions").map_err(|e| e.to_string())?,
                    "checkpoints": r.count("checkpoints").map_err(|e| e.to_string())?,
                })
            } else {
                serde_json::json!({ "path": jpath, "exists": false })
            };
            let report = serde_json::json!({
                "daemon": daemon_info,
                "journal": journal_info,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        Command::ShellInit { shell } => {
            print!("{}", render_shell_init(shell));
            Ok(0)
        }
    }
}

/// Renders a self-contained, zero-checkout-dependency shell integration script
/// with embedded capability declarations so `eval "$(li shell-init ...)"` or
/// `li shell-init fish | source` works out of the box across terminals.
fn render_shell_init(shell: ShellKind) -> String {
    match shell {
        ShellKind::Bash => {
            let cap = include_str!("../../../capabilities/bash.json").trim();
            let adapter = include_str!("../../../shell/bash/bash.sh");
            format!(
                "IFS= read -r _LI_CAP_RECORD <<'__LI_CAP_EOF__'\n{cap}\n__LI_CAP_EOF__\n{adapter}"
            )
        }
        ShellKind::Zsh => {
            let cap = include_str!("../../../capabilities/zsh.json").trim();
            let adapter = include_str!("../../../shell/zsh/zsh.zsh");
            format!(
                "IFS= read -r _LI_CAP_RECORD <<'__LI_CAP_EOF__'\n{cap}\n__LI_CAP_EOF__\n{adapter}"
            )
        }
        ShellKind::Fish => {
            let cap = include_str!("../../../capabilities/fish.json")
                .trim()
                .replace('\\', "\\\\")
                .replace('\'', "\\'");
            let adapter = include_str!("../../../shell/fish/fish.fish");
            format!("set -g _LI_CAP_RECORD '{cap}'\n{adapter}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_db(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("li-cli-test-{tag}-{}-{n}.db", std::process::id()))
    }

    #[test]
    fn query_output_never_contains_caused_and_always_shows_evidence_status() {
        let db = scratch_db("query");
        let mut j = Journal::open(&db, Config::default()).unwrap();
        j.migrate().unwrap();
        let ev = li_proto::Event::new(
            li_proto::EventId::new("cli-q-1").unwrap(),
            li_proto::Source::Script,
            None,
            0,
            10,
            "2026-11-24T00:00:00.000Z",
            li_proto::EventType::new("ScriptFinished").unwrap(),
            serde_json::Map::new(),
            li_proto::EvidenceStatus::Observed,
        )
        .unwrap();
        j.event(&ev).unwrap();
        j.flush().unwrap();
        drop(j);

        let text = query::render_query(Some(&db), None, None, None, 10, false).unwrap();
        assert!(text.contains("evidence_status=observed"));
        assert!(!text.to_ascii_lowercase().contains("caused"));

        let json = query::render_query(Some(&db), None, None, None, 10, true).unwrap();
        assert!(json.contains("\"evidence_status\": \"observed\""));
        assert!(!json.to_ascii_lowercase().contains("caused"));
        assert!(query::assert_no_caused("event caused failure").is_err());

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn li_run_records_two_distinct_lifecycle_events_with_internal_commands_false() {
        let db = scratch_db("run");
        let code = run::execute(run::RunOptions {
            session: Some("li-run-test"),
            socket: None,
            journal: Some(&db),
            post_exit_ms: 10,
            program: "/bin/sh",
            args: &["-c".into(), "exit 7".into()],
        })
        .unwrap();
        assert_eq!(code, 7);

        let j = Journal::open(&db, Config::default()).unwrap();
        let r = j.reader().unwrap();
        let events = r.query_events(Some("li-run-test"), None, None, 10).unwrap();
        assert_eq!(events.len(), 2);
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"ScriptStarted"));
        assert!(types.contains(&"ScriptFinished"));
        let fin = events
            .iter()
            .find(|e| e.event_type == "ScriptFinished")
            .unwrap();
        assert_eq!(fin.payload["exit_status"], 7);
        assert_eq!(fin.payload["internal_script_commands"], false);

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn li_checkpoint_creates_only_and_carries_caveat() {
        let db = scratch_db("cp");
        let msg = checkpoint::create("manual", None, Some("pre-edit"), None, Some(&db)).unwrap();
        assert!(msg.contains("manual"));
        let list = checkpoint::list(Some(&db), 10).unwrap();
        assert!(list.contains("pre-edit"));
        assert!(list.contains("not an atomic filesystem snapshot"));

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn shell_init_is_self_contained_and_free_of_downstream_coupling() {
        for kind in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
            let out = render_shell_init(kind);
            assert!(out.contains("_LI_CAP_RECORD"));
            assert!(out.contains("\"adapter\""));
            assert!(!out.to_ascii_lowercase().contains("lumi"));
        }
    }
}
