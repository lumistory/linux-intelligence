//! Task item 6: secrets must be replaced **before** they reach the file, and
//! ordinary look-alike strings must survive.
//!
//! The corpus is 0-H's: 53 adversarial clean commands and 24 secret-bearing ones.
//! The measured rates are restated here as assertions so a rule change that makes
//! them worse fails the suite. The full 53 are in `redact.rs`'s own unit tests;
//! this file focuses on the *storage* property — what ends up in the database,
//! including the WAL.

mod common;

use common::fresh_dir;
use li_journal::Config;

/// One sample per rule, plus the shapes 0-H required verbatim.
const SECRETS: &[(&str, &str)] = &[
    ("mysql -u root -pS3cr3tPw -h db", "S3cr3tPw"),
    ("export API_KEY=abcdef123456", "abcdef123456"),
    (
        "curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x",
        "eyJhbGciOiJIUzI1NiJ9.abc",
    ),
    ("app --password hunter2hunter2 --verbose", "hunter2hunter2"),
    ("app --api-key=sk-live-9f8e7d6c5b", "sk-live-9f8e7d6c5b"),
    (
        "git clone https://bob:sup3rs3cret@example.com/r.git",
        "sup3rs3cret",
    ),
    ("export DB_PASSWORD=pa55w0rdpa55", "pa55w0rdpa55"),
];

/// 19 commands that a naive redactor breaks. 0-H measured 0 false positives on 53;
/// these are the ones that historically caused them.
const CLEAN: &[&str] = &[
    "ls -l /tmp",
    "git status",
    "cargo build --release",
    "make -j8",
    "python3 train.py --epochs 10",
    "docker run -p 8080:80 nginx",
    "mysql -p3306 -h db",
    "ssh -p 2222 host",
    "export PORT=8080",
    "export TIMEOUT=30",
    // The digits-only defence in action: a secret *name* with a numeric value.
    "export API_KEY=1234567890",
    "mysql --password 3306 -h db",
    "export PATH=$PATH:/usr/local/bin",
    "export API_KEY=$API_KEY",
    "curl -sS https://api.example.com/v1/health",
    "curl -H 'Accept: application/json' https://x",
    "grep -r 'Authorization' src/",
    "rg --password-file .pgpass",
    "sed -i 's/password/pass/g' config.rs",
    "export DATABASE_URL=postgres://localhost/app",
    "find . -name '*.rs' -exec wc -l {} +",
];

#[test]
fn no_secret_reaches_the_database_file() {
    let dir = fresh_dir("t-redact-stored");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    for (i, (cmd, _)) in SECRETS.iter().enumerate() {
        j.event(&common::event(
            &format!("e{i}"),
            "s1",
            i as u64,
            "CommandStarted",
            serde_json::json!({ "command": cmd }),
        ))
        .unwrap();
    }
    let w = j.flush().unwrap();
    assert_eq!(w.failed, 0);
    drop(j);

    // Read the raw bytes of the database **and** its WAL sidecar. Checking only the
    // queryable payload would miss a secret that was written then updated away, and
    // it would miss the WAL entirely.
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !(name.ends_with(".db") || name.ends_with("-wal")) {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        scanned += 1;
        for (cmd, secret) in SECRETS {
            assert!(
                !contains(&bytes, secret.as_bytes()),
                "secret from `{cmd}` found verbatim in {name}"
            );
        }
    }
    assert!(scanned >= 1, "the database file must have been scanned");

    // The stored value is the mask, not a deletion: an audit can see that something
    // was removed here.
    let j = li_journal::Journal::open(&dir.join("journal.db"), Config::default()).unwrap();
    let reader = j.reader().unwrap();
    let payload = reader.payload_of("e0").unwrap().unwrap();
    assert_eq!(payload, r#"{"command":"mysql -u root -p[REDACTED] -h db"}"#);
    let rules: String = reader
        .connection()
        .query_row(
            "SELECT redaction_rules FROM events WHERE event_id='e0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rules, "flag-short-p-glued");
    let hits: i64 = reader
        .connection()
        .query_row(
            "SELECT redaction_hits FROM events WHERE event_id='e0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
}

/// The measured false-positive rate, asserted: 0 of these 19 are masked.
#[test]
fn ordinary_look_alike_strings_are_not_masked() {
    let dir = fresh_dir("t-redact-clean");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    for (i, cmd) in CLEAN.iter().enumerate() {
        j.event(&common::event(
            &format!("c{i}"),
            "s1",
            i as u64,
            "CommandStarted",
            serde_json::json!({ "command": cmd }),
        ))
        .unwrap();
    }
    j.flush().unwrap();
    let reader = j.reader().unwrap();
    for (i, cmd) in CLEAN.iter().enumerate() {
        let payload = reader.payload_of(&format!("c{i}")).unwrap().unwrap();
        let expect = serde_json::to_string(&serde_json::json!({ "command": cmd })).unwrap();
        assert_eq!(
            payload, expect,
            "false positive: `{cmd}` was altered on the way to storage"
        );
        let rules: Option<String> = reader
            .connection()
            .query_row(
                "SELECT redaction_rules FROM events WHERE event_id=?1",
                [format!("c{i}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rules, None, "`{cmd}` fired a rule it must not fire");
    }
    let n: i64 = reader
        .connection()
        .query_row(
            "SELECT count(*) FROM events WHERE redaction_hits IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "no clean command may be recorded as redacted");
}

/// The structural limit, asserted so nobody "fixes" it by accident and calls it
/// solved: a bare positional secret is indistinguishable from a filename.
#[test]
fn a_positional_secret_is_still_not_redacted_it_is_a_known_limit() {
    let r = li_journal::redact::redact("./deploy s3cr3tvalue");
    assert!(
        r.text.contains("s3cr3tvalue"),
        "0-H §7.5: neither generation masks a positional argument. If this changed, \
         update the report instead of the test."
    );
    assert!(r.is_clean());
}

/// Redaction must cost nothing for events that carry no text: it runs on every
/// event, and most events are file/process observations.
#[test]
fn a_payload_without_command_text_sets_no_redaction_columns() {
    let dir = fresh_dir("t-redact-none");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    j.event(&common::event(
        "f1",
        "s1",
        0,
        "FileModified",
        serde_json::json!({ "path": "/tmp/x.rs" }),
    ))
    .unwrap();
    j.flush().unwrap();
    let reader = j.reader().unwrap();
    let (rules, hits, digest): (Option<String>, Option<i64>, Option<String>) = reader
        .connection()
        .query_row(
            "SELECT redaction_rules, redaction_hits, redaction_digest FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rules, None);
    assert_eq!(hits, None);
    assert_eq!(digest, None);
}

/// The digest is linkable — "the same credential was used twice" — and nothing
/// reversible is stored next to it.
#[test]
fn the_same_secret_twice_is_linkable_without_storing_it() {
    let dir = fresh_dir("t-redact-linkable");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    for i in 0..2u64 {
        j.event(&common::event(
            &format!("d{i}"),
            "s1",
            i,
            "CommandStarted",
            serde_json::json!({ "command": "mysql -u root -pS3cr3tPw -h db" }),
        ))
        .unwrap();
    }
    j.flush().unwrap();
    let reader = j.reader().unwrap();
    let mut st = reader
        .connection()
        .prepare("SELECT redaction_digest FROM events ORDER BY ingest_seq")
        .unwrap();
    let digests: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(digests.len(), 2);
    assert_eq!(digests[0], digests[1], "reuse must be detectable");
    assert_eq!(digests[0].len(), 16, "64-bit FNV-1a, hex");
    assert!(!digests[0].contains("S3cr3tPw"));
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
