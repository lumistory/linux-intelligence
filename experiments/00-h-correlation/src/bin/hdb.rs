//! SQLite experiments: migration, ingest throughput, the three query shapes,
//! WAL concurrency, `kill -9` recovery and retention.
//!
//! Every subcommand prints machine-parseable lines so `run.sh` and REPORT.md can
//! quote them without hand-copying.

use std::time::Instant;

use corr0h::db::{DbConfig, EventRow, Store};
use corr0h::util::{percentile, Rng};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const MIGRATIONS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let r = match cmd {
        "migrate" => migrate(&args),
        "bench" => bench(&args),
        "wal" => wal(&args),
        "writer" => writer(&args),
        "reader" => reader(&args),
        "retention" => retention(&args),
        "context" => context(&args),
        "integrity" => integrity(&args),
        "verify" => verify(&args),
        _ => {
            eprintln!("usage: hdb <migrate|bench|wal|writer|reader|retention|context|integrity|verify> ...");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("FAILED: {e}");
        std::process::exit(1);
    }
}

fn arg<'a>(args: &'a [String], i: usize, default: &'a str) -> &'a str {
    args.get(i).map(String::as_str).unwrap_or(default)
}

/// Orchestrates 1 writer + 4 readers as *separate processes* (separate SQLite
/// connections in separate address spaces, which is the real deployment shape),
/// at two `busy_timeout` values. The self-exec trick keeps it in one binary so
/// `run.sh` does not have to juggle background pids and wait statuses.
fn wal(args: &[String]) -> R<()> {
    use std::process::{Command, Stdio};

    let path = arg(args, 2, "/tmp/li-phase0-h/wal.db");
    let seconds: u64 = arg(args, 3, "10").parse()?;
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut store = Store::open(path, DbConfig::default())?;
    store.migrate(MIGRATIONS, None)?;
    store.upsert_session("sess-0", "bash", None, "2025-11-24T00:00:00.000Z", 0, None)?;
    println!("journal_mode={} wal_autocheckpoint={}",
        store.journal_mode()?,
        store.conn.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get::<_, i64>(0))?);
    drop(store);

    let exe = std::env::current_exe()?;
    for busy_ms in [5_000u64, 0u64] {
        println!();
        println!("### busy_timeout_ms={busy_ms}");
        let mut writer = Command::new(&exe)
            .args(["writer", path, &seconds.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Let the writer establish its transaction pattern before readers join.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut readers = Vec::new();
        for i in 0..4 {
            readers.push(
                Command::new(&exe)
                    .args([
                        "reader",
                        path,
                        &(seconds.saturating_sub(1)).to_string(),
                        &format!("r{i}"),
                        &busy_ms.to_string(),
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            );
        }
        let out = writer.wait_with_output()?;
        print!("{}", String::from_utf8_lossy(&out.stdout));
        if !out.stderr.is_empty() {
            print!("writer stderr: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut crashed = 0;
        let mut total_busy = 0u64;
        for r in readers {
            let o = r.wait_with_output()?;
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            print!("{s}");
            for tok in s.split_whitespace() {
                if let Some(v) = tok.strip_prefix("busy=") {
                    total_busy += v.parse::<u64>().unwrap_or(0);
                }
                if let Some(v) = tok.strip_prefix("other_errors=") {
                    if v.parse::<u64>().unwrap_or(0) > 0 {
                        crashed += 1;
                    }
                }
            }
        }
        println!("summary: readers_with_errors={crashed} total_busy_retries_across_readers={total_busy}");
    }

    // Readers never block under WAL, so `busy_timeout` has to be justified by the
    // case that actually contends: two writers. Without this the 5000 ms value
    // would be folklore.
    println!();
    println!("### two concurrent writers (the case busy_timeout actually covers)");
    for busy_ms in [5_000u64, 0u64] {
        let mut w1 = Command::new(&exe)
            .args(["writer", path, &seconds.to_string(), &busy_ms.to_string(), "wa"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut w2 = Command::new(&exe)
            .args(["writer", path, &seconds.to_string(), &busy_ms.to_string(), "wb"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let o1 = w1.wait_with_output()?;
        let o2 = w2.wait_with_output()?;
        let s1 = String::from_utf8_lossy(&o1.stdout).trim().to_string();
        let s2 = String::from_utf8_lossy(&o2.stdout).trim().to_string();
        let e1 = String::from_utf8_lossy(&o1.stderr);
        let e2 = String::from_utf8_lossy(&o2.stderr);
        println!("busy_timeout_ms={busy_ms}");
        println!("  writer A: {s1}");
        println!("  writer B: {s2}");
        if !e1.is_empty() {
            println!("  writer A stderr: {}", e1.lines().next().unwrap_or(""));
        }
        if !e2.is_empty() {
            println!("  writer B stderr: {}", e2.lines().next().unwrap_or(""));
        }
    }
    Ok(())
}

/// Fresh database, migrate to the requested target, then report what is there.
fn migrate(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/mig.db");
    let target: Option<i64> = args.get(3).and_then(|s| s.parse().ok());
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut store = Store::open(path, DbConfig::default())?;
    println!("journal_mode={} synchronous={} busy_timeout_ms={} foreign_keys=on",
        store.journal_mode()?, store.synchronous()?, store.busy_timeout()?);
    let before = store.schema_version()?;
    let applied = store.migrate(MIGRATIONS, target)?;
    let after = store.schema_version()?;
    println!("schema_version: {before} -> {after}, applied={applied:?}");
    println!("integrity_check={}", store.integrity_check()?);
    println!("foreign_key_violations={}", store.foreign_key_violations()?);
    println!("tables:");
    let mut st = store.conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for n in names {
        println!("  {n}");
    }
    println!("indexes:");
    let mut st = store.conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for n in names {
        println!("  {n}");
    }
    Ok(())
}

/// Populate a database with `n` synthetic events in one transaction and time it.
fn populate(store: &Store, n: usize, seed: u64) -> R<()> {
    let mut rng = Rng::new(seed);
    let sessions: Vec<String> = (0..8).map(|i| format!("sess-{i}")).collect();
    for s in &sessions {
        store.upsert_session(s, "bash", Some("5.3.20"), "2025-11-24T00:00:00.000Z", 0, Some("/home/u"))?;
    }
    let t0 = Instant::now();
    store.conn.execute_batch("BEGIN")?;
    {
        let mut st = store.conn.prepare(
            "INSERT INTO events
               (event_id, schema_version, source, session_id, process_row_id, sequence,
                ts_mono_ns, ts_wall, type, type_class, payload, evidence_status,
                dedup_group, ingested_mono_ns)
             VALUES (?1,1,?2,?3,NULL,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(event_id) DO NOTHING",
        )?;
        for i in 0..n {
            let session = &sessions[i % sessions.len()];
            let ts = 1_763_942_400_000_000_000i64 + (i as i64) * 1_000_000;
            let ty = ["CommandStarted", "CommandFinished", "FileModified", "ProcessExit"]
                [rng.below(4) as usize];
            let class = match ty {
                "CommandStarted" => "proc.start",
                "CommandFinished" | "ProcessExit" => "proc.end",
                _ => "fs.modify",
            };
            let payload = format!(
                "{{\"command\":\"cmd{}\",\"path\":\"/tmp/f{}.txt\"}}",
                i % 5000,
                i % 2000
            );
            st.execute(rusqlite::params![
                format!("ev-{i:08}"),
                if i % 3 == 0 { "proc" } else { "shell.bash" },
                session,
                (i % 400) as i64,
                ts,
                format!("2025-11-24T00:{:02}:{:02}.000Z", (i / 60) % 60, i % 60),
                ty,
                class,
                payload,
                if i % 3 == 0 { "observed" } else { "declared" },
                format!("g{}", i % 3000),
                ts,
            ])?;
        }
    }
    store.conn.execute_batch("COMMIT")?;
    let elapsed = t0.elapsed();
    println!("inserted={n} wall_ms={} per_row_us={:.2} rows_per_s={:.0}",
        elapsed.as_millis(),
        elapsed.as_secs_f64() * 1e6 / n as f64,
        n as f64 / elapsed.as_secs_f64());
    Ok(())
}

fn bench(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/bench.db");
    let n: usize = arg(args, 3, "100000").parse()?;
    let iters: usize = arg(args, 4, "200").parse()?;
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut store = Store::open(path, DbConfig::default())?;
    store.migrate(MIGRATIONS, None)?;
    populate(&store, n, 0x5EED)?;
    println!("db_file_bytes={} page_count={}", store.file_size(), store.wal_pages()?.1);

    // Some operations and memberships so the third query shape is real.
    store.conn.execute_batch("BEGIN")?;
    for op in 0..200 {
        store.conn.execute(
            "INSERT INTO operations (operation_id, session_id, label, state, started_mono_ns)
             VALUES (?1, ?2, ?3, 'open', ?4)",
            rusqlite::params![
                format!("op-{op:04}"),
                format!("sess-{}", op % 8),
                format!("operation {op}"),
                1_763_942_400_000_000_000i64 + op as i64
            ],
        )?;
        for k in 0..50 {
            let idx = (op * 37 + k * 13) % n;
            store.conn.execute(
                "INSERT OR IGNORE INTO event_operations
                   (event_id, operation_id, attached_by, level, attached_mono_ns)
                 VALUES (?1,?2,'R6','Weak',?3)",
                rusqlite::params![format!("ev-{idx:08}"), format!("op-{op:04}"), 0i64],
            )?;
        }
    }
    store.conn.execute_batch("COMMIT")?;
    println!("operations={} event_operations={}",
        store.count("operations")?, store.count("event_operations")?);

    // Relations: the third query shape needs real rows on both sides.
    store.conn.execute_batch("BEGIN")?;
    for i in 0..5000usize {
        let from = format!("ev-{:08}", (i * 7) % n);
        let to = format!("ev-{:08}", (i * 7 + 2) % n);
        if from == to {
            continue;
        }
        store.conn.execute(
            "INSERT OR IGNORE INTO event_relations
               (from_event, to_event, kind, level, rule, inferred_at_mono_ns)
             VALUES (?1,?2,?3,?4,'R5',0)",
            rusqlite::params![
                from,
                to,
                if i % 2 == 0 { "modified_during" } else { "observed_within" },
                if i % 3 == 0 { "Probable" } else { "Weak" }
            ],
        )?;
    }
    store.conn.execute_batch("COMMIT")?;
    println!("event_relations={}", store.count("event_relations")?);

    let mut rng = Rng::new(0xBEEF);
    let mut lat: Vec<u64> = Vec::with_capacity(iters);

    // Query 1: by session.
    lat.clear();
    let mut rows_out = 0usize;
    for i in 0..iters {
        let s = format!("sess-{}", i % 8);
        let t = Instant::now();
        let rows = store.events_of_session(&s, 200)?;
        lat.push(t.elapsed().as_nanos() as u64);
        rows_out += rows.len();
    }
    report("q1_events_by_session", &lat, rows_out);

    // Query 2: by time range (1 second slice).
    lat.clear();
    rows_out = 0;
    for _ in 0..iters {
        let lo = 1_763_942_400_000_000_000i64 + rng.below(90_000) as i64 * 1_000_000;
        let t = Instant::now();
        let rows = store.events_in_range(lo, lo + 1_000_000_000)?;
        lat.push(t.elapsed().as_nanos() as u64);
        rows_out += rows.len();
    }
    report("q2_events_by_time_range", &lat, rows_out);

    // Query 3: by operation membership.
    lat.clear();
    rows_out = 0;
    for i in 0..iters {
        let op = format!("op-{:04}", i % 200);
        let t = Instant::now();
        let rows = store.operation_trail(&op)?;
        lat.push(t.elapsed().as_nanos() as u64);
        rows_out += rows.len();
    }
    report("q3_events_by_operation", &lat, rows_out);

    // Not required, but it is the query a user will actually type first.
    lat.clear();
    rows_out = 0;
    for i in 0..iters {
        let p = format!("/tmp/f{}.txt", i % 2000);
        let t = Instant::now();
        let rows = store.file_trail(&p)?;
        lat.push(t.elapsed().as_nanos() as u64);
        rows_out += rows.len();
    }
    report("q4_relations_by_path_NO_index", &lat, rows_out);

    // An index on the extracted expression is the only way to make the JSON-path
    // join cheap without denormalising the payload.
    let t = Instant::now();
    store
        .conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_events_payload_path ON events(json_extract(payload,'$.path'))")?;
    println!("create_expression_index elapsed_ms={}", t.elapsed().as_millis());
    lat.clear();
    rows_out = 0;
    for i in 0..iters {
        let p = format!("/tmp/f{}.txt", i % 2000);
        let t = Instant::now();
        let rows = store.file_trail(&p)?;
        lat.push(t.elapsed().as_nanos() as u64);
        rows_out += rows.len();
    }
    report("q4b_relations_by_path_expr_index", &lat, rows_out);
    println!("db_file_bytes_after_index={}", store.file_size());

    // Writes while the database is this large.
    lat.clear();
    for _ in 0..iters {
        let t = Instant::now();
        store.insert_event(&EventRow {
            event_id: "ev-tail",
            schema_version: 1,
            source: "proc",
            session_id: Some("sess-0"),
            process_row_id: None,
            sequence: 0,
            ts_mono_ns: 1,
            ts_wall: "2025-11-24T00:00:00.000Z",
            type_: "ProcessExit",
            type_class: "proc.end",
            payload: "{}",
            evidence_status: "observed",
            dedup_group: None,
            duplicate_of: None,
            redaction_digest: None,
            ingested_mono_ns: 0,
        })?;
        lat.push(t.elapsed().as_nanos() as u64);
    }
    report("w1_single_row_upsert", &lat, 0);
    Ok(())
}

fn report(name: &str, lat: &[u64], rows: usize) {
    println!(
        "{name} n={} p50_us={:.3} p95_us={:.3} p99_us={:.3} max_us={:.3} rows_returned_total={rows}",
        lat.len(),
        percentile(lat, 50.0) as f64 / 1000.0,
        percentile(lat, 95.0) as f64 / 1000.0,
        percentile(lat, 99.0) as f64 / 1000.0,
        lat.iter().copied().max().unwrap_or(0) as f64 / 1000.0
    );
}

/// Long-running writer used by the WAL concurrency test and the kill -9 test.
fn writer(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/wal.db");
    let seconds: u64 = arg(args, 3, "10").parse()?;
    let busy_ms: u64 = arg(args, 4, "5000").parse()?;
    let store = Store::open(
        path,
        DbConfig {
            busy_timeout_ms: busy_ms,
            ..DbConfig::default()
        },
    )?;
    let tag = arg(args, 5, "w");
    store.upsert_session("sess-0", "bash", None, "2025-11-24T00:00:00.000Z", 0, None)?;
    let deadline = Instant::now() + std::time::Duration::from_secs(seconds);
    let mut i = 0u64;
    let mut errors = 0u64;
    // One transaction per event: the worst case for a concurrent reader, and the
    // only way to get a *partially* applied transaction when the process dies.
    while Instant::now() < deadline {
        let r = store.insert_event(&EventRow {
            event_id: &format!("{tag}-{i:08}"),
            schema_version: 1,
            source: "proc",
            session_id: Some("sess-0"),
            process_row_id: None,
            sequence: i as i64,
            ts_mono_ns: 1_763_942_400_000_000_000 + i as i64,
            ts_wall: "2025-11-24T00:00:00.000Z",
            type_: "ProcessExit",
            type_class: "proc.end",
            payload: "{\"n\":1}",
            evidence_status: "observed",
            dedup_group: None,
            duplicate_of: None,
            redaction_digest: None,
            ingested_mono_ns: i as i64,
        });
        match r {
            Ok(_) => {}
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    eprintln!("writer error #{errors}: {e}");
                }
            }
        }
        i += 1;
    }
    println!("writer {tag} attempted={i} errors={errors}");
    Ok(())
}

fn reader(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/wal.db");
    let seconds: u64 = arg(args, 3, "10").parse()?;
    let id = arg(args, 4, "r0");
    let busy_ms: u64 = arg(args, 5, "5000").parse()?;
    let conn = Store::open_reader(path, busy_ms)?;
    let deadline = Instant::now() + std::time::Duration::from_secs(seconds);
    let mut lat: Vec<u64> = Vec::new();
    let (mut ok, mut busy, mut other) = (0u64, 0u64, 0u64);
    while Instant::now() < deadline {
        let t = Instant::now();
        let r: rusqlite::Result<i64> =
            conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0));
        lat.push(t.elapsed().as_nanos() as u64);
        match r {
            Ok(_) => ok += 1,
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    || e.code == rusqlite::ErrorCode::DatabaseLocked =>
            {
                busy += 1
            }
            Err(_) => other += 1,
        }
    }
    println!("{id} ok={ok} busy={busy} other_errors={other} p50_us={:.1} p99_us={:.1} max_us={:.1}",
        percentile(&lat, 50.0) as f64 / 1000.0,
        percentile(&lat, 99.0) as f64 / 1000.0,
        lat.iter().copied().max().unwrap_or(0) as f64 / 1000.0);
    Ok(())
}

fn integrity(args: &[String]) -> R<()> {
    integrity_impl(arg(args, 2, "/tmp/li-phase0-h/wal.db"))
}

/// Prove, in SQL against the real schema, that membership is 0..n.
fn context(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/ctx.db");
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut store = Store::open(path, DbConfig::default())?;
    store.migrate(MIGRATIONS, None)?;

    store.upsert_session("sess-1", "bash", None, "2025-11-24T00:00:00.000Z", 0, None)?;
    store.conn.execute_batch(
        "INSERT INTO operations (operation_id, session_id, label, state, started_mono_ns) VALUES
           ('op-edit','sess-1','edit parser','open',0),
           ('op-recover','sess-1','recover lost work','open',600000000000),
           ('op-untouched','sess-1','never sees an event','open',0);",
    )?;
    for (id, seq, ty) in [
        ("ev-1", 1, "CommandStarted"),
        ("ev-2", 2, "FileModified"),
        ("ev-3", 3, "ProcessExit"),
    ] {
        store.insert_event(&EventRow {
            event_id: id,
            schema_version: 1,
            source: "fs",
            session_id: Some("sess-1"),
            process_row_id: None,
            sequence: seq,
            ts_mono_ns: seq * 1_000_000,
            ts_wall: "2025-11-24T00:00:00.000Z",
            type_: ty,
            type_class: "fs.modify",
            payload: "{\"path\":\"/tmp/li/x\"}",
            evidence_status: "observed",
            dedup_group: None,
            duplicate_of: None,
            redaction_digest: None,
            ingested_mono_ns: 0,
        })?;
    }

    // ev-2 belongs to two operations at two different levels; ev-3 belongs to
    // none. No sentinel row, no placeholder operation.
    store.conn.execute_batch(
        "INSERT INTO event_operations (event_id, operation_id, attached_by, level, attached_mono_ns) VALUES
           ('ev-1','op-edit','R6','Weak',0),
           ('ev-2','op-edit','R5','Probable',0),
           ('ev-2','op-recover','R4','Strong',0);",
    )?;

    println!("### 6.3 SQLite: 0..n membership queries against the real schema");
    println!();
    println!("| query | rows |");
    println!("|---|---|");
    let ops_of_ev2: i64 = store.conn.query_row(
        "SELECT count(*) FROM event_operations WHERE event_id='ev-2' AND detached_mono_ns IS NULL",
        [],
        |r| r.get(0),
    )?;
    println!("| operations of ev-2 | {ops_of_ev2} |");
    let ev_of_op_recover: i64 = store.conn.query_row(
        "SELECT count(*) FROM event_operations WHERE operation_id='op-recover' AND detached_mono_ns IS NULL",
        [],
        |r| r.get(0),
    )?;
    println!("| events of op-recover | {ev_of_op_recover} |");
    let orphans: i64 = store.conn.query_row(
        "SELECT count(*) FROM events e
         WHERE NOT EXISTS (SELECT 1 FROM event_operations eo
                           WHERE eo.event_id = e.event_id AND eo.detached_mono_ns IS NULL)",
        [],
        |r| r.get(0),
    )?;
    println!("| events with no operation | {orphans} |");
    let empty_ops: i64 = store.conn.query_row(
        "SELECT count(*) FROM operations o
         WHERE NOT EXISTS (SELECT 1 FROM event_operations eo WHERE eo.operation_id = o.operation_id)",
        [],
        |r| r.get(0),
    )?;
    println!("| operations with no event | {empty_ops} |");

    println!();
    println!("ev-2 per-operation credibility (the reason level lives on the edge, not the event):");
    println!("| operation | level | attached_by |");
    println!("|---|---|---|");
    {
        let mut st = store.conn.prepare(
            "SELECT operation_id, level, attached_by FROM event_operations
             WHERE event_id='ev-2' ORDER BY operation_id",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (op, level, by) = row?;
            println!("| {op} | {level} | {by} |");
        }
    }

    println!();
    println!("--- retraction: ev-2 leaves op-recover, history kept ---");
    store.conn.execute(
        "UPDATE event_operations SET detached_mono_ns=700000000000
         WHERE event_id='ev-2' AND operation_id='op-recover'",
        [],
    )?;
    let live: i64 = store.conn.query_row(
        "SELECT count(*) FROM event_operations WHERE event_id='ev-2' AND detached_mono_ns IS NULL",
        [],
        |r| r.get(0),
    )?;
    let total: i64 = store.conn.query_row(
        "SELECT count(*) FROM event_operations WHERE event_id='ev-2'",
        [],
        |r| r.get(0),
    )?;
    println!("live memberships of ev-2 = {live}, membership rows retained = {total}");
    println!();
    println!("--- the single-column alternative, shown failing ---");
    println!("With `events.operation_id`, ev-2 would need one value. The two rows above");
    println!("differ in level (Probable vs Strong) and in attached_by, so no single value");
    println!("can represent ev-2's state without discarding a measured fact.");
    println!();
    println!("integrity_check={}", store.integrity_check()?);
    println!("foreign_key_violations={}", store.foreign_key_violations()?);
    Ok(())
}

fn integrity_impl(path: &str) -> R<()> {
    let store = Store::open(path, DbConfig::default())?;
    let t = Instant::now();
    let ic = store.integrity_check()?;
    println!("integrity_check={ic} elapsed_ms={}", t.elapsed().as_millis());
    println!("foreign_key_violations={}", store.foreign_key_violations()?);
    println!("events={} event_relations={} schema_version={}",
        store.count("events")?, store.count("event_relations")?, store.schema_version()?);
    // A committed row must still be readable, and the largest visible sequence
    // must not contain a hole caused by the crash: SQLite rolls back whole
    // transactions, so the max committed sequence is the boundary.
    let (max_seq, cnt): (i64, i64) = store.conn.query_row(
        "SELECT coalesce(max(sequence), -1), count(*) FROM events WHERE event_id LIKE 'w-%'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    println!("writer_rows_visible={cnt} max_sequence={max_seq}");
    if cnt != max_seq + 1 {
        println!("NOTE: visible row count != max_sequence+1 -> a partially committed transaction is visible");
    } else {
        println!("OK: visible rows form a contiguous committed prefix (no torn transaction)");
    }
    Ok(())
}

fn retention(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/ret.db");
    let n: usize = arg(args, 3, "100000").parse()?;
    let mode = arg(args, 4, "time");
    let add_index = arg(args, 5, "no") == "yes";
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut store = Store::open(path, DbConfig::default())?;
    store.migrate(MIGRATIONS, None)?;
    if add_index {
        // The index that makes DELETE linear instead of quadratic: `duplicate_of`
        // is a self-referencing FK with ON DELETE SET NULL, so every deleted row
        // costs a scan of `events` unless the child column is indexed.
        let t = Instant::now();
        store.conn.execute_batch(
            "CREATE INDEX idx_events_duplicate_of ON events(duplicate_of);
             CREATE INDEX idx_event_relations_from_fk ON event_relations(from_event);
             CREATE INDEX idx_event_relations_to_fk ON event_relations(to_event);
             CREATE INDEX idx_event_operations_event_fk ON event_operations(event_id);",
        )?;
        println!("fk_child_indexes_created elapsed_ms={}", t.elapsed().as_millis());
    }
    populate(&store, n, 0x5EED)?;
    let before = store.file_size();
    println!("before rows={} file_bytes={before}", store.count("events")?);

    let t = Instant::now();
    let removed = if mode == "time" {
        // Delete the oldest 10 %: a time-based window with a 90 % retention.
        let cutoff = 1_763_942_400_000_000_000i64 + (n as i64 / 10) * 1_000_000;
        let r = store.trim_by_time(cutoff, 5000)?;
        println!("trim_by_time batch=5000 removed={r}");
        r
    } else {
        let r = store.trim_by_size((n as i64) * 9 / 10, 5000)?;
        println!("trim_by_size keep={} batch=5000 removed={r}", (n as i64) * 9 / 10);
        r
    };
    let trim_ms = t.elapsed().as_millis();
    println!("trim removed={removed} elapsed_ms={trim_ms} rows_now={} file_bytes={}",
        store.count("events")?, store.file_size());

    let t = Instant::now();
    store.vacuum()?;
    let vacuum_ms = t.elapsed().as_millis();
    println!("vacuum elapsed_ms={vacuum_ms} file_bytes={} rows={}",
        store.file_size(), store.count("events")?);

    let t = Instant::now();
    store.incremental_vacuum(1000)?;
    println!("incremental_vacuum auto_vacuum_mode={} elapsed_ms={}",
        store.conn.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))?,
        t.elapsed().as_millis());
    println!("integrity_check={}", store.integrity_check()?);
    Ok(())
}

/// Prove the v1 database can be upgraded without data loss.
fn verify(args: &[String]) -> R<()> {
    let path = arg(args, 2, "/tmp/li-phase0-h/mig.db");
    let store = Store::open(path, DbConfig::default())?;
    println!("schema_version={}", store.schema_version()?);
    let cols = columns(&store, "events")?;
    println!("events columns: {}", cols.join(", "));
    let scols = columns(&store, "sessions")?;
    println!("sessions columns: {}", scols.join(", "));
    println!("events={} sessions={}", store.count("events")?, store.count("sessions")?);
    for (v, desc, ck) in store.applied_migrations()? {
        println!("  v{v} {desc} checksum={ck}");
    }
    println!("integrity_check={}", store.integrity_check()?);
    Ok(())
}

/// Column names of a table, via `PRAGMA table_info`. A helper rather than an
/// inline block because the prepared statement must outlive the iterator.
fn columns(store: &Store, table: &str) -> R<Vec<String>> {
    let mut st = store
        .conn
        .prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = st.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
