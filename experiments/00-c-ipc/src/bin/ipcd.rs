//! `ipcd` — daemon prototype for experiment 00-C.
//!
//! Deliberately small: accept → SO_PEERCRED gate → per-connection reader with a
//! hard frame limit → bounded ingest queue → single worker (journal + dedup +
//! sequence check) → Ack.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use li_ipc::{
    peer_cred, ClientMsg, Event, ServerMsg, DISCARD_CAP, MAX_FRAME_BYTES, SCHEMA_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// Async counterpart of `li_ipc::write_json_line` for the tokio side.
async fn write_msg<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &ServerMsg) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await
}

#[derive(Debug)]
struct Config {
    socket: PathBuf,
    journal: PathBuf,
    allowed_uid: u32,
    queue_cap: usize,
    slow_ms: u64,
    ack: bool,
    reply_cap: usize,
    dir_mode: u32,
    sock_mode: u32,
    journal_append: bool,
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    rejected_uid: AtomicU64,
    oversize_frames: AtomicU64,
    bad_frames: AtomicU64,
    dedup_dropped: AtomicU64,
    seq_violations: AtomicU64,
    conns_opened: AtomicU64,
    conns_closed: AtomicU64,
    events_ingested: AtomicU64,
    acks_sent: AtomicU64,
    acks_dropped: AtomicU64,
    bytes_read: AtomicU64,
}

struct Ingest {
    event: Event,
    /// Reply channel of the connection that produced the event.
    reply: mpsc::Sender<ServerMsg>,
}

