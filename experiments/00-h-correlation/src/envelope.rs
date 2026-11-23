//! The shared event envelope from `docs/00-phase0-protocol.md` §2, plus the
//! classification helpers the correlator needs (source class, type class).

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "shell.bash")]
    ShellBash,
    #[serde(rename = "shell.zsh")]
    ShellZsh,
    #[serde(rename = "shell.fish")]
    ShellFish,
    #[serde(rename = "script")]
    Script,
    #[serde(rename = "pty")]
    Pty,
    #[serde(rename = "proc")]
    Proc,
    #[serde(rename = "fs")]
    Fs,
    #[serde(rename = "recovery")]
    Recovery,
    #[serde(rename = "policy")]
    Policy,
}

/// Independence class. Two claims can only corroborate each other if they come
/// from *different* classes AND neither was derived from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    /// Kernel-observed process facts (procfs, proc connector).
    KernelProc,
    /// Kernel-observed filesystem facts (inotify, fanotify).
    KernelFs,
    /// Self-reported by a shell hook, a script, or the user.
    Declared,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::ShellBash => "shell.bash",
            Source::ShellZsh => "shell.zsh",
            Source::ShellFish => "shell.fish",
            Source::Script => "script",
            Source::Pty => "pty",
            Source::Proc => "proc",
            Source::Fs => "fs",
            Source::Recovery => "recovery",
            Source::Policy => "policy",
        }
    }

    pub fn class(self) -> SourceClass {
        match self {
            Source::Proc => SourceClass::KernelProc,
            Source::Fs => SourceClass::KernelFs,
            Source::Recovery | Source::Policy => SourceClass::Declared,
            Source::ShellBash
            | Source::ShellZsh
            | Source::ShellFish
            | Source::Script
            | Source::Pty => SourceClass::Declared,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStatus {
    Observed,
    Declared,
    Inferred,
}

impl EvidenceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceStatus::Observed => "observed",
            EvidenceStatus::Declared => "declared",
            EvidenceStatus::Inferred => "inferred",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: i64,
    pub event_id: String,
    pub source: Source,
    pub session_id: Option<String>,
    pub sequence: u64,
    pub ts_mono_ns: u64,
    pub ts_wall: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub payload: serde_json::Value,
    pub evidence_status: EvidenceStatus,
}

impl Event {
    // Nine independent envelope fields, fixed by docs/00-phase0-protocol.md §2.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: impl Into<String>,
        source: Source,
        session_id: Option<String>,
        sequence: u64,
        ts_mono_ns: u64,
        type_: impl Into<String>,
        payload: serde_json::Value,
        evidence_status: EvidenceStatus,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event_id: event_id.into(),
            source,
            session_id,
            sequence,
            ts_mono_ns,
            ts_wall: crate::util::iso8601_from_unix_ms(
                1_763_942_400_000 + u128::from(ts_mono_ns / 1_000_000),
            ),
            type_: type_.into(),
            payload,
            evidence_status,
        }
    }

    pub fn payload_str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(serde_json::Value::as_str)
    }

    pub fn payload_u64(&self, key: &str) -> Option<u64> {
        self.payload.get(key).and_then(serde_json::Value::as_u64)
    }

    pub fn pid_key(&self) -> Option<crate::identity::ProcKey> {
        let pid = self.payload_u64("pid")? as u32;
        let starttime = self.payload_u64("starttime")?;
        Some(crate::identity::ProcKey { pid, starttime })
    }

    /// Coarse type class: the level at which two different sources can be
    /// talking about the same fact (`CommandFinished` vs `ProcessExit`).
    pub fn type_class(&self) -> &'static str {
        match self.type_.as_str() {
            "CommandStarted" | "ProcessStart" | "ProcessExec" => "proc.start",
            "CommandFinished" | "ProcessExit" => "proc.end",
            "FileModified" => "fs.modify",
            "FileCreated" => "fs.create",
            "FileDeleted" => "fs.delete",
            "FileRenamed" => "fs.rename",
            "SessionStart" => "session.start",
            "SessionEnd" => "session.end",
            _ => "other",
        }
    }

    /// The subject a duplicate of this event would have to share: normalized
    /// command text for process events, path for filesystem events.
    pub fn subject(&self) -> Option<String> {
        match self.type_class() {
            "proc.start" | "proc.end" => self
                .payload_str("command")
                .or_else(|| self.payload_str("argv0"))
                .map(crate::util::normalize_ws),
            "fs.modify" | "fs.create" | "fs.delete" | "fs.rename" => {
                self.payload_str("path").map(str::to_string)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_classes_are_stable() {
        assert_eq!(Source::Proc.class(), SourceClass::KernelProc);
        assert_eq!(Source::Fs.class(), SourceClass::KernelFs);
        assert_eq!(Source::ShellBash.class(), SourceClass::Declared);
        assert_ne!(Source::Proc.class(), Source::Fs.class());
    }

    #[test]
    fn hook_and_procfs_end_share_a_type_class() {
        let a = Event::new(
            "a",
            Source::ShellBash,
            Some("s".into()),
            1,
            0,
            "CommandFinished",
            serde_json::json!({}),
            EvidenceStatus::Declared,
        );
        let b = Event::new(
            "b",
            Source::Proc,
            Some("s".into()),
            2,
            0,
            "ProcessExit",
            serde_json::json!({}),
            EvidenceStatus::Observed,
        );
        assert_eq!(a.type_class(), b.type_class());
    }
}
