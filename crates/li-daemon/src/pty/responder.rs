//! First-class terminal-query responder (P3-02).
//!
//! Answers DSR, DA1, DA2, kitty keyboard, OSC 10/11, `ESC[18t`, DECRQM,
//! XTVERSION, and XTGETTCAP so interactive shells (especially `fish`) start in
//! milliseconds instead of stalling 10+ seconds waiting on terminal probes.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsweredQuery {
    pub query: String,
    pub reply: String,
}

pub fn respond(bytes: &[u8], rows: u16, cols: u16) -> (Vec<u8>, Vec<AnsweredQuery>) {
    let mut out: Vec<u8> = Vec::new();
    let mut seen: Vec<AnsweredQuery> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b']' {
            if let Some((end, body)) = scan_osc(bytes, i + 2) {
                let text = String::from_utf8_lossy(&body);
                match text.as_ref() {
                    "11;?" => {
                        let r = "\x1b]11;rgb:0000/0000/0000\x1b\\";
                        out.extend_from_slice(r.as_bytes());
                        seen.push(AnsweredQuery {
                            query: "OSC 11;?".into(),
                            reply: r.into(),
                        });
                    }
                    "10;?" => {
                        let r = "\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
                        out.extend_from_slice(r.as_bytes());
                        seen.push(AnsweredQuery {
                            query: "OSC 10;?".into(),
                            reply: r.into(),
                        });
                    }
                    _ => {}
                }
                i = end;
                continue;
            }
            break;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'P' {
            if let Some((end, body)) = scan_dcs(bytes, i + 2) {
                let text = String::from_utf8_lossy(&body);
                if let Some(hexcaps) = text.strip_prefix("+q") {
                    for cap in hexcaps.split(';') {
                        let reply = format!("\x1bP0+r{cap}\x1b\\");
                        out.extend_from_slice(reply.as_bytes());
                        seen.push(AnsweredQuery {
                            query: format!("XTGETTCAP {}", decode_hex(cap)),
                            reply: "0+r".into(),
                        });
                    }
                }
                i = end;
                continue;
            }
            break;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if let Some((end, params, final_byte)) = scan_csi(bytes, i + 2) {
                let q = format!(
                    "CSI {}{}",
                    String::from_utf8_lossy(&params),
                    final_byte as char
                );
                let reply = match (params.as_slice(), final_byte) {
                    (b"", b'n') | (b"5", b'n') | (b"6", b'n') => Some("\x1b[1;1R".to_string()),
                    (b"", b'c') | (b"0", b'c') => Some("\x1b[?1;2c".to_string()),
                    (b">", b'c') | (b">0", b'c') => Some("\x1b[>0;276;0c".to_string()),
                    (b"?", b'u') => Some("\x1b[?0u".to_string()),
                    (b">", b'q') | (b">0", b'q') => Some("\x1bP>|li-pty(0.1)\x1b\\".to_string()),
                    (b"18", b't') => Some(format!("\x1b[8;{rows};{cols}t")),
                    (p, b'$') if p.first() == Some(&b'?') => Some("\x1b[?0$y".to_string()),
                    _ => None,
                };
                if let Some(r) = reply {
                    out.extend_from_slice(r.as_bytes());
                    seen.push(AnsweredQuery { query: q, reply: r });
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
    if !clean.len().is_multiple_of(2) {
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