fn parse_args() -> Result<Config, String> {
    let mut socket = PathBuf::from(li_ipc::default_socket_path());
    let mut journal = PathBuf::from("/tmp/li-phase0-ipc/journal.jsonl");
    let mut allowed_uid = None;
    let mut queue_cap = 1024usize;
    let mut slow_ms = 0u64;
    let mut ack = true;
    let mut reply_cap = 8192usize;
    let mut dir_mode = 0o700u32;
    let mut sock_mode = 0o600u32;
    let mut journal_append = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--socket" => socket = PathBuf::from(val("--socket")?),
            "--journal" => journal = PathBuf::from(val("--journal")?),
            "--allowed-uid" => {
                allowed_uid = Some(
                    val("--allowed-uid")?
                        .parse::<u32>()
                        .map_err(|e| format!("bad --allowed-uid: {e}"))?,
                )
            }
            "--queue-cap" => {
                queue_cap = val("--queue-cap")?
                    .parse()
                    .map_err(|e| format!("bad --queue-cap: {e}"))?
            }
            "--slow-ms" => {
                slow_ms = val("--slow-ms")?
                    .parse()
                    .map_err(|e| format!("bad --slow-ms: {e}"))?
            }
            "--dir-mode" => {
                dir_mode = u32::from_str_radix(&val("--dir-mode")?, 8)
                    .map_err(|e| format!("bad --dir-mode (octal): {e}"))?
            }
            "--sock-mode" => {
                sock_mode = u32::from_str_radix(&val("--sock-mode")?, 8)
                    .map_err(|e| format!("bad --sock-mode (octal): {e}"))?
            }
            "--no-ack" => ack = false,
            "--reply-cap" => {
                reply_cap = val("--reply-cap")?
                    .parse()
                    .map_err(|e| format!("bad --reply-cap: {e}"))?
            }
            "--journal-append" => journal_append = true,
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    // SAFETY: geteuid has no preconditions and no failure mode.
    let euid = unsafe { libc::geteuid() };
    Ok(Config {
        socket,
        journal,
        allowed_uid: allowed_uid.unwrap_or(euid),
        queue_cap: queue_cap.max(1),
        slow_ms,
        ack,
        reply_cap: reply_cap.max(1),
        dir_mode,
        sock_mode,
        journal_append,
    })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::process::ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ipcd: {e}");
            eprintln!(
                "usage: ipcd [--socket PATH] [--journal PATH] [--allowed-uid UID] \
                 [--queue-cap N] [--slow-ms N] [--no-ack] [--dir-mode OCTAL] \
                 [--sock-mode OCTAL] [--journal-append]"
            );
            return std::process::ExitCode::from(2);
        }
    };
    match run(cfg).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ipcd: fatal: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cfg: Config) -> Result<(), String> {
    if let Some(parent) = cfg.socket.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        set_dir_mode(parent, cfg.dir_mode)?;
    }
    // A stale socket from a previous run must not block binding; we own the dir.
    match std::fs::remove_file(&cfg.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("unlink stale socket {:?}: {e}", cfg.socket)),
    }

    let listener =
        UnixListener::bind(&cfg.socket).map_err(|e| format!("bind {:?}: {e}", cfg.socket))?;
    set_dir_mode(&cfg.socket, cfg.sock_mode)?;

    let journal = if cfg.journal_append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.journal)
            .map_err(|e| format!("open-append journal {:?}: {e}", cfg.journal))?
    } else {
        std::fs::File::create(&cfg.journal)
            .map_err(|e| format!("create journal {:?}: {e}", cfg.journal))?
    };

    let counters = Arc::new(Counters::default());
    let (tx, rx) = mpsc::channel::<Ingest>(cfg.queue_cap);
    let worker = tokio::spawn(worker_loop(
        rx,
        journal,
        cfg.slow_ms,
        cfg.ack,
        counters.clone(),
    ));

    eprintln!(
        "ipcd: listening on {} (allowed_uid={} queue_cap={} slow_ms={} ack={} sock_mode={:o})",
        cfg.socket.display(),
        cfg.allowed_uid,
        cfg.queue_cap,
        cfg.slow_ms,
        cfg.ack,
        cfg.sock_mode
    );

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("signal: {e}"))?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("ipcd: accept error: {e}");
                        continue;
                    }
                };
                counters.conns_opened.fetch_add(1, Ordering::Relaxed);
                let tx = tx.clone();
                let counters = counters.clone();
                let allowed = cfg.allowed_uid;
                let reply_cap = cfg.reply_cap;
                tokio::spawn(async move {
                    handle_conn(stream, tx, allowed, reply_cap, counters.clone()).await;
                    counters.conns_closed.fetch_add(1, Ordering::Relaxed);
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("ipcd: SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("ipcd: SIGTERM, shutting down");
                break;
            }
        }
    }

    drop(tx);
    drop(listener);
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = std::fs::remove_file(&cfg.socket);
    print_stats(&counters);
    Ok(())
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {path:?}: {e}"))
}

fn print_stats(c: &Counters) {
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    println!(
        "STATS {}",
        serde_json::json!({
            "accepted": g(&c.accepted),
            "rejected_uid": g(&c.rejected_uid),
            "oversize_frames": g(&c.oversize_frames),
            "bad_frames": g(&c.bad_frames),
            "dedup_dropped": g(&c.dedup_dropped),
            "seq_violations": g(&c.seq_violations),
            "conns_opened": g(&c.conns_opened),
            "conns_closed": g(&c.conns_closed),
            "events_ingested": g(&c.events_ingested),
            "acks_sent": g(&c.acks_sent),
            "acks_dropped": g(&c.acks_dropped),
            "bytes_read": g(&c.bytes_read),
        })
    );
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

async fn handle_conn(
    stream: UnixStream,
    tx: mpsc::Sender<Ingest>,
    allowed_uid: u32,
    reply_cap: usize,
    counters: Arc<Counters>,
) {
    let cred = match peer_cred(&stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ipcd: SO_PEERCRED failed: {e}");
            return;
        }
    };
    let (read_half, write_half) = stream.into_split();
    let mut writer = write_half;

    if cred.uid != allowed_uid {
        eprintln!(
            "ipcd: reject peer pid={} uid={} gid={} (allowed_uid={})",
            cred.pid, cred.uid, cred.gid, allowed_uid
        );
        counters.rejected_uid.fetch_add(1, Ordering::Relaxed);
        let msg = ServerMsg::error(
            "peer_uid_rejected",
            &format!("peer uid {} is not allowed", cred.uid),
        );
        let _ = write_msg(&mut writer, &msg).await;
        let _ = writer.shutdown().await;
        return;
    }
    eprintln!(
        "ipcd: accept peer pid={} uid={} gid={}",
        cred.pid, cred.uid, cred.gid
    );

    // Writer task: serialises replies for this connection. Bounded, so a client
    // that never reads cannot stall the ingest worker; acks are dropped and
    // counted instead.
    let (reply_tx, mut reply_rx) = mpsc::channel::<ServerMsg>(reply_cap);
    let wcounters = counters.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = reply_rx.recv().await {
            if write_msg(&mut writer, &msg).await.is_err() {
                break;
            }
            wcounters.acks_sent.fetch_add(1, Ordering::Relaxed);
        }
        let _ = writer.shutdown().await;
    });

    let reader = BufReader::with_capacity(64 * 1024, read_half);
    read_loop(reader, tx, reply_tx, counters.clone()).await;
    let _ = writer_task.await;
}

