//! `ipcclient` — load/behaviour harness for experiment 00-C.
//!
//! Subcommands map 1:1 onto the numbered tests in `README.md` / `run.sh`.
//! Clients are synchronous threads on purpose: one client process per shell is
//! the real topology, and blocking behaviour is exactly what tests 4, 5 and 9
//! need to observe.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use li_ipc::{
    micros, millis, mono_ns, read_frame, write_json_line, Event, ReadOutcome, MAX_FRAME_BYTES,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn sock_path() -> PathBuf {
    match std::env::var("LI_IPC_SOCK") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(li_ipc::default_socket_path()),
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Sorted percentiles for a mutable sample vector (sorted in place).
fn stats(samples: &mut [u64]) -> serde_json::Value {
    samples.sort_unstable();
    serde_json::json!({
        "n": samples.len(),
        "p50_us": micros(pct(samples, 0.50)),
        "p90_us": micros(pct(samples, 0.90)),
        "p99_us": micros(pct(samples, 0.99)),
        "max_us": micros(samples.last().copied().unwrap_or(0)),
    })
}

fn ranges_of(missing: &[u64]) -> Vec<[u64; 2]> {
    let mut out: Vec<[u64; 2]> = Vec::new();
    if missing.is_empty() {
        return out;
    }
    let mut start = missing[0];
    let mut prev = missing[0];
    for &v in &missing[1..] {
        if v != prev + 1 {
            out.push([start, prev]);
            start = v;
        }
        prev = v;
    }
    out.push([start, prev]);
    out
}

#[derive(Default)]
struct Args {
    kv: HashMap<String, String>,
    flags: BTreeSet<String>,
}

impl Args {
    /// Accepts `--key value` and bare `--flag`. A token starting with `--` is a
    /// key; the following token is its value only if it does not itself start
    /// with `--`. Peeking by index (not a consuming iterator) keeps
    /// `--reconnect --max-backoff-ms 500` unambiguous.
    fn parse(raw: &[String]) -> Result<Self, String> {
        let mut a = Args::default();
        let mut i = 0;
        while i < raw.len() {
            let tok = &raw[i];
            let key = match tok.strip_prefix("--") {
                Some(k) if !k.is_empty() => k.to_string(),
                _ => return Err(format!("unexpected positional argument: {tok}")),
            };
            match raw.get(i + 1) {
                Some(next) if !next.starts_with("--") => {
                    a.kv.insert(key, next.clone());
                    i += 2;
                }
                _ => {
                    a.flags.insert(key);
                    i += 1;
                }
            }
        }
        Ok(a)
    }
    fn u64(&self, key: &str, d: u64) -> u64 {
        self.kv.get(key).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
    fn s(&self, key: &str, d: &str) -> String {
        self.kv.get(key).cloned().unwrap_or_else(|| d.to_string())
    }
    fn opt(&self, key: &str) -> Option<String> {
        self.kv.get(key).cloned()
    }
    fn has(&self, key: &str) -> bool {
        self.flags.contains(key) || self.kv.contains_key(key)
    }
}

fn connect(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Send `Ping` and wait for `Pong`. Because the server routes Pings through the
/// ingest queue, a Pong proves everything sent earlier has been ingested.
fn barrier(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> std::io::Result<()> {
    let nonce = mono_ns();
    let msg = serde_json::json!({"schema_version": 1, "type": "Ping", "nonce": nonce});
    write_json_line(stream, &msg)?;
    let mut errors = 0usize;
    loop {
        match read_frame(reader, MAX_FRAME_BYTES)? {
            ReadOutcome::Frame(f) => {
                let v: serde_json::Value = serde_json::from_slice(&f).unwrap_or_default();
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("Pong") => return Ok(()),
                    Some("Error") => errors += 1,
                    _ => {}
                }
                let _ = errors;
            }
            ReadOutcome::TooLarge { .. } => continue,
            ReadOutcome::Eof => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed connection during barrier",
                ))
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = argv.first().cloned().unwrap_or_default();
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    let res = match mode.as_str() {
        "concurrency" => concurrency(&rest),
        "fill" => fill(&rest),
        "probe" => probe(&rest),
        "oversize" => oversize(&rest),
        "sender" => sender(&rest),
        "audit" => audit(&rest),
        "dup" => dup(&rest),
        "hold" => hold(&rest),
        "abrupt" => abrupt(&rest),
        "unlink" => unlink_path(&rest),
        "latency" => latency(&rest),
        other => Err(format!("unknown mode {other:?}")),
    };
    match res {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ipcclient {mode}: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Test 1 — 50 clients x 100 events: completeness, throughput, p50/p99
// ---------------------------------------------------------------------------

struct ClientOutcome {
    sent: usize,
    acked: usize,
    t_start: u64,
    t_flushed: u64,
    t_done: u64,
    latency: Vec<u64>,
}

fn concurrency(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let clients = a.u64("clients", 50) as usize;
    let events = a.u64("events", 100) as usize;
    let tag = a.s("tag", "run");
    let path = sock_path();

    let gate = Arc::new(Barrier::new(clients + 1));
    let connect_errors = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(clients);

    for c in 0..clients {
        let path = path.clone();
        let gate = gate.clone();
        let connect_errors = connect_errors.clone();
        let tag = tag.clone();
        handles.push(std::thread::spawn(
            move || -> Result<ClientOutcome, String> {
                let mut stream = match connect(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        connect_errors.fetch_add(1, Ordering::Relaxed);
                        return Err(format!("client {c} connect: {e}"));
                    }
                };
                let clone = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
                let mut reader = BufReader::with_capacity(64 * 1024, clone);
                gate.wait();

                let session = format!("{tag}-c{c}");
                let t_start = mono_ns();
                let mut sent: Vec<(u64, String)> = Vec::with_capacity(events);
                let mut wire = Vec::with_capacity(events * 128);
                for seq in 0..events as u64 {
                    let ev = Event::new(
                        &session,
                        seq,
                        "CommandStarted",
                        serde_json::json!({"cmd": "echo hi", "cwd": "<HOME>", "seq": seq}),
                    );
                    let mut line = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
                    line.push(b'\n');
                    sent.push((mono_ns(), ev.event_id.clone()));
                    wire.extend_from_slice(&line);
                }
                // One write syscall for the batch: a hook writes one small event at
                // a time, so this keeps syscall noise out of the latency samples
                // while still pushing the same byte volume.
                stream
                    .write_all(&wire)
                    .map_err(|e| format!("client {c} write: {e}"))?;
                let t_flushed = mono_ns();

                let nonce = mono_ns();
                let ping = serde_json::json!({"schema_version": 1, "type": "Ping", "nonce": nonce});
                write_json_line(&mut stream, &ping).map_err(|e| format!("ping: {e}"))?;

                let mut acked: HashMap<String, u64> = HashMap::with_capacity(events);
                let mut got_pong = false;
                while !got_pong {
                    let frame = read_frame(&mut reader, MAX_FRAME_BYTES)
                        .map_err(|e| format!("client {c} read: {e}"))?;
                    let f = match frame {
                        ReadOutcome::Frame(f) => f,
                        ReadOutcome::TooLarge { .. } => continue,
                        ReadOutcome::Eof => return Err(format!("client {c}: server closed early")),
                    };
                    let v: serde_json::Value = serde_json::from_slice(&f).unwrap_or_default();
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("Ack") => {
                            if let Some(eid) = v.get("event_id").and_then(|e| e.as_str()) {
                                acked.insert(eid.to_string(), mono_ns());
                            }
                        }
                        Some("Pong") => got_pong = true,
                        Some("Error") => {
                            return Err(format!("client {c}: {}", String::from_utf8_lossy(&f)))
                        }
                        _ => {}
                    }
                }
                let t_done = mono_ns();

                let mut latency = Vec::with_capacity(events);
                for (ts, eid) in &sent {
                    match acked.get(eid) {
                        Some(ack) => latency.push(ack.saturating_sub(*ts)),
                        None => return Err(format!("client {c}: no ack for {eid}")),
                    }
                }
                Ok(ClientOutcome {
                    sent: events,
                    acked: acked.len(),
                    t_start,
                    t_flushed,
                    t_done,
                    latency,
                })
            },
        ));
    }

    gate.wait();
    let mut results = Vec::with_capacity(clients);
    let mut errors = Vec::new();
    for h in handles {
        match h.join() {
            Ok(Ok(o)) => results.push(o),
            Ok(Err(e)) => errors.push(e),
            Err(_) => errors.push("client thread panicked".to_string()),
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("CLIENT-ERROR {e}");
        }
        return Err(format!("{} of {clients} clients failed", errors.len()));
    }

    let t0 = results.iter().map(|r| r.t_start).min().unwrap_or(0);
    let t1 = results.iter().map(|r| r.t_done).max().unwrap_or(0);
    let wall_ns = t1.saturating_sub(t0);
    let total: usize = results.iter().map(|r| r.sent).sum();
    let acked: usize = results.iter().map(|r| r.acked).sum();

    let mut all: Vec<u64> = Vec::with_capacity(total);
    for r in &results {
        all.extend_from_slice(&r.latency);
    }
    let mut batch: Vec<u64> = results
        .iter()
        .map(|r| r.t_flushed.saturating_sub(r.t_start))
        .collect();

    let eps = total as f64 / (wall_ns as f64 / 1e9);
    println!(
        "{}",
        serde_json::json!({
            "mode": "concurrency",
            "clients": clients,
            "events_per_client": events,
            "events_total": total,
            "events_acked": acked,
            "all_acked": acked == total,
            "wall_ms": millis(wall_ns),
            "throughput_events_per_sec": eps,
            "latency_event_to_ack": stats(&mut all),
            "client_batch_write": stats(&mut batch),
            "connect_errors": connect_errors.load(Ordering::Relaxed),
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 1 (latency) — per-event round trip on an otherwise idle daemon
// ---------------------------------------------------------------------------

/// One event, one synchronous ack, repeated. This is the number that maps onto
/// what a shell hook would actually pay: the hook writes its event and (if it
/// waits at all) waits for the ack. No other client is running, so the sample
/// is not inflated by cross-client queueing.
fn latency(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let path = sock_path();
    let n = a.u64("n", 1000) as usize;
    let session = a.s("session", "lat");
    let mut stream = connect(&path).map_err(|e| format!("connect: {e}"))?;
    let clone = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::with_capacity(16 * 1024, clone);

    let mut samples: Vec<u64> = Vec::with_capacity(n);
    for i in 0..n as u64 {
        let ev = Event::new(&session, i, "CommandStarted", serde_json::json!({"i": i}));
        let t0 = mono_ns();
        let mut line = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
        line.push(b'\n');
        stream.write_all(&line).map_err(|e| format!("write: {e}"))?;
        // Wait for the ack of this specific event.
        loop {
            match read_frame(&mut reader, MAX_FRAME_BYTES).map_err(|e| format!("read: {e}"))? {
                ReadOutcome::Frame(f) => {
                    let v: serde_json::Value = serde_json::from_slice(&f).unwrap_or_default();
                    if v.get("type").and_then(|t| t.as_str()) == Some("Ack")
                        && v.get("event_id").and_then(|e| e.as_str()) == Some(ev.event_id.as_str())
                    {
                        samples.push(mono_ns().saturating_sub(t0));
                        break;
                    }
                }
                ReadOutcome::TooLarge { .. } => {}
                ReadOutcome::Eof => return Err("server closed".into()),
            }
        }
    }
    // A second pass without waiting: how fast can the client enqueue?
    let bulk_start = mono_ns();
    let mut batch = Vec::with_capacity(n * 128);
    for i in 0..n as u64 {
        let ev = Event::new(
            &format!("{session}-burst"),
            i,
            "CommandStarted",
            serde_json::json!({"i": i}),
        );
        let mut line = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
        line.push(b'\n');
        batch.extend_from_slice(&line);
    }
    stream
        .write_all(&batch)
        .map_err(|e| format!("burst write: {e}"))?;
    let burst_us = micros(mono_ns().saturating_sub(bulk_start));

    println!(
        "{}",
        serde_json::json!({
            "mode": "latency",
            "session": session,
            "round_trip": stats(&mut samples),
            "burst_write": {
                "events": n,
                "total_us": burst_us,
                "per_event_us": burst_us / n as f64,
            },
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// low-level socket helpers for tests 4 and 9
// ---------------------------------------------------------------------------

fn poll_out(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd; the return value is checked before use.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0 && (pfd.revents & libc::POLLOUT) != 0
}

/// SO_SNDTIMEO takes a `timeval`; the conversion is the whole point of the
/// helper, since `set_write_timeout` only offers millisecond granularity.
fn set_sndtimeo_us(fd: i32, us: i64) -> std::io::Result<()> {
    let tv = libc::timeval {
        tv_sec: (us / 1_000_000) as libc::time_t,
        tv_usec: (us % 1_000_000) as libc::suseconds_t,
    };
    // SAFETY: valid fd, valid timeval of the declared size; rc is checked.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn sndbuf_bytes(fd: i32) -> i64 {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid fd and a correctly sized out-buffer.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &mut v as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return -1;
    }
    v as i64
}

// ---------------------------------------------------------------------------
// Test 4 — writes until the client blocks (server SIGSTOPped)
// ---------------------------------------------------------------------------

fn fill(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let pad = a.u64("pad", 120) as usize;
    let wait_ms = a.u64("wait-ms", 2000);
    let session = a.s("session", "fill");
    let sndtimeo_us = a.u64("sndtimeo-us", 2000);
    let path = sock_path();
    let padding = "x".repeat(pad);

    // -- phase A: non-blocking socket, exact count of accepted frames --------
    let stream = connect(&path).map_err(|e| format!("connect: {e}"))?;
    let fd = stream.as_raw_fd();
    let sndbuf = sndbuf_bytes(fd);
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;
    let mut w = &stream;

    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut frame_len = 0usize;
    let t0 = mono_ns();
    loop {
        let ev = Event::new(
            &session,
            frames,
            "CommandStarted",
            serde_json::json!({ "pad": padding, "seq": frames }),
        );
        let mut buf = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
        buf.push(b'\n');
        if frames == 0 {
            frame_len = buf.len();
        }
        match w.write(&buf) {
            Ok(n) if n == buf.len() => {
                frames += 1;
                bytes += n as u64;
            }
            Ok(n) => {
                bytes += n as u64;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(format!("write after {frames} frames: {e}")),
        }
        if frames > 2_000_000 {
            return Err("fill never blocked after 2e6 frames — server is reading".into());
        }
    }
    let t_blocked = mono_ns();

    let wait_start = Instant::now();
    let mut drained_after_ms: Option<u128> = None;
    while wait_start.elapsed() < Duration::from_millis(wait_ms) {
        if poll_out(fd, 100) {
            drained_after_ms = Some(wait_start.elapsed().as_millis());
            break;
        }
    }

    // -- phase B: blocking socket, wall time of the blocking write ----------
    // SO_SNDTIMEO is what a shell hook would actually set: without it the write
    // below never returns while the daemon is stopped.
    let stream2 = connect(&path).map_err(|e| format!("connect(blocking): {e}"))?;
    set_sndtimeo_us(stream2.as_raw_fd(), sndtimeo_us as i64)
        .map_err(|e| format!("SO_SNDTIMEO: {e}"))?;
    let mut w2 = &stream2;
    let mut blocking_frames = 0u64;
    let mut blocking_bytes = 0u64;
    let mut first_block_us: Option<f64> = None;
    let mut block_err: Option<String> = None;
    for i in 0..200_000u64 {
        let ev = Event::new(
            &format!("{session}-blk"),
            i,
            "CommandStarted",
            serde_json::json!({ "pad": padding, "seq": i }),
        );
        let mut buf = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
        buf.push(b'\n');
        let w0 = Instant::now();
        let r = w2.write_all(&buf);
        let el = w0.elapsed();
        match r {
            Ok(()) => {
                blocking_frames += 1;
                blocking_bytes += buf.len() as u64;
                if el >= Duration::from_millis(20) {
                    first_block_us = Some(el.as_secs_f64() * 1e6);
                    break;
                }
            }
            Err(e) => {
                first_block_us = Some(el.as_secs_f64() * 1e6);
                block_err = Some(e.to_string());
                break;
            }
        }
    }

    println!(
        "{}",
        serde_json::json!({
            "mode": "fill",
            "session": session,
            "frame_bytes": frame_len,
            "so_sndtimeo_us": sndtimeo_us,
            "so_sndbuf_bytes": sndbuf,
            "nonblocking": {
                "frames_accepted": frames,
                "bytes_accepted": bytes,
                "blocked_at_frame_number": frames + 1,
                "fill_duration_us": micros(t_blocked.saturating_sub(t0)),
                "drained_within_wait": drained_after_ms.is_some(),
                "drained_after_ms": drained_after_ms,
            },
            "blocking": {
                "frames_written_before_block": blocking_frames,
                "bytes_written_before_block": blocking_bytes,
                "blocking_write_wall_us": first_block_us,
                "write_error": block_err,
            },
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 5 — one report attempt with no daemon socket
// ---------------------------------------------------------------------------

fn probe(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let n = a.u64("n", 100) as usize;
    let path = sock_path();
    let mut samples: Vec<u64> = Vec::with_capacity(n);
    let mut outcomes: HashMap<String, u64> = HashMap::new();
    for _ in 0..n {
        let t0 = mono_ns();
        let r = connect(&path);
        samples.push(mono_ns().saturating_sub(t0));
        let key = match r {
            Ok(_) => "connected".to_string(),
            Err(e) => format!("{:?}/{}", e.kind(), e.raw_os_error().unwrap_or(0)),
        };
        *outcomes.entry(key).or_insert(0) += 1;
    }
    println!(
        "{}",
        serde_json::json!({
            "mode": "probe",
            "socket_exists": path.exists(),
            "socket": path.display().to_string(),
            "attempts": n,
            "outcomes": outcomes,
            "attempt_cost": stats(&mut samples),
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3 — 10 MiB frame
// ---------------------------------------------------------------------------

fn oversize(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let mib = a.u64("mib", 10);
    let path = sock_path();
    let mut stream = connect(&path).map_err(|e| format!("connect: {e}"))?;
    let clone = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, clone);

    let pad = "A".repeat(mib as usize * 1024 * 1024);
    let ev = Event::new(
        "oversize",
        0,
        "CommandStarted",
        serde_json::json!({"pad": pad}),
    );
    let mut line = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
    let frame_bytes = line.len() + 1;
    line.push(b'\n');
    let t0 = mono_ns();
    let write_res = stream.write_all(&line);
    let write_us = micros(mono_ns().saturating_sub(t0));

    let mut error_frames: Vec<String> = Vec::new();
    let mut pong = false;
    let nonce = mono_ns();
    let ping = serde_json::json!({"schema_version": 1, "type": "Ping", "nonce": nonce});
    let barrier_err = match write_json_line(&mut stream, &ping) {
        Ok(_) => loop {
            match read_frame(&mut reader, MAX_FRAME_BYTES) {
                Ok(ReadOutcome::Frame(f)) => {
                    let v: serde_json::Value = serde_json::from_slice(&f).unwrap_or_default();
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("Pong") => {
                            pong = true;
                            break None;
                        }
                        Some("Error") => error_frames.push(String::from_utf8_lossy(&f).to_string()),
                        _ => {}
                    }
                }
                Ok(ReadOutcome::TooLarge { bytes }) => {
                    error_frames.push(format!("client-side oversize skip: {bytes} bytes"))
                }
                Ok(ReadOutcome::Eof) => break Some("server closed connection".to_string()),
                Err(e) => break Some(e.to_string()),
            }
        },
        Err(e) => Some(e.to_string()),
    };

    println!(
        "{}",
        serde_json::json!({
            "mode": "oversize",
            "declared_mib": mib,
            "frame_bytes": frame_bytes,
            "write_ok": write_res.is_ok(),
            "write_error": write_res.err().map(|e| e.to_string()),
            "write_us": write_us,
            "error_frames": error_frames,
            "connection_survived": pong,
            "barrier_error": barrier_err,
        })
    );
    if !pong {
        return Err("connection did not survive the oversized frame".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests 1/6/8/9 — sender with bounded queue, drop counting, reconnect backoff
// ---------------------------------------------------------------------------

struct Shared {
    acked: Mutex<BTreeSet<u64>>,
    /// Last `Pong` seen by the reader thread; carries the daemon's per-session
    /// watermark, which is the authoritative delivery proof (per-event `Ack`
    /// frames are best-effort and may be dropped under load).
    last_pong: Mutex<Option<serde_json::Value>>,
    resolver_alive: AtomicBool,
}

fn sender(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let session = a.s("session", "sender");
    let count = a.u64("count", 1000);
    let rate_us = a.u64("rate-us", 0);
    let queue_cap = a.u64("queue-cap", 1024).max(1) as usize;
    let policy = a.s("policy", "drop");
    let reconnect = a.has("reconnect");
    let report = a.opt("report");
    let max_backoff_ms = a.u64("max-backoff-ms", 500);
    let path = sock_path();

    let shared = Arc::new(Shared {
        acked: Mutex::new(BTreeSet::new()),
        last_pong: Mutex::new(None),
        resolver_alive: AtomicBool::new(false),
    });

    let mut conn: Option<UnixStream> = None;
    let mut seq: u64 = 0;
    let mut produced: u64 = 0;
    let mut dropped_full: u64 = 0;
    let mut written: u64 = 0;
    let mut send_errors: u64 = 0;
    // Bounded: a long outage can produce thousands of failed attempts, and an
    // unbounded log would be useless (and huge). Keep the first few, the last
    // few, and an exact counter.
    let mut reconnects: Vec<serde_json::Value> = Vec::new();
    let mut failed_attempt_total: u64 = 0;
    let mut reconnect_total: u64 = 0;
    let reconnect_note_cap: u64 = 20;
    let mut queue: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
    let mut max_queue_depth = 0usize;
    let mut backoff_ms = 10u64;
    let mut down_since: Option<u64> = None;
    let mut failed_attempts = 0u64;
    let mut connect_attempts = 0u64;
    let mut connect_failures = 0u64;
    let t_all0 = mono_ns();
    let deadline = Instant::now() + Duration::from_secs(a.u64("timeout-s", 120));

    while !(produced >= count && queue.is_empty()) && Instant::now() < deadline {
        // ---- 1. connection -------------------------------------------------
        if conn.is_none() {
            connect_attempts += 1;
            match connect(&path) {
                Ok(s) => {
                    s.set_nonblocking(true)
                        .map_err(|e| format!("set_nonblocking: {e}"))?;
                    let rd = s.try_clone().map_err(|e| format!("clone: {e}"))?;
                    rd.set_nonblocking(false)
                        .map_err(|e| format!("reader mode: {e}"))?;
                    let st = shared.clone();
                    st.resolver_alive.store(true, Ordering::SeqCst);
                    std::thread::spawn(move || {
                        let mut r = BufReader::with_capacity(64 * 1024, rd);
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            match r.read_until(b'\n', &mut line) {
                                Ok(0) => break,
                                Ok(_) => {
                                    let v: serde_json::Value =
                                        serde_json::from_slice(&line).unwrap_or_default();
                                    match v.get("type").and_then(|t| t.as_str()) {
                                        Some("Ack") => {
                                            if let Some(sq) =
                                                v.get("sequence").and_then(|x| x.as_u64())
                                            {
                                                st.acked.lock().unwrap().insert(sq);
                                            }
                                        }
                                        Some("Pong") => {
                                            *st.last_pong.lock().unwrap() = Some(v);
                                        }
                                        _ => {}
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        st.resolver_alive.store(false, Ordering::SeqCst);
                    });
                    // Only a connect that followed a down period is a reconnect;
                    // the very first connect is not.
                    if let Some(t_down) = down_since.take() {
                        reconnect_total += 1;
                        if reconnects.len() < 200 {
                            reconnects.push(serde_json::json!({
                                "event": "reconnected",
                                "down_for_us": micros(mono_ns().saturating_sub(t_down)),
                                "failed_attempts": failed_attempts,
                                "last_backoff_ms": backoff_ms,
                                "queued_at_reconnect": queue.len(),
                                "reconnect_number": reconnect_total,
                            }));
                        }
                    }
                    failed_attempts = 0;
                    backoff_ms = 10;
                    conn = Some(s);
                }
                Err(e) => {
                    connect_failures += 1;
                    failed_attempts += 1;
                    if !reconnect {
                        return Err(format!("connect: {e}"));
                    }
                    let now = mono_ns();
                    let down = *down_since.get_or_insert(now);
                    failed_attempt_total += 1;
                    let worth_logging =
                        failed_attempt_total <= reconnect_note_cap || backoff_ms == max_backoff_ms;
                    if worth_logging && reconnects.len() < 200 {
                        reconnects.push(serde_json::json!({
                            "event": "connect_failed",
                            "err": format!("{:?}", e.kind()),
                            "down_for_us": micros(now.saturating_sub(down)),
                            "sleep_ms": backoff_ms,
                            "attempt": failed_attempt_total,
                        }));
                    }
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
                }
            }
        }

        // ---- 2. flush what is queued (never blocks) ------------------------
        let mut blocked = false;
        if let Some(c) = conn.as_ref() {
            while let Some(front) = queue.front() {
                match (&*c).write(front) {
                    Ok(n) if n == front.len() => {
                        queue.pop_front();
                        written += 1;
                    }
                    Ok(_) => {
                        send_errors += 1;
                        conn = None;
                        blocked = true;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        blocked = true;
                        break;
                    }
                    Err(e) => {
                        send_errors += 1;
                        eprintln!("sender: write error: {e}");
                        conn = None;
                        blocked = true;
                        break;
                    }
                }
            }
        }
        max_queue_depth = max_queue_depth.max(queue.len());

        // ---- 3. produce, up to the bound --------------------------------
        while produced < count && queue.len() < queue_cap {
            if rate_us > 0 && produced > 0 {
                std::thread::sleep(Duration::from_micros(rate_us));
            }
            let ev = Event::new(
                &session,
                seq,
                "CommandStarted",
                serde_json::json!({"seq": seq}),
            );
            let mut buf = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
            buf.push(b'\n');
            queue.push_back(buf);
            seq += 1;
            produced += 1;
        }
        max_queue_depth = max_queue_depth.max(queue.len());

        // ---- 4. queue full: apply the policy ----------------------------
        if produced < count && queue.len() >= queue_cap {
            match policy.as_str() {
                "block" => {}
                _ => {
                    // A shell hook must never wait for the daemon. Drop, count,
                    // and keep the sequence advancing so the loss is a visible
                    // gap (protocol requirement, see docs/ipc-protocol-v0.md).
                    dropped_full += 1;
                    seq += 1;
                    produced += 1;
                }
            }
        }

        // ---- 5. nothing could move: wait for writability ----------------
        if blocked && conn.is_some() {
            if let Some(fd) = conn.as_ref().map(|c| c.as_raw_fd()) {
                poll_out(fd, 2);
            }
        }
    }
    let t_produced_done = mono_ns();

    // ---- final flush with a deadline -------------------------------------
    let flush_deadline = Instant::now() + Duration::from_secs(10);
    while !queue.is_empty() && Instant::now() < flush_deadline {
        let Some(c) = conn.as_ref() else { break };
        let front_len = queue.front().unwrap().len();
        match (&*c).write(queue.front().unwrap()) {
            Ok(n) if n == front_len => {
                queue.pop_front();
                written += 1;
            }
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                poll_out(c.as_raw_fd(), 50);
            }
            Err(_) => break,
        }
    }
    // Final flush barrier: the Pong carries the daemon's own count of what it
    // accepted for this session, which is stronger evidence than per-event Acks
    // (those are best-effort and get dropped when the client is slow to read).
    let mut server_watermark: Option<u64> = None;
    let mut server_accepted: Option<u64> = None;
    let mut barrier_rtt_us: Option<f64> = None;
    if let Some(c) = conn.as_ref() {
        let nonce = mono_ns();
        let ping = serde_json::json!({"schema_version": 1, "type": "Ping", "nonce": nonce});
        let tb = Instant::now();
        let mut sent_ok = false;
        if let Ok(mut blocking) = c.try_clone() {
            if blocking.set_nonblocking(false).is_ok() {
                sent_ok = write_json_line(&mut blocking, &ping).is_ok();
            }
        }
        if sent_ok {
            let deadline_b = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline_b {
                let hit = shared
                    .last_pong
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|v| v.get("nonce").and_then(|n| n.as_u64()) == Some(nonce));
                if let Some(v) = hit {
                    barrier_rtt_us = Some(tb.elapsed().as_secs_f64() * 1e6);
                    server_watermark = v
                        .get("watermarks")
                        .and_then(|m| m.get(&session))
                        .and_then(|x| x.as_u64());
                    server_accepted = v
                        .get("counts")
                        .and_then(|m| m.get(&session))
                        .and_then(|x| x.as_u64());
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // Wait until the ack count stops moving (max 5 s).
    let ack_deadline = Instant::now() + Duration::from_secs(5);
    let mut last = usize::MAX;
    while Instant::now() < ack_deadline {
        let now = shared.acked.lock().unwrap().len();
        if now == last {
            break;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(200));
    }
    let t_done = mono_ns();

    let acked_set = shared.acked.lock().unwrap().clone();
    let acked = acked_set.len() as u64;
    let mut unacked: Vec<u64> = (0..count).filter(|s| !acked_set.contains(s)).collect();
    unacked.sort_unstable();
    let ranges = ranges_of(&unacked);
    let out = serde_json::json!({
        "mode": "sender",
        "session": session,
        "count_requested": count,
        "produced": produced,
        "written_to_socket": written,
        "acked": acked,
        "unacked": unacked.len(),
        "unacked_ranges": ranges,
        "dropped_client_queue_full": dropped_full,
        "server_watermark": server_watermark,
        "server_accepted_count": server_accepted,
        "transport_loss": server_accepted.map(|a| written.saturating_sub(a)),
        "barrier_rtt_us": barrier_rtt_us,
        "send_errors": send_errors,
        "max_queue_depth": max_queue_depth,
        "queue_cap": queue_cap,
        "policy": policy,
        "connect_attempts": connect_attempts,
        "connect_failures": connect_failures,
        "reconnect_events": reconnects,
        "reconnect_succeeded": reconnect_total,
        "connect_failures_total": failed_attempt_total,
        "backoff_ceiling_ms": max_backoff_ms,
        "elapsed_ms": millis(t_done.saturating_sub(t_all0)),
        "production_ms": millis(t_produced_done.saturating_sub(t_all0)),
        "rate_eps": produced as f64 / (t_produced_done.saturating_sub(t_all0) as f64 / 1e9),
        "reader_thread_died": !shared.resolver_alive.load(Ordering::SeqCst),
        "timed_out": Instant::now() >= deadline,
    });
    println!("{out}");
    if let Some(p) = report {
        std::fs::write(&p, format!("{out}\n")).map_err(|e| format!("report {p}: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Journal audit — which sequences actually reached the daemon
// ---------------------------------------------------------------------------

fn audit(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let journal = PathBuf::from(a.s("journal", "/tmp/li-phase0-ipc/journal.jsonl"));
    let session = a.s("session", "sender");
    let expect = a.u64("expect", 0);

    let f = std::fs::File::open(&journal).map_err(|e| format!("open {journal:?}: {e}"))?;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut dup_lines = 0u64;
    let mut other = 0u64;
    let mut bad_lines = 0u64;
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| format!("read: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                bad_lines += 1;
                continue;
            }
        };
        if v.get("sid").and_then(|s| s.as_str()) != Some(session.as_str()) {
            other += 1;
            continue;
        }
        if let Some(sq) = v.get("seq").and_then(|s| s.as_u64()) {
            if !seen.insert(sq) {
                dup_lines += 1;
            }
        }
    }
    let mut missing: Vec<u64> = (0..expect).filter(|s| !seen.contains(s)).collect();
    missing.sort_unstable();
    let ranges = ranges_of(&missing);
    println!(
        "{}",
        serde_json::json!({
            "mode": "audit",
            "session": session,
            "journal": journal.display().to_string(),
            "expected_events": expect,
            "present": seen.len(),
            "lost": missing.len(),
            "lost_ranges": ranges,
            "duplicate_lines": dup_lines,
            "malformed_lines": bad_lines,
            "lines_of_other_sessions": other,
            "starts_at_zero": seen.iter().next().copied() == Some(0),
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 7 — duplicate event_id
// ---------------------------------------------------------------------------

fn dup(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let path = sock_path();
    let n = a.u64("count", 10);
    let session = a.s("session", "dup");
    let repeat_idx = a.u64("repeat-idx", 5).min(n.saturating_sub(1));
    let mut stream = connect(&path).map_err(|e| format!("connect: {e}"))?;
    let clone = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, clone);

    let mut lines: Vec<Vec<u8>> = Vec::new();
    for i in 0..n {
        let ev = Event::new(&session, i, "CommandStarted", serde_json::json!({"i": i}));
        let mut l = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
        l.push(b'\n');
        lines.push(l);
    }
    for l in &lines {
        stream.write_all(l).map_err(|e| format!("write: {e}"))?;
    }
    // Byte-identical resend: same event_id, same sequence, same payload.
    stream
        .write_all(&lines[repeat_idx as usize])
        .map_err(|e| format!("resend: {e}"))?;
    barrier(&mut stream, &mut reader).map_err(|e| format!("barrier: {e}"))?;

    let cred = li_ipc::peer_cred(&stream).ok();
    println!(
        "{}",
        serde_json::json!({
            "mode": "dup",
            "session": session,
            "unique_events_sent": n,
            "duplicate_resend_of_index": repeat_idx,
            "lines_written_total": n + 1,
            "self_peer_cred": cred.map(|c| serde_json::json!({"pid": c.pid, "uid": c.uid, "gid": c.gid})),
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 10b — existing connection must survive unlink of the socket file
// ---------------------------------------------------------------------------

fn hold(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let path = sock_path();
    let hold_ms = a.u64("hold-ms", 1500);
    let mut stream = connect(&path).map_err(|e| format!("connect: {e}"))?;
    let clone = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, clone);
    barrier(&mut stream, &mut reader).map_err(|e| format!("barrier: {e}"))?;
    println!("CONNECTED pid={}", std::process::id());
    std::io::stdout().flush().ok();
    std::thread::sleep(Duration::from_millis(hold_ms));

    let ev = Event::new(
        "hold",
        0,
        "CommandStarted",
        serde_json::json!({"phase": "after_unlink"}),
    );
    let mut l = serde_json::to_vec(&ev).map_err(|e| format!("json: {e}"))?;
    l.push(b'\n');
    let write_ok = stream.write_all(&l).is_ok();
    let barrier_ok = barrier(&mut stream, &mut reader).is_ok();
    let fresh = match connect(&path) {
        Ok(_) => "connected".to_string(),
        Err(e) => format!("{:?}", e.kind()),
    };
    println!(
        "{}",
        serde_json::json!({
            "mode": "hold",
            "existing_conn_write_ok": write_ok,
            "existing_conn_barrier_ok": barrier_ok,
            "new_connection_after_unlink": fresh,
            "socket_file_exists": path.exists(),
        })
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Utility: unlink the socket path from inside the harness (used by T10 B).
// The experiment rules forbid `rm` in shell; doing it here keeps the intent
// explicit and the reason auditable.
// ---------------------------------------------------------------------------

fn unlink_path(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw)?;
    let path = PathBuf::from(a.s("path", &li_ipc::default_socket_path()));
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!(
                "{}",
                serde_json::json!({"mode": "unlink", "path": path.display().to_string(), "removed": true})
            );
            Ok(())
        }
        Err(e) => Err(format!("unlink {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Test 10a — abrupt close without shutdown()
// ---------------------------------------------------------------------------

fn abrupt(raw: &[String]) -> Result<(), String> {
    use std::os::fd::IntoRawFd;
    let a = Args::parse(raw)?;
    let path = sock_path();
    let conns = a.u64("conns", 20);
    let linger0 = a.has("linger0");
    let mut closed = 0u64;
    let mut write_failures = 0u64;
    for i in 0..conns {
        let s = match connect(&path) {
            Ok(s) => s,
            Err(e) => return Err(format!("connect #{i}: {e}")),
        };
        let mut w = &s;
        let ev = Event::new("abrupt", i, "CommandStarted", serde_json::json!({"i": i}));
        if write_json_line(&mut w, &ev).is_err() {
            write_failures += 1;
            continue;
        }
        if linger0 {
            let lg = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: valid fd, valid linger struct of the right size; a failure
            // only weakens this test, which is reported via `linger0_applied`.
            unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    &lg as *const libc::linger as *const libc::c_void,
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                );
            }
        }
        // No shutdown(), no protocol goodbye: plain close(2) of the fd.
        let fd = s.into_raw_fd();
        // SAFETY: `fd` was just taken from into_raw_fd and is closed once here.
        unsafe { libc::close(fd) };
        closed += 1;
        std::thread::sleep(Duration::from_millis(2));
    }
    // Give the daemon time to notice all the closes, then it prints its stats.
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "{}",
        serde_json::json!({
            "mode": "abrupt",
            "conns_closed_abruptly": closed,
            "write_failures": write_failures,
            "linger0": linger0,
        })
    );
    Ok(())
}
