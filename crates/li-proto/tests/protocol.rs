//! Contract tests for `li-proto`.
//!
//! Every test here asserts something a Phase 0 measurement or a clause of
//! `docs/ipc-protocol-v0.md` claims, and would fail if that claim were
//! violated. Nothing asserts a value against itself.

use li_proto::event::{Order, SemanticFingerprint, SourceClass};
use li_proto::{
    CURRENT_SCHEMA_VERSION, ClientMessage, CredibilityLevel, ErrorCode, Event, EventId, EventKind,
    EventType, EvidenceStatus, MAX_FRAME_BYTES, ProtocolError, SchemaVersion,
    SemanticFingerprint as Fp, SessionId, Source, decode_client_message, decode_server_message,
    encode_frame,
};

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

const TS: &str = "2026-11-23T18:00:00.000Z";

fn id(s: &str) -> EventId {
    EventId::new(s).expect("valid event id")
}

fn sid(s: &str) -> SessionId {
    SessionId::new(s).expect("valid session id")
}

fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match v {
        serde_json::Value::Object(m) => m,
        other => panic!("expected an object, got {other}"),
    }
}

fn ev(kind: EventKind, seq: u64, payload: serde_json::Value) -> Event {
    Event::new(
        id(&format!("ev-{}-{seq}", kind.as_str())),
        Source::ShellBash,
        Some(sid("sess-a")),
        seq,
        // Saturating on purpose: the u64::MAX sequence test would otherwise
        // overflow the fixture clock rather than test the field under test.
        1_000_000_000u64.saturating_add(seq),
        TS,
        EventType::new(kind.as_str()).unwrap(),
        obj(payload),
        EvidenceStatus::Declared,
    )
    .expect("valid envelope")
}

/// The representative payload for each event type. The point is a *faithful*
/// payload per type — the shapes Phase 0 actually emitted — not a placeholder,
/// because a placeholder round-trip test proves nothing about the real fields.
fn sample_payload(kind: EventKind) -> serde_json::Value {
    match kind {
        EventKind::SessionStarted => serde_json::json!({
            "shell": "bash", "version": "5.3.9",
            "capabilities": {"command_text": true, "internal_script_commands": false}
        }),
        EventKind::SessionEnded => serde_json::json!({"reason": "exit", "commands": 41}),
        EventKind::CommandStarted => serde_json::json!({
            "command": "git status", "cwd": "<HOME>/src", "pid": 4242, "starttime": 900
        }),
        EventKind::CommandFinished => serde_json::json!({
            "command": "git status", "exit_status": 0, "duration_ns": 12_345_000u64,
            "pid": 4242, "starttime": 900
        }),
        EventKind::ScriptStarted => {
            serde_json::json!({"argv": ["deploy.sh", "--fast"], "tier": "lifecycle"})
        }
        EventKind::ScriptFinished => {
            serde_json::json!({"argv": ["deploy.sh", "--fast"], "exit_status": 2})
        }
        EventKind::ProcessCreated => serde_json::json!({"pid": 4243, "ppid": 4242, "comm": "make"}),
        EventKind::ProcessExited => {
            serde_json::json!({"pid": 4243, "starttime": 901, "ambiguous": false})
        }
        EventKind::FileCreated => serde_json::json!({"path": "/tmp/li/a.txt"}),
        EventKind::FileModified => {
            serde_json::json!({"path": "/tmp/li/a.txt", "masks": ["IN_MODIFY"]})
        }
        EventKind::FileDeleted => serde_json::json!({"path": "/tmp/li/a.txt"}),
        EventKind::FileRenamed => {
            serde_json::json!({"from": "/tmp/li/a.tmp", "to": "/tmp/li/a.txt"})
        }
        EventKind::SnapshotCreated => serde_json::json!({
            "provider": "git", "checkpoint": "ck-1", "caveats": ["tracked files only"]
        }),
        EventKind::RiskDetected => serde_json::json!({
            "level": "L2", "rule": "rm-rf-tmp", "policy_version": 1
        }),
        EventKind::ApprovalGranted => serde_json::json!({"request": "req-9", "scope": ["/tmp/li"]}),
        EventKind::RecoveryStarted => serde_json::json!({"provider": "git", "checkpoint": "ck-1"}),
        EventKind::RecoveryCompleted => serde_json::json!({
            "provider": "git", "checkpoint": "ck-1", "verified": true
        }),
        EventKind::DaemonRestarted => serde_json::json!({
            "previous_boot": "boot-1", "boot_id": "boot-2",
            "reconciled_sessions": ["sess-a"], "unexplained_gaps": 0
        }),
        EventKind::LateArrival => serde_json::json!({
            "watermark": 41u64, "got": 12u64,
            "uncertainty_ns": 10_000_000u64
        }),
    }
}

// ---------------------------------------------------------------------------
// 1. Round trip, for every event type
// ---------------------------------------------------------------------------