enum Frame {
    Line(Vec<u8>),
    TooLarge(u64),
    Eof,
    DiscardOverflow,
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> std::io::Result<Frame> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut oversize: Option<u64> = None;
    loop {
        let (used, found, eof) = {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                (0, false, true)
            } else {
                let (used, found) = match available.iter().position(|&b| b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (available.len(), false),
                };
                let chunk = &available[..used];
                match oversize {
                    None => {
                        if buf.len() + chunk.len() > max {
                            oversize = Some((buf.len() + chunk.len()) as u64);
                            buf.clear();
                        } else {
                            buf.extend_from_slice(chunk);
                        }
                    }
                    Some(ref mut n) => *n += chunk.len() as u64,
                }
                (used, found, false)
            }
        };
        if eof {
            return Ok(match oversize {
                Some(bytes) => Frame::TooLarge(bytes),
                None if buf.is_empty() => Frame::Eof,
                None => Frame::Line(buf),
            });
        }
        r.consume(used);
        if let Some(n) = oversize {
            if n > DISCARD_CAP {
                return Ok(Frame::DiscardOverflow);
            }
        }
        if found {
            return Ok(match oversize {
                Some(bytes) => Frame::TooLarge(bytes),
                None => {
                    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                        buf.pop();
                    }
                    Frame::Line(buf)
                }
            });
        }
    }
}

