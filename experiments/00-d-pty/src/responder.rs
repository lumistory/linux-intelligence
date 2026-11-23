//! Minimal terminal-query responder.
//!
//! A PTY is a byte pipe; it is *not* a terminal. Programs that care (fish,
//! vim, less, anything using terminfo's `pixel`/colour probes) emit escape
//! sequences that ask the terminal a question and then **block** until the
//! answer arrives. With no responder, the child stops making progress and the
//! harness sees an empty stream — which is exactly what happened on the first
//! run of this experiment against fish.
//!
//! This implements the small, well-defined set of queries that real tools
//! actually send. Every query is recorded, so the report can show which ones
//! were needed.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Answered {
    pub query: String,
    pub reply: String,
}

/// Scan `bytes` for terminal queries and produce the replies a real terminal
/// would send. `rows`/`cols` are used for the cursor-position and
/// text-area-size reports.
pub fn respond(bytes: &[u8], rows: u16, cols: u16) -> (Vec<u8>, Vec<Answered>) {
    let mut out: Vec<u8> = Vec::new();
    let mut seen: Vec<Answered> = Vec::new();
    let mut i = 0usize;
    let name = |q: &str, r: String| Answered { query: q.to_string(), reply: r };

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        // ESC ] ... (OSC)
        if i + 1 < bytes.len() && bytes[i + 1] == b']' {
            if let Some((end, body)) = scan_osc(bytes, i + 2) {
                let text = String::from_utf8_lossy(&body).into_owned();
                match text.as_str() {
                    "11;?" => {
                        // Background colour query: report a black background.
                        let r = "\x1b]11;rgb:0000/0000/0000\x1b\\";
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name("OSC 11;? (bg colour)", r.into()));
                    }
                    "10;?" => {
                        let r = "\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name("OSC 10;? (fg colour)", r.into()));
                    }
                    _ => {}
                }
                i = end;
                continue;
            }
            break;
        }
        // ESC P ... (DCS: XTGETTCAP, DECRQSS)
        if i + 1 < bytes.len() && bytes[i + 1] == b'P' {
            if let Some((end, body)) = scan_dcs(bytes, i + 2) {
                let text = String::from_utf8_lossy(&body).into_owned();
                if let Some(hexcaps) = text.strip_prefix("+q") {
                    // XTGETTCAP: one or more ';'-separated hex-encoded names.
                    for cap in hexcaps.split(';') {
                        let decoded = decode_hex(cap);
                        // We advertise nothing: a well-formed "not found" is
                        // the correct answer and keeps the program on its
                        // fallback path, which is what we want to observe.
                        let reply = format!("\x1bP0+r{cap}\x1b\\");
                        out.extend_from_slice(reply.as_bytes());
                        seen.push(name(
                            &format!("XTGETTCAP {decoded}"),
                            "not-supported (0+r)".to_string(),
                        ));
                    }
                } else if text.starts_with("$q") || text.starts_with("+r") {
                    seen.push(name(&format!("DCS {text}"), "ignored".to_string()));
                }
                i = end;
                continue;
            }
            break;
        }
        // ESC [ ... (CSI)
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if let Some((end, params, final_byte)) = scan_csi(bytes, i + 2) {
                let q = format!("CSI {}{}", String::from_utf8_lossy(&params), final_byte as char);
                match (params.as_slice(), final_byte) {
                    // DSR: report cursor position (1,1 — we do not track it).
                    (b"", b'n') | (b"5", b'n') | (b"6", b'n') => {
                        let r = "\x1b[1;1R".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // DA1: claim a VT100 with ANSI colour (classic, safe).
                    (b"", b'c') | (b"0", b'c') => {
                        let r = "\x1b[?1;2c".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // DA2 (secondary device attributes).
                    (b">", b'c') | (b">0", b'c') => {
                        let r = "\x1b[>0;276;0c".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // Kitty keyboard protocol query: report "no support" so the
                    // program uses the legacy encoding.
                    (b"?", b'u') => {
                        let r = "\x1b[?0u".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // XTVERSION query.
                    (b">", b'q') | (b">0", b'q') => {
                        let r = "\x1bP>|li-pty-proto(0.1)\x1b\\".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // Text area size in characters.
                    (b"18", b't') => {
                        let r = format!("\x1b[8;{rows};{cols}t");
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    // DECRQM: every mode is reported "not recognised"; that is
                    // a valid answer and programs handle it.
                    (p, b'$') if p.first() == Some(&b'?') => {
                        let r = "\x1b[?0$y".to_string();
                        out.extend_from_slice(r.as_bytes());
                        seen.push(name(&q, r));
                    }
                    _ => {}
                }
                i = end;
                continue;
            }
            break;
        }
        i += 1;
    }
    (out, seen)
}

/// Returns (index after terminator, body).
fn scan_osc(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let mut j = start;
    while j < bytes.len() {
        if bytes[j] == 0x07 {
            return Some((j + 1, bytes[start..j].to_vec()));
        }
        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            return Some((j + 2, bytes[start..j].to_vec()));
        }
        j += 1;
    }
    None
}

fn scan_dcs(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let mut j = start;
    while j + 1 < bytes.len() {
        if bytes[j] == 0x1b && bytes[j + 1] == b'\\' {
            return Some((j + 2, bytes[start..j].to_vec()));
        }
        j += 1;
    }
    None
}

fn scan_csi(bytes: &[u8], start: usize) -> Option<(usize, Vec<u8>, u8)> {
    let mut j = start;
    while j < bytes.len() {
        let b = bytes[j];
        if (0x40..=0x7e).contains(&b) {
            return Some((j + 1, bytes[start..j].to_vec(), b));
        }
        j += 1;
    }
    None
}

fn decode_hex(s: &str) -> String {
    let clean: Vec<u8> = s.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    if clean.len() % 2 != 0 {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    for pair in clean.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push(hi << 4 | lo);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Summary of the queries a program made, for the report.
pub fn summarise(seen: &[Answered]) -> Value {
    json!({
        "query_count": seen.len(),
        "queries": seen.iter().map(|a| json!({
            "query": a.query,
            "reply": a.reply,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_dsr() {
        let (out, seen) = respond(b"\x1b[6n", 24, 80);
        assert_eq!(out, b"\x1b[1;1R");
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn answers_osc_background_query() {
        let (out, _) = respond(b"\x1b]11;?\x1b\\", 24, 80);
        assert_eq!(String::from_utf8_lossy(&out), "\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn answers_kitty_keyboard_query() {
        let (out, _) = respond(b"\x1b[?u", 24, 80);
        assert_eq!(out, b"\x1b[?0u");
    }

    #[test]
    fn answers_xtgettcap_with_not_supported() {
        let (out, seen) = respond(b"\x1bP+q696e646e\x1b\\", 24, 80);
        // XTGETTCAP "not supported" reply. The leading '+q' of the request is
        // consumed as a prefix, so the reply carries "0+r" followed by the
        // *same hex name* the program queried (not re-prefixed).
        assert_eq!(String::from_utf8_lossy(&out), "\x1bP0+r696e646e\x1b\\");
        assert!(seen[0].query.contains("indn"));
        assert!(seen[0].reply.contains("not-supported"));
    }

    #[test]
    fn ignores_plain_output() {
        let (out, seen) = respond(b"hello world\r\n", 24, 80);
        assert!(out.is_empty());
        assert!(seen.is_empty());
    }

    #[test]
    fn incomplete_sequence_is_not_answered() {
        // A truncated query must not produce a bogus reply.
        let (out, _) = respond(b"abc\x1b[6", 24, 80);
        assert!(out.is_empty());
    }

    #[test]
    fn hex_decode_roundtrip() {
        assert_eq!(decode_hex("7175657279"), "query");
    }
}