#[test]
fn every_known_event_type_round_trips_byte_identically() {
    assert_eq!(EventKind::ALL.len(), 19, "the vocabulary list drifted");

    for kind in EventKind::ALL {
        let original = ev(kind, 7, sample_payload(kind));
        let line = original.to_json_line().expect("encode");

        let decoded = match decode_client_message(&line).expect("decode") {
            ClientMessage::Event(e) => *e,
            other => panic!("{} decoded as {other:?}", kind.as_str()),
        };

        assert_eq!(decoded, original, "{} lost information", kind.as_str());
        assert_eq!(decoded.kind(), Some(kind), "{} reclassified", kind.as_str());
        // Re-encoding the decoded value must reproduce the same bytes: that is
        // what makes a frame worth hashing and a journal line diffable.
        assert_eq!(
            decoded.to_json_line().unwrap(),
            line,
            "{} is not encoding-stable",
            kind.as_str()
        );
    }
}

#[test]
fn every_event_type_name_maps_to_exactly_one_kind() {
    // Two kinds sharing a wire string would make `from_wire` order-dependent.
    let mut seen: Vec<&str> = Vec::new();
    for kind in EventKind::ALL {
        let name = kind.as_str();
        assert!(!seen.contains(&name), "duplicate wire name {name}");
        seen.push(name);
        assert_eq!(EventKind::from_wire(name), Some(kind));
    }
}

#[test]
fn the_phase0_short_spellings_are_aliases_but_not_the_wire_string() {
    // 0-H emitted SessionStart/SessionEnd/ProcessStart/ProcessExit. They must
    // still classify; the stored `type` must not be rewritten.
    for (alias, kind) in [
        ("SessionStart", EventKind::SessionStarted),
        ("SessionEnd", EventKind::SessionEnded),
        ("ProcessStart", EventKind::ProcessCreated),
        ("ProcessExit", EventKind::ProcessExited),
    ] {
        let t = EventType::new(alias).unwrap();
        assert_eq!(t.kind(), Some(kind));
        assert_eq!(t.as_str(), alias, "the alias must not be rewritten");
    }
    // exec is NOT creation: merging them would assert an unobserved fork edge.
    assert_eq!(EventKind::from_wire("ProcessExec"), None);
}