async fn read_loop(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    tx: mpsc::Sender<Ingest>,
    reply_tx: mpsc::Sender<ServerMsg>,
    counters: Arc<Counters>,
) {
    loop {
        let frame = match read_frame(&mut reader, MAX_FRAME_BYTES).await {
            Ok(f) => f,
            Err(e) => {
                // Abrupt client close (test 10a) surfaces here as ECONNRESET;
                // it is expected input, not a daemon fault.
                eprintln!("ipcd: read error (conn end): {e}");
                return;
            }
        };
        match frame {
            Frame::Eof => return,
            Frame::DiscardOverflow => {
                eprintln!("ipcd: oversized frame exceeded discard cap, dropping connection");
                let _ = reply_tx
                    .send(ServerMsg::error("frame_too_large", "discard cap exceeded"))
                    .await;
                return;
            }
            Frame::TooLarge(bytes) => {
                counters.oversize_frames.fetch_add(1, Ordering::Relaxed);
                let msg = ServerMsg::error(
                    "payload_too_large",
                    &format!("frame of {bytes} bytes exceeds limit of {MAX_FRAME_BYTES}"),
                );
                if reply_tx.send(msg).await.is_err() {
                    return;
                }
                // Connection stays open and usable.
            }
            Frame::Line(line) => {
                counters
                    .bytes_read
                    .fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
                if line.is_empty() {
                    continue;
                }
                match li_ipc::parse_client_line(&line) {
                    Ok(ClientMsg::Event(ev)) => {
                        if ev.schema_version != SCHEMA_VERSION {
                            counters.bad_frames.fetch_add(1, Ordering::Relaxed);
                            let _ = reply_tx
                                .send(ServerMsg::error(
                                    "schema_version_unsupported",
                                    &format!("got {}", ev.schema_version),
                                ))
                                .await;
                            continue;
                        }
                        let item = Ingest {
                            event: ev,
                            reply: reply_tx.clone(),
                        };
                        // Bounded: this await is where backpressure is applied.
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    Ok(ClientMsg::Ping { nonce }) => {
                        // Routed through the same queue so a Pong is a flush
                        // barrier for everything sent before it. The nonce rides
                        // in the payload so the worker can echo it.
                        let ev = Event::new("__ping__", 0, "Ping", serde_json::json!({}));
                        let ev = Event {
                            payload: serde_json::json!({ "nonce": nonce }),
                            ..ev
                        };
                        let item = Ingest {
                            event: ev,
                            reply: reply_tx.clone(),
                        };
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    Err(code) => {
                        counters.bad_frames.fetch_add(1, Ordering::Relaxed);
                        let msg = ServerMsg::error(&code, "malformed frame");
                        if reply_tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker: journal, dedup, sequence check, ack
// ---------------------------------------------------------------------------

async fn worker_loop(
    mut rx: mpsc::Receiver<Ingest>,
    journal: std::fs::File,
    slow_ms: u64,
    ack: bool,
    counters: Arc<Counters>,
) {
    let mut journal = journal;
    let mut seen: HashSet<String> = HashSet::new();
    let mut last_seq: HashMap<String, u64> = HashMap::new();
    let mut accepted_count: HashMap<String, u64> = HashMap::new();
    let slow = Duration::from_millis(slow_ms);

    while let Some(item) = rx.recv().await {
        counters.events_ingested.fetch_add(1, Ordering::Relaxed);
        if !slow.is_zero() {
            tokio::time::sleep(slow).await;
        }

        let ev = item.event;
        if ev.kind == "Ping" {
            let nonce = ev
                .payload
                .get("nonce")
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let marks: std::collections::BTreeMap<String, u64> =
                last_seq.iter().map(|(k, v)| (k.clone(), *v)).collect();
            let counts: std::collections::BTreeMap<String, u64> = accepted_count
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            let _ = item.reply.try_send(ServerMsg::pong(nonce, marks, counts));
            continue;
        }

        counters.accepted.fetch_add(1, Ordering::Relaxed);

        if !seen.insert(ev.event_id.clone()) {
            counters.dedup_dropped.fetch_add(1, Ordering::Relaxed);
            if ack {
                let _ = item
                    .reply
                    .try_send(ServerMsg::ack(&ev.event_id, ev.sequence));
            }
            continue;
        }

        match last_seq.get(&ev.session_id).copied() {
            None => {
                if ev.sequence != 0 {
                    counters.seq_violations.fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(prev) => {
                if ev.sequence != prev + 1 {
                    counters.seq_violations.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        last_seq.insert(ev.session_id.clone(), ev.sequence);
        *accepted_count.entry(ev.session_id.clone()).or_insert(0) += 1;

        let line = serde_json::json!({
            "sid": ev.session_id,
            "seq": ev.sequence,
            "eid": ev.event_id,
            "t": ev.ts_mono_ns,
        });
        if let Err(e) = writeln!(journal, "{line}").and_then(|()| journal.flush()) {
            eprintln!("ipcd: journal write failed: {e}");
        }

        if ack
            && item
                .reply
                .try_send(ServerMsg::ack(&ev.event_id, ev.sequence))
                .is_err()
        {
            counters.acks_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
