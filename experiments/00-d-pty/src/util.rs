//! Small formatting helpers shared by the scenes and the report writer.

/// Escape control characters and non-ASCII bytes so a raw PTY transcript can be
/// stored and diffed as plain text without losing anything.
pub fn escape_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 64);
    for ch in s.chars() {
        match ch {
            '\u{1b}' => out.push_str("\\e"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n\n"),
            '\t' => out.push_str("\\t"),
            '\u{7f}' => out.push_str("\\x7f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) > 0x7e => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Hex dump of a byte slice, for the "what exactly did the shell emit" section.
pub fn hex(bytes: &[u8], limit: usize) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len().min(limit) * 3);
    for b in bytes.iter().take(limit) {
        let _ = write!(s, "{b:02x} ");
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_reversible_in_text() {
        assert_eq!(escape_control("a\u{1b}[1mb\r\n"), "a\\e[1mb\\r\\n\n");
    }

    #[test]
    fn hex_limits() {
        assert_eq!(hex(&[0x1b, 0x5b], 10), "1b 5b");
        assert_eq!(hex(&[0x1b, 0x5b], 1), "1b");
    }
}

/// Strip ANSI/VT escape sequences (CSI, OSC, and the two-byte `ESC x` forms)
/// from a byte-stream decoded as text.
///
/// This is *not* a terminal emulator: it does not interpret cursor motion, so
/// it cannot reconstruct a screen. It is only enough to recover the printable
/// text of a line-oriented stream (which is all the batch-mode control case
/// needs) and to make `bare_line_eq` comparisons possible.
pub fn strip_ansi(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c != '\u{1b}' {
            out.push(c);
            i += 1;
            continue;
        }
        // ESC ...
        i += 1;
        if i >= b.len() {
            break;
        }
        match b[i] {
            '[' => {
                // CSI: parameters until a final byte in 0x40..=0x7e
                i += 1;
                while i < b.len() {
                    let d = b[i] as u32;
                    i += 1;
                    if (0x40..=0x7e).contains(&d) {
                        break;
                    }
                }
            }
            ']' => {
                // OSC: terminated by BEL or ST (ESC \)
                i += 1;
                while i < b.len() {
                    if b[i] == '\u{7}' {
                        i += 1;
                        break;
                    }
                    if b[i] == '\u{1b}' && i + 1 < b.len() && b[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            'P' | '^' | '_' => {
                // DCS / PM / APC: same terminator as OSC
                i += 1;
                while i < b.len() {
                    if b[i] == '\u{1b}' && i + 1 < b.len() && b[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // Two-character sequence (e.g. ESC = , ESC > , ESC M)
                i += 1;
            }
        }
    }
    out
}

/// Split a raw PTY stream into "printed lines" the way a reader would see them,
/// without emulating cursor movement:
///   * `\r` and `\n` both terminate a line;
///   * escape sequences are removed;
///   * trailing spaces and carriage-return artifacts are trimmed.
///
/// This deliberately collapses `\r\n` into a single break and treats a bare
/// `\r` as a break too, which is what makes it usable across bash, zsh and
/// fish without a full terminal model.
pub fn printed_lines(s: &str) -> Vec<String> {
    strip_ansi(s)
        .split(['\r', '\n'])
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Count printed lines that are *exactly* `token`.
///
/// Used to prove a command's own output arrived: the terminal echo of a typed
/// command line never forms such a line, but `echo TOKEN` always does.
pub fn count_bare_lines(s: &str, token: &str) -> usize {
    printed_lines(s).iter().filter(|l| l.as_str() == token).count()
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("a\u{1b}[1mb\u{1b}[0m"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]133;A\u{1b}\\b"), "ab");
        assert_eq!(strip_ansi("\u{1b}=\u{1b}>ok"), "ok");
    }

    #[test]
    fn bare_lines_survive_bracketed_paste_noise() {
        let stream = "echo TOK\r\n\u{1b}[?2004l\rTOK\r\n\u{1b}[?2004hP> ";
        assert_eq!(count_bare_lines(stream, "TOK"), 1);
    }

    #[test]
    fn printed_lines_splits_on_cr_and_lf() {
        let lines = printed_lines("a\r\nb\rc");
        assert_eq!(lines, vec!["a", "b", "c"]);
    }
}
