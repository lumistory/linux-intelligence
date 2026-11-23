//! Test client for the simulated helper.
//!
//! Modes:
//!   send --socket P --opcode N [--path S] [--repeat N]
//!   raw  --socket P --bytes N [--newline] [--repeat N]
//!   rawstr --socket P --line S
//!   storm --socket P --clients N --requests M [--opcode N]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use serde_json::json;

fn main() {
    if let Err(e) = run() {
        eprintln!("client: {e}");
        std::process::exit(2);
    }
}

fn arg_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn arg_num(args: &[String], name: &str) -> Option<u64> {
    arg_val(args, name).and_then(|v| v.parse().ok())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("usage: client send|raw|rawstr|storm ...")?.clone();
    let socket = arg_val(&args, "--socket").ok_or("--socket required")?;

    match mode.as_str() {
        "send" => {
            let opcode = arg_num(&args, "--opcode").ok_or("--opcode required")?;
            let path = arg_val(&args, "--path");
            let repeat = arg_num(&args, "--repeat").unwrap_or(1).max(1);
            let mut req = json!({ "opcode": opcode });
            if let Some(p) = path {
                req["args"] = json!({ "path": p });
            } else {
                req["args"] = json!({});
            }
            let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
            let mut conn = connect(&socket)?;
            let rd = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
            let mut rd = rd;
            for _ in 0..repeat {
                writeln!(conn, "{line}").map_err(|e| format!("write: {e}"))?;
                let mut resp = String::new();
                rd.read_line(&mut resp).map_err(|e| format!("read: {e}"))?;
                println!("{}", resp.trim_end());
            }
        }
        "rawstr" => {
            // Send an arbitrary literal line (already-escaped by the caller).
            let line = arg_val(&args, "--line").ok_or("--line required")?;
            let mut conn = connect(&socket)?;
            let mut rd = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
            writeln!(conn, "{line}").map_err(|e| format!("write: {e}"))?;
            conn.flush().ok();
            let mut resp = String::new();
            let _ = rd.read_line(&mut resp);
            println!("{}", resp.trim_end());
        }
        "raw" => {
            // A single frame of N bytes with no structure at all, followed
            // (optionally) by a newline. Used for oversize / no-newline tests.
            let bytes = arg_num(&args, "--bytes").ok_or("--bytes required")? as usize;
            let newline = args.iter().any(|a| a == "--newline");
            let repeat = arg_num(&args, "--repeat").unwrap_or(1).max(1);
            let body = vec![b'A'; bytes];
            let mut conn = connect(&socket)?;
            let t0 = Instant::now();
            let mut wrote = 0u64;
            for _ in 0..repeat {
                match conn.write_all(&body) {
                    Ok(()) => wrote += bytes as u64,
                    Err(e) => {
                        println!(
                            "{}",
                            json!({ "case": "raw_write_error", "wrote_bytes": wrote,
                                    "error": e.to_string(), "errno_kind": format!("{:?}", e.kind()),
                                    "elapsed_ms": t0.elapsed().as_millis() as u64 })
                        );
                        return Ok(());
                    }
                }
                if newline {
                    let _ = conn.write_all(b"\n");
                }
            }
            let _ = conn.flush();
            // Try to read a response; the helper may have closed on us.
            let mut rd = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
            let mut resp = String::new();
            let read_ok = rd.read_line(&mut resp).is_ok() && !resp.is_empty();
            println!(
                "{}",
                json!({
                    "case": "raw_frame",
                    "requested_bytes": bytes, "repeat": repeat, "trailing_newline": newline,
                    "wrote_bytes": wrote,
                    "response_received": read_ok,
                    "response": resp.trim_end(),
                    "elapsed_ms": t0.elapsed().as_millis() as u64,
                })
            );
        }
        "storm" => {
            let clients = arg_num(&args, "--clients").ok_or("--clients required")? as usize;
            let requests = arg_num(&args, "--requests").ok_or("--requests required")? as usize;
            let opcode = arg_num(&args, "--opcode").unwrap_or(1);
            storm(&socket, clients, requests, opcode)?;
        }
        other => return Err(format!("unknown mode {other}")),
    }
    Ok(())
}

fn connect(path: &str) -> Result<UnixStream, String> {
    UnixStream::connect(path).map_err(|e| format!("connect {path}: {e}"))
}

fn storm(socket: &str, clients: usize, requests: usize, opcode: u64) -> Result<(), String> {
    // Pre-build the frame once; identical bytes from every client so the only
    // variable is concurrency.
    let line = if opcode == 2 || opcode == 3 || opcode == 4 {
        serde_json::to_string(&json!({ "opcode": opcode, "args": { "path": "probe.txt" } }))
    } else {
        serde_json::to_string(&json!({ "opcode": opcode, "args": {} }))
    }
    .map_err(|e| e.to_string())?
        + "\n";
    let line = Arc::new(line);

    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut handles = Vec::with_capacity(clients);
    let mut errors = 0usize;
    let t0 = Instant::now();

    for c in 0..clients {
        let barrier = Arc::clone(&barrier);
        let line = Arc::clone(&line);
        let socket = socket.to_owned();
        handles.push(std::thread::spawn(move || -> usize {
            // All sockets are created *before* the barrier so accept() is not
            // part of the measured window.
            let stream = match UnixStream::connect(&socket) {
                Ok(s) => s,
                Err(_) => return requests,
            };
            let mut w = stream.try_clone().unwrap_or_else(|_| stream.try_clone().unwrap());
            let mut rd = BufReader::new(stream);
            barrier.wait();
            let mut errs = 0usize;
            for _ in 0..requests {
                if w.write_all(line.as_bytes()).is_err() {
                    errs += 1;
                    break;
                }
                let mut resp = String::new();
                match rd.read_line(&mut resp) {
                    Ok(0) => {
                        errs += 1;
                        break;
                    }
                    Ok(_) => {
                        if !resp.contains("\"ok\":true") && !resp.contains("\"ok\":false") {
                            errs += 1;
                        }
                    }
                    Err(_) => {
                        errs += 1;
                        break;
                    }
                }
            }
            let _ = c;
            errs
        }));
    }
    barrier.wait();
    for h in handles {
        match h.join() {
            Ok(e) => errors += e,
            Err(_) => errors += requests,
        }
    }
    let elapsed = t0.elapsed();
    let total = clients * requests;
    println!(
        "{}",
        json!({
            "case": "storm_summary",
            "clients": clients, "requests_per_client": requests,
            "requests_sent": total, "client_errors": errors,
            "elapsed_ms": elapsed.as_millis() as u64,
            "throughput_per_s": if elapsed.as_secs_f64() > 0.0 {
                (total as f64 / elapsed.as_secs_f64()).round() as u64
            } else { 0 },
        })
    );
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
