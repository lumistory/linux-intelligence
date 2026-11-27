//! `li query` — read-only query API (P1-22 / P1-23).
//!
//! Every returned event carries `evidence_status`; every relation carries its
//! `credibility_level`; no output path may ever emit the word `"caused"`.

use std::path::Path;

use li_journal::read::{EventRecord, RelationRecord};
use li_journal::{Config, Journal};
use serde::Serialize;

use crate::client::resolve_journal_path;

#[derive(Debug, Serialize)]
pub struct QueryOutput {
    pub events: Vec<EventRecord>,
    pub relations: Vec<RelationRecord>,
}

/// Hard output contract guard (P1-23 / spec §6.7).
pub fn assert_no_caused(text: &str) -> Result<(), String> {
    if text.to_ascii_lowercase().contains("caused") {
        return Err("contract violation: query output contained forbidden word `caused`".into());
    }
    Ok(())
}

pub fn render_query(
    journal_path: Option<&Path>,
    session: Option<&str>,
    path: Option<&str>,
    event_type: Option<&str>,
    limit: i64,
    as_json: bool,
) -> Result<String, String> {
    let jpath = resolve_journal_path(journal_path)?;
    if !jpath.exists() {
        return Err(format!("journal does not exist: {}", jpath.display()));
    }
    let j = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
    let reader = j.reader().map_err(|e| e.to_string())?;
    let events = reader
        .query_events(session, path, event_type, limit)
        .map_err(|e| e.to_string())?;
    let relations = if let Some(first) = events.first() {
        reader
            .query_relations(Some(&first.event_id), limit)
            .map_err(|e| e.to_string())?
    } else {
        reader
            .query_relations(None, limit)
            .map_err(|e| e.to_string())?
    };

    let rendered = if as_json {
        let out = QueryOutput { events, relations };
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    } else {
        let mut buf = String::new();
        for ev in &events {
            buf.push_str(&format!(
                "[#{} {}] type={} evidence_status={} source={} session={} seq={}\n",
                ev.ingest_seq,
                ev.ts_wall,
                ev.event_type,
                ev.evidence_status,
                ev.source,
                ev.session_id.as_deref().unwrap_or("-"),
                ev.sequence
            ));
        }
        for rel in &relations {
            buf.push_str(&format!(
                "relation #{}: {} -[{}]-> {} (credibility_level={}, rule={})\n",
                rel.relation_id,
                rel.from_event,
                rel.kind,
                rel.to_event,
                rel.credibility_level,
                rel.rule
            ));
        }
        if buf.is_empty() {
            buf.push_str("(no matching events or relations)\n");
        }
        buf
    };
    assert_no_caused(&rendered)?;
    Ok(rendered)
}