#[test]
fn an_unknown_type_is_stored_verbatim_not_rejected() {
    let line = r#"{"schema_version":1,"event_id":"e1","source":"fs","sequence":0,"ts_mono_ns":5,"ts_wall":"2026-11-23T18:00:00.000Z","type":"SomeFutureThing","payload":{"k":1},"evidence_status":"observed"}"#;
    let ClientMessage::Event(e) = decode_client_message(line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(e.event_type.as_str(), "SomeFutureThing");
    assert_eq!(e.kind(), None);
    assert_eq!(
        e.to_json_line().unwrap(),
        line,
        "unknown types must round-trip"
    );
}

// ---------------------------------------------------------------------------
// 2. schema_version rejection
// ---------------------------------------------------------------------------

#[test]
fn the_current_version_constant_is_one_and_the_wire_says_so() {
    assert_eq!(CURRENT_SCHEMA_VERSION, 1);
    assert_eq!(SchemaVersion::CURRENT.get(), 1);
    let line = ev(EventKind::CommandStarted, 0, serde_json::json!({}))
        .to_json_line()
        .unwrap();
    assert!(line.starts_with(r#"{"schema_version":1,"#), "got: {line}");
}

#[test]
fn a_future_schema_version_is_rejected_on_every_frame_kind() {
    let cases = [
        // event
        r#"{"schema_version":2,"event_id":"e1","source":"fs","sequence":0,"ts_mono_ns":5,"ts_wall":"2026-11-23T18:00:00.000Z","type":"FileModified","payload":{},"evidence_status":"observed"}"#,
        // ping
        r#"{"schema_version":2,"type":"Ping","nonce":1}"#,
        // neither discriminator: must be a version error, not missing_type
        r#"{"schema_version":2}"#,
        // structurally invalid *and* future: version still wins
        r#"{"schema_version":2,"event_id":123}"#,
    ];
    for line in cases {
        let err = decode_client_message(line).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::SchemaVersionUnsupported(2),
            "frame {line} produced {err}"
        );
        assert_eq!(err.code(), ErrorCode::SchemaVersionUnsupported);
    }
}

#[test]
fn schema_version_zero_is_rejected_because_v0_never_existed_on_the_wire() {
    let err = decode_client_message(r#"{"schema_version":0,"type":"Ping","nonce":1}"#).unwrap_err();
    assert_eq!(err, ProtocolError::SchemaVersionUnsupported(0));
}

#[test]
fn a_missing_or_non_integer_schema_version_is_invalid_envelope_not_accepted() {
    for line in [
        r#"{"event_id":"e1"}"#,
        r#"{"schema_version":"1","type":"Ping","nonce":1}"#,
        r#"{"schema_version":1.5,"type":"Ping","nonce":1}"#,
        r#"{"schema_version":-1,"type":"Ping","nonce":1}"#,
    ] {
        let err = decode_client_message(line).unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::InvalidEnvelope,
            "frame {line} -> {err}"
        );
    }
}

#[test]
fn the_server_frame_decoders_reject_a_future_version_too() {
    let pong = r#"{"schema_version":9,"nonce":1,"watermarks":[],"counts":{"ingested":0,"dedup_dropped":0,"seq_violations":0,"rejected":0},"server_schema_version":9}"#;
    assert_eq!(
        decode_server_message(pong).unwrap_err(),
        ProtocolError::SchemaVersionUnsupported(9)
    );
}

// ---------------------------------------------------------------------------
// 3. Five credibility levels + three evidence statuses, stable strings
// ---------------------------------------------------------------------------

#[test]
fn all_five_credibility_levels_round_trip_as_lowercase_strings() {
    assert_eq!(CredibilityLevel::ALL.len(), 5);
    for level in CredibilityLevel::ALL {
        let json = encode_frame(&level).unwrap();
        assert_eq!(
            json,
            format!("\"{}\"", level.as_str()),
            "the wire string must be the pinned contract, not a Debug rendering"
        );
        // Not "Confirmed": a capitalised or Debug form is a contract break.
        assert!(json.chars().all(|c| !c.is_ascii_uppercase()), "{json}");
        let back: CredibilityLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, level);
    }
    // The exact vocabulary 0-H §4.4 measured.
    assert_eq!(
        CredibilityLevel::ALL.map(|l| l.as_str()),
        ["confirmed", "strong", "probable", "weak", "unknown"]
    );
}

#[test]
fn credibility_levels_are_ordered_unknown_to_confirmed_for_monotonicity() {
    // 0-H: adding a corroboration must never lower the level, so the type has
    // to make that a `>=` comparison.
    assert!(CredibilityLevel::Unknown < CredibilityLevel::Weak);
    assert!(CredibilityLevel::Weak < CredibilityLevel::Probable);
    assert!(CredibilityLevel::Probable < CredibilityLevel::Strong);
    assert!(CredibilityLevel::Strong < CredibilityLevel::Confirmed);
}

#[test]
fn all_three_evidence_statuses_round_trip_as_lowercase_strings() {
    assert_eq!(EvidenceStatus::ALL.len(), 3);
    for status in EvidenceStatus::ALL {
        let json = encode_frame(&status).unwrap();
        assert_eq!(json, format!("\"{}\"", status.as_str()));
        let back: EvidenceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }
    assert_eq!(
        EvidenceStatus::ALL.map(|s| s.as_str()),
        ["observed", "declared", "inferred"]
    );
}

#[test]
fn an_event_carrying_each_evidence_status_round_trips() {
    for status in EvidenceStatus::ALL {
        let mut e = ev(
            EventKind::FileModified,
            0,
            serde_json::json!({"path": "/tmp/x"}),
        );
        e.evidence_status = status;
        let line = e.to_json_line().unwrap();
        assert!(line.contains(&format!(r#""evidence_status":"{status}""#)));
        let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
            panic!("expected an event")
        };
        assert_eq!(back.evidence_status, status);
    }
}

// ---------------------------------------------------------------------------
// 4. Gold frame: byte-exact
// ---------------------------------------------------------------------------

const GOLD_LINE: &str = concat!(
    r#"{"schema_version":1,"#,
    r#""event_id":"0192f0c4-6a1b-7c3d-8e9f-0a1b2c3d4e5f","#,
    r#""source":"shell.bash","#,
    r#""session_id":"sess-7f3a","#,
    r#""sequence":42,"#,
    r#""ts_mono_ns":1234567890123,"#,
    r#""ts_wall":"2026-11-23T18:00:00.000Z","#,
    r#""type":"CommandFinished","#,
    r#""payload":{"command":"rm -rf build","cwd":"<HOME>/src","exit_status":0},"#,
    r#""evidence_status":"declared"}"#
);

fn gold_event() -> Event {
    Event::new(
        id("0192f0c4-6a1b-7c3d-8e9f-0a1b2c3d4e5f"),
        Source::ShellBash,
        Some(sid("sess-7f3a")),
        42,
        1_234_567_890_123,
        "2026-11-23T18:00:00.000Z",
        EventType::new("CommandFinished").unwrap(),
        obj(serde_json::json!({
            "command": "rm -rf build",
            "exit_status": 0,
            "cwd": "<HOME>/src"
        })),
        EvidenceStatus::Declared,
    )
    .unwrap()
}

#[test]
fn gold_frame_matches_byte_for_byte() {
    assert_eq!(gold_event().to_json_line().unwrap(), GOLD_LINE);
}

#[test]
fn gold_frame_is_a_single_line_and_the_only_newline_the_reader_sees_is_the_fence() {
    assert_eq!(GOLD_LINE.matches('\n').count(), 0);
    assert!(!GOLD_LINE.ends_with('\n'));
    assert!(
        GOLD_LINE.is_ascii(),
        "every byte of the frame must be ASCII"
    );
}

#[test]
fn gold_frame_decodes_and_reencodes_to_itself() {
    let ClientMessage::Event(back) = decode_client_message(GOLD_LINE).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(*back, gold_event());
    assert_eq!(back.to_json_line().unwrap(), GOLD_LINE);
}

#[test]
fn the_optional_fingerprint_appears_only_when_present() {
    // Absent → absent. Not `null`. A `null` would make "no fingerprint" and
    // "a producer that cannot compute one" distinguishable on the wire for no
    // benefit, and would grow every frame.
    assert!(!GOLD_LINE.contains("semantic_fingerprint"));

    let mut e = gold_event();
    e.semantic_fingerprint = Some(Fp {
        type_class: "proc.end".into(),
        session: Some(sid("sess-7f3a")),
        subject: "rm -rf build".into(),
        bucket_ns: 2,
    });
    let line = e.to_json_line().unwrap();
    assert!(line.contains(
        r#""semantic_fingerprint":{"type_class":"proc.end","session":"sess-7f3a","subject":"rm -rf build","bucket_ns":2}"#
    ));
    let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(*back, e);
}

#[test]
fn unknown_payload_keys_survive_and_do_not_affect_the_envelope() {
    // Forward compatibility for payloads: the protocol says unknown payload keys
    // must be ignored by consumers, which requires keeping them.
    let mut e = gold_event();
    e.payload
        .insert("future_field".into(), serde_json::json!({"nested": [1, 2]}));
    let line = e.to_json_line().unwrap();
    let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(
        back.payload.get("future_field"),
        Some(&serde_json::json!({"nested": [1, 2]}))
    );
}

// ---------------------------------------------------------------------------
// 5. Unknown fields: the decided behaviour, both directions
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_envelope_field_is_rejected_loudly() {
    // Decided policy: the envelope is a closed 9-field set. A silently ignored
    // envelope field is a silent data bug — e.g. a producer spelling `seq` would
    // have every event stored with `sequence: 0` and the loss detector would
    // report gaps that never happened.
    let line = GOLD_LINE.replace(
        r#""sequence":42,"#,
        r#""sequence":42,"future_envelope_key":true,"#,
    );
    let err = decode_client_message(&line).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidEnvelope, "{err}");
    assert!(err.to_string().contains("future_envelope_key"), "{err}");
}

#[test]
fn an_unknown_key_inside_the_fingerprint_is_rejected_too() {
    let mut e = gold_event();
    e.semantic_fingerprint = Some(SemanticFingerprint {
        type_class: "fs.modify".into(),
        session: None,
        subject: "/tmp/x".into(),
        bucket_ns: 1,
    });
    let line = e
        .to_json_line()
        .unwrap()
        .replace(r#""bucket_ns":1"#, r#""bucket_ns":1,"extra":1"#);
    assert_eq!(
        decode_client_message(&line).unwrap_err().code(),
        ErrorCode::InvalidEnvelope
    );
}

#[test]
fn a_fingerprint_without_a_type_class_is_rejected() {
    let line = GOLD_LINE.replace(
        r#""evidence_status":"declared""#,
        r#""evidence_status":"declared","semantic_fingerprint":{"type_class":"","subject":"x","bucket_ns":0}"#,
    );
    let err = decode_client_message(&line).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidEnvelope);
    assert!(err.to_string().contains("type_class"), "{err}");
}

#[test]
fn dropping_an_optional_field_is_a_downgrade_that_still_reads() {
    // The one direction that must not be strict: an event without
    // `session_id` / `semantic_fingerprint` is legal (proc/inotify events have
    // no session), and it must decode.
    let line = concat!(
        r#"{"schema_version":1,"event_id":"e1","source":"fs","sequence":0,"#,
        r#""ts_mono_ns":5,"ts_wall":"2026-11-23T18:00:00.000Z","type":"FileModified","#,
        r#""payload":{"path":"/tmp/x"},"evidence_status":"observed"}"#
    );
    let ClientMessage::Event(e) = decode_client_message(line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(e.session_id, None);
    assert_eq!(e.semantic_fingerprint, None);
    assert_eq!(e.source_class(), SourceClass::KernelFs);
}

// ---------------------------------------------------------------------------
// 6. sequence typed as u64 starting at 0 — the loss-detection contract
// ---------------------------------------------------------------------------

#[test]
fn sequence_zero_is_legal_and_means_the_first_event_of_a_session() {
    // `NonZeroU64` would be wrong: 0-C §3/§8 fixes "starts at 0, +1 per event".
    let e = ev(EventKind::SessionStarted, 0, serde_json::json!({}));
    assert_eq!(e.sequence, 0);
    let line = e.to_json_line().unwrap();
    let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(back.sequence, 0);
}

#[test]
fn a_sequence_gap_is_accepted_because_the_client_advances_on_drop() {
    // C1/C2 + 0-C §8: a gap means "the client chose not to send this". The
    // receiver must journal it and must not reject or reorder around it.
    let a = ev(
        EventKind::CommandStarted,
        10,
        serde_json::json!({"command": "a"}),
    );
    let b = ev(
        EventKind::CommandStarted,
        14,
        serde_json::json!({"command": "b"}),
    );
    for e in [&a, &b] {
        let ClientMessage::Event(back) = decode_client_message(&e.to_json_line().unwrap()).unwrap()
        else {
            panic!("expected an event")
        };
        assert_eq!(back.sequence, e.sequence);
    }
    assert_eq!(b.sequence - a.sequence, 4, "the gap is data, not an error");
}

#[test]
fn u64_max_sequence_is_representable() {
    let mut e = ev(EventKind::CommandFinished, u64::MAX, serde_json::json!({}));
    e.event_id = id("ev-max");
    let line = e.to_json_line().unwrap();
    assert!(line.contains("\"sequence\":18446744073709551615"));
    let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(back.sequence, u64::MAX);
}

#[test]
fn a_sequence_without_a_session_is_refused() {
    // Otherwise a fabricated session ("") would show a gap of 42 in an audit.
    let err = Event::new(
        id("e1"),
        Source::Fs,
        None,
        42,
        1,
        TS,
        EventType::new("FileModified").unwrap(),
        obj(serde_json::json!({"path": "/tmp/x"})),
        EvidenceStatus::Observed,
    )
    .unwrap_err();
    assert!(err.to_string().contains("sequence"), "{err}");

    let line = GOLD_LINE.replace(r#""session_id":"sess-7f3a","#, "");
    let err = decode_client_message(&line).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidEnvelope);
    assert!(err.to_string().contains("sequence"), "{err}");
}

// ---------------------------------------------------------------------------
// 7. Timestamps
// ---------------------------------------------------------------------------

#[test]
fn ts_mono_ns_is_a_full_width_u64_and_not_a_duration() {
    // CLOCK_MONOTONIC ns on a long-uptime host exceeds u32 and, at ~10 years,
    // exceeds 2^53 — so it must not be carried as a JSON number a JS client
    // would round. Here it is an integer and we assert exactness at the edges.
    let mut e = ev(EventKind::CommandStarted, 0, serde_json::json!({}));
    e.ts_mono_ns = 9_007_199_254_740_993; // 2^53 + 1
    let line = e.to_json_line().unwrap();
    let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
        panic!("expected an event")
    };
    assert_eq!(back.ts_mono_ns, 9_007_199_254_740_993);
}

#[test]
fn ts_wall_must_be_rfc3339_utc_with_milliseconds_and_z() {
    for bad in [
        "2026-11-23T18:00:00Z",          // no milliseconds
        "2026-11-23T18:00:00.000+08:00", // not UTC
        "2026-11-23 18:00:00.000Z",      // space instead of T
        "2026-11-23T18:00:00.000000Z",   // nanoseconds: fabricated precision
        "1759000000",                    // unix seconds
    ] {
        let err = Event::new(
            id("e1"),
            Source::Fs,
            None,
            0,
            1,
            bad,
            EventType::new("FileModified").unwrap(),
            obj(serde_json::json!({"path": "/tmp/x"})),
            EvidenceStatus::Observed,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("ts_wall"),
            "ts_wall {bad} was accepted or misreported: {err}"
        );
    }
}

#[test]
fn ts_wall_sorts_lexicographically_like_time_because_it_is_fixed_width() {
    let mut moments = [
        "2026-11-23T18:00:00.010Z",
        "2026-11-23T18:00:00.002Z",
        "2026-11-24T00:00:00.000Z",
        "2026-11-23T09:59:59.999Z",
    ];
    moments.sort_unstable();
    assert_eq!(
        moments,
        [
            "2026-11-23T09:59:59.999Z",
            "2026-11-23T18:00:00.002Z",
            "2026-11-23T18:00:00.010Z",
            "2026-11-24T00:00:00.000Z",
        ]
    );
}

// ---------------------------------------------------------------------------
// 8. Ordering, including the case that is not knowable
// ---------------------------------------------------------------------------

#[test]
fn within_a_session_sequence_decides_order_exactly() {
    let a = ev(EventKind::CommandStarted, 1, serde_json::json!({}));
    let b = ev(EventKind::CommandFinished, 2, serde_json::json!({}));
    assert_eq!(a.order_vs(&b), Order::Before);
    assert_eq!(b.order_vs(&a), Order::After);
}

#[test]
fn across_source_classes_closer_than_one_tick_the_order_is_ambiguous() {
    // 0-H §8.1.4: cross-source order carries a ±1 tick (10 ms) uncertainty and
    // must not be claimed better than that.
    let mut hook = ev(EventKind::CommandFinished, 0, serde_json::json!({}));
    hook.ts_mono_ns = 100_000_000;
    let mut fs = ev(
        EventKind::FileModified,
        0,
        serde_json::json!({"path": "/tmp/x"}),
    );
    fs.source = Source::Fs;
    fs.session_id = None;
    fs.ts_mono_ns = 100_000_000 + 9_999_999; // < 10 ms apart
    assert_eq!(hook.order_vs(&fs), Order::Ambiguous);
    assert_eq!(fs.order_vs(&hook), Order::Ambiguous);

    fs.ts_mono_ns = 100_000_000 + 10_000_000; // exactly one tick
    assert_eq!(hook.order_vs(&fs), Order::Before);
}

#[test]
fn source_classes_split_into_the_three_independence_classes() {
    assert_eq!(Source::Proc.class(), SourceClass::KernelProc);
    assert_eq!(Source::Fs.class(), SourceClass::KernelFs);
    for declared in [
        Source::ShellBash,
        Source::ShellZsh,
        Source::ShellFish,
        Source::Script,
        Source::Pty,
        Source::Recovery,
        Source::Policy,
    ] {
        assert_eq!(declared.class(), SourceClass::Declared, "{declared:?}");
    }
    // Two bash hooks firing twice are ONE source (0-H S12 = Weak).
    assert_eq!(Source::ShellBash.class(), Source::ShellZsh.class());
}

#[test]
fn every_source_spelling_is_stable_on_the_wire() {
    let all = [
        (Source::ShellBash, "shell.bash"),
        (Source::ShellZsh, "shell.zsh"),
        (Source::ShellFish, "shell.fish"),
        (Source::Script, "script"),
        (Source::Pty, "pty"),
        (Source::Proc, "proc"),
        (Source::Fs, "fs"),
        (Source::Recovery, "recovery"),
        (Source::Policy, "policy"),
    ];
    for (source, wire) in all {
        assert_eq!(source.as_str(), wire);
        assert_eq!(encode_frame(&source).unwrap(), format!("\"{wire}\""));
        let back: Source = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
        assert_eq!(back, source);
    }
}

// ---------------------------------------------------------------------------
// 9. Frame size
// ---------------------------------------------------------------------------

#[test]
fn a_frame_at_the_measured_limit_is_encodable_and_one_over_is_not() {
    // 0-C test 3: 1 MiB per frame, connection survives a 10 MiB reject.
    let mut e = ev(EventKind::CommandFinished, 0, serde_json::json!({}));
    let base = e.to_json_line().unwrap().len();
    let filler = MAX_FRAME_BYTES - base - r#""blob":"""#.len() - 16;
    e.payload
        .insert("blob".into(), serde_json::json!("a".repeat(filler)));
    let len = e.to_json_line().unwrap().len();
    assert!(len <= MAX_FRAME_BYTES, "len {len}");

    e.payload
        .insert("blob".into(), serde_json::json!("a".repeat(filler + 64)));
    assert!(matches!(
        e.to_json_line().unwrap_err(),
        ProtocolError::PayloadTooLarge(_)
    ));
}

// ---------------------------------------------------------------------------
// 10. Errors map to the documented codes
// ---------------------------------------------------------------------------

#[test]
fn every_error_kind_reports_a_code_from_the_documented_table() {
    let cases: Vec<(ProtocolError, ErrorCode)> = vec![
        (
            ProtocolError::InvalidJson("x".into()),
            ErrorCode::InvalidJson,
        ),
        (
            ProtocolError::InvalidEnvelope {
                field: "event_id",
                reason: "x".into(),
            },
            ErrorCode::InvalidEnvelope,
        ),
        (ProtocolError::MissingType, ErrorCode::MissingType),
        (ProtocolError::UnknownFrameType, ErrorCode::UnknownFrameType),
        (
            ProtocolError::SchemaVersionUnsupported(2),
            ErrorCode::SchemaVersionUnsupported,
        ),
        (
            ProtocolError::PayloadTooLarge(1),
            ErrorCode::PayloadTooLarge,
        ),
        (ProtocolError::FrameTooLarge(1), ErrorCode::PayloadTooLarge),
    ];
    for (err, code) in cases {
        assert_eq!(err.code(), code, "{err}");
        assert!(!err.to_string().is_empty());
    }
}

#[test]
fn an_error_frame_built_from_a_protocol_error_keeps_the_specific_cause() {
    let err = ProtocolError::SchemaVersionUnsupported(3);
    let frame = li_proto::ErrorFrame::from_error(&err);
    assert_eq!(frame.code, ErrorCode::SchemaVersionUnsupported);
    assert!(frame.message.contains("got 3"), "{}", frame.message);
}

#[test]
fn identifiers_reject_what_a_journal_could_not_store() {
    for bad in ["", "with space", "tab\there", "line\nbreak"] {
        assert!(EventId::new(bad).is_err(), "event_id {bad:?} was accepted");
        assert!(
            SessionId::new(bad).is_err(),
            "session_id {bad:?} was accepted"
        );
    }
    assert!(EventId::new("a".repeat(129)).is_err());
    assert!(EventId::new("a".repeat(128)).is_ok());
}

// ---------------------------------------------------------------------------
// 11. daemon-restart reconciliation surface (spec §3, 0-H §4.2)
// ---------------------------------------------------------------------------

#[test]
fn a_daemon_restarted_event_can_carry_the_reconciliation_verdict() {
    let e = ev(
        EventKind::DaemonRestarted,
        0,
        sample_payload(EventKind::DaemonRestarted),
    );
    let ClientMessage::Event(back) = decode_client_message(&e.to_json_line().unwrap()).unwrap()
    else {
        panic!("expected an event")
    };
    assert_eq!(
        back.payload.get("unexplained_gaps"),
        Some(&serde_json::json!(0))
    );
}

#[test]
fn a_late_arrival_keeps_both_the_watermark_and_the_sequence_it_carries() {
    // 0-H §4.2: an event below the watermark cannot be reordered into place.
    // The frame must preserve both numbers so the reader can re-derive the
    // decision instead of trusting a label.
    let e = ev(
        EventKind::LateArrival,
        12,
        sample_payload(EventKind::LateArrival),
    );
    let ClientMessage::Event(back) = decode_client_message(&e.to_json_line().unwrap()).unwrap()
    else {
        panic!("expected an event")
    };
    assert_eq!(back.sequence, 12);
    assert_eq!(back.payload.get("watermark"), Some(&serde_json::json!(41)));
    assert_eq!(back.payload.get("got"), Some(&serde_json::json!(12)));
}

// ---------------------------------------------------------------------------
// 12. Server frames: round trip, and the C2 rule that an Ack is not proof
// ---------------------------------------------------------------------------

#[test]
fn an_ack_round_trips_and_carries_the_event_id_plus_sequence() {
    let ack = li_proto::Ack {
        schema_version: SchemaVersion::CURRENT,
        event_id: id("ev-1"),
        sequence: 7,
    };
    let line = encode_frame(&ack).unwrap();
    assert_eq!(
        decode_server_message(&line).unwrap(),
        li_proto::ServerMessage::Ack(ack)
    );
}

#[test]
fn a_pong_round_trips_with_watermarks_counts_and_the_server_version() {
    let pong = li_proto::Pong {
        schema_version: SchemaVersion::CURRENT,
        nonce: 42,
        watermarks: vec![
            li_proto::Watermark {
                session_id: sid("sess-a"),
                contiguous_through: Some(10),
                highest_seen: Some(12),
            },
            // A session whose first event has not arrived yet. `None` must mean
            // "never ingested", not "ingested up to 0" (0-H §4.2).
            li_proto::Watermark {
                session_id: sid("sess-b"),
                contiguous_through: None,
                highest_seen: None,
            },
        ],
        counts: li_proto::Counts {
            ingested: 11,
            dedup_dropped: 1,
            seq_violations: 2,
            rejected: 0,
        },
        server_schema_version: SchemaVersion::CURRENT,
    };
    let msg = li_proto::ServerMessage::Pong(Box::new(pong));
    let line = encode_frame(&msg).unwrap();
    assert_eq!(decode_server_message(&line).unwrap(), msg);
}

#[test]
fn a_pong_reports_the_server_schema_version_so_a_client_can_see_a_mismatch() {
    // docs/ipc-protocol-v0.md §5 lists this as TBD in 0-C; Phase 1 must carry
    // it, otherwise "no negotiation" means "no way to even find out".
    let line = concat!(
        r#"{"schema_version":1,"nonce":1,"watermarks":[],"#,
        r#""counts":{"ingested":0,"dedup_dropped":0,"seq_violations":0,"rejected":0},"#,
        r#""server_schema_version":2}"#
    );
    let li_proto::ServerMessage::Pong(p) = decode_server_message(line).unwrap() else {
        panic!("expected a pong")
    };
    assert_eq!(p.server_schema_version.get(), 2);
    assert_ne!(p.server_schema_version, SchemaVersion::CURRENT);
}

#[test]
fn only_the_documented_codes_close_the_connection() {
    // docs/ipc-protocol-v0.md §4: `frame_too_large` and `peer_uid_rejected` are
    // terminal; everything else leaves the connection usable.
    let terminal: Vec<ErrorCode> = [
        ErrorCode::PayloadTooLarge,
        ErrorCode::FrameTooLarge,
        ErrorCode::InvalidJson,
        ErrorCode::InvalidEnvelope,
        ErrorCode::MissingType,
        ErrorCode::UnknownFrameType,
        ErrorCode::SchemaVersionUnsupported,
        ErrorCode::PeerUidRejected,
        ErrorCode::Other,
    ]
    .into_iter()
    .filter(ErrorCode::closes_connection)
    .collect();
    assert_eq!(
        terminal,
        vec![ErrorCode::FrameTooLarge, ErrorCode::PeerUidRejected]
    );
}

#[test]
fn an_unrecognised_error_code_degrades_to_other_and_still_means_not_stored() {
    // A v1.x server invents a code. A v1 client must not fail to parse it, and
    // must conclude "not stored", and must not treat it as terminal.
    let line = r#"{"schema_version":1,"code":"some_future_code","message":"x"}"#;
    let li_proto::ServerMessage::Error(e) = decode_server_message(line).unwrap() else {
        panic!("expected an error frame")
    };
    assert_eq!(e.code, ErrorCode::Other);
    assert!(e.code.means_not_stored());
    assert!(!e.code.closes_connection());
}

#[test]
fn the_documented_error_codes_use_snake_case_tokens() {
    let expected = [
        (ErrorCode::PayloadTooLarge, "payload_too_large"),
        (ErrorCode::FrameTooLarge, "frame_too_large"),
        (ErrorCode::InvalidJson, "invalid_json"),
        (ErrorCode::InvalidEnvelope, "invalid_envelope"),
        (ErrorCode::MissingType, "missing_type"),
        (ErrorCode::UnknownFrameType, "unknown_frame_type"),
        (
            ErrorCode::SchemaVersionUnsupported,
            "schema_version_unsupported",
        ),
        (ErrorCode::PeerUidRejected, "peer_uid_rejected"),
    ];
    for (code, wire) in expected {
        assert_eq!(encode_frame(&code).unwrap(), format!("\"{wire}\""));
    }
}

#[test]
fn a_ping_round_trips_and_is_answered_by_nonce_not_by_arrival_order() {
    let ping = ClientMessage::Ping(li_proto::Ping {
        schema_version: SchemaVersion::CURRENT,
        nonce: 0xDEAD_BEEF,
    });
    let line = encode_frame(&ping).unwrap();
    assert_eq!(decode_client_message(&line).unwrap(), ping);
    assert!(line.contains(r#""type":"Ping""#));
    assert!(!line.contains("event_id"), "a ping is not an event");
}

#[test]
fn a_ping_without_a_nonce_is_rejected_rather_than_answered_with_zero() {
    // Answering with a defaulted nonce would let a client match the wrong
    // barrier, which is the only delivery proof the protocol has.
    let err = decode_client_message(r#"{"schema_version":1,"type":"Ping"}"#).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidEnvelope);
    assert!(err.to_string().contains("nonce"), "{err}");
}

#[test]
fn a_message_enum_encodes_exactly_like_the_frame_it_wraps() {
    // No `{"kind":...}` wrapper: the discriminator is the presence of
    // `event_id`, so a wrapper would break every Phase 0 client.
    let ev = gold_event();
    let line = encode_frame(&li_proto::ClientMessage::Event(Box::new(ev.clone()))).unwrap();
    assert_eq!(line, encode_frame(&ev).unwrap());
    assert_eq!(line, GOLD_LINE);

    let ack = li_proto::Ack {
        schema_version: SchemaVersion::CURRENT,
        event_id: id("ev-1"),
        sequence: 1,
    };
    assert_eq!(
        encode_frame(&li_proto::ServerMessage::Ack(ack.clone())).unwrap(),
        encode_frame(&ack).unwrap()
    );
    assert_eq!(li_proto::ServerMessage::Ack(ack).kind_str(), "Ack");
}

#[test]
fn a_ping_with_a_foreign_type_is_not_silently_read_as_a_barrier() {
    let err = decode_client_message(r#"{"schema_version":1,"type":"Pong","nonce":1}"#).unwrap_err();
    assert_eq!(err.code(), ErrorCode::UnknownFrameType);
}

#[test]
fn a_full_session_exchange_decodes_end_to_end() {
    // A realistic sequence: handshake, three events with one dropped (a gap),
    // then a barrier whose watermarks explain the gap. This is the shape the
    // daemon and hook must agree on, exercised without any I/O.
    let ping = encode_frame(&ClientMessage::Ping(li_proto::Ping {
        schema_version: SchemaVersion::CURRENT,
        nonce: 1,
    }))
    .unwrap();
    assert!(matches!(
        decode_client_message(&ping).unwrap(),
        ClientMessage::Ping(_)
    ));

    for seq in [0u64, 1, 3] {
        let e = ev(
            EventKind::CommandFinished,
            seq,
            sample_payload(EventKind::CommandFinished),
        );
        let ClientMessage::Event(back) = decode_client_message(&e.to_json_line().unwrap()).unwrap()
        else {
            panic!("expected an event")
        };
        assert_eq!(back.sequence, seq);
    }

    let pong = encode_frame(&li_proto::ServerMessage::Pong(Box::new(li_proto::Pong {
        schema_version: SchemaVersion::CURRENT,
        nonce: 1,
        watermarks: vec![li_proto::Watermark {
            session_id: sid("sess-a"),
            // contiguous stops at 1 because 2 was dropped by the client...
            contiguous_through: Some(1),
            // ...but 3 did arrive, so the highest seen is 3.
            highest_seen: Some(3),
        }],
        counts: li_proto::Counts {
            ingested: 3,
            dedup_dropped: 0,
            seq_violations: 1,
            rejected: 0,
        },
        server_schema_version: SchemaVersion::CURRENT,
    })))
    .unwrap();

    let li_proto::ServerMessage::Pong(p) = decode_server_message(&pong).unwrap() else {
        panic!("expected a pong")
    };
    let wm = &p.watermarks[0];
    assert_eq!(wm.contiguous_through, Some(1));
    assert_eq!(wm.highest_seen, Some(3));
    // The gap is derivable from the barrier alone: expected 3 arrived, max is 3.
    let expected = wm.highest_seen.unwrap() + 1 - p.counts.ingested;
    assert_eq!(expected, 1, "exactly one event was dropped by the client");
}
