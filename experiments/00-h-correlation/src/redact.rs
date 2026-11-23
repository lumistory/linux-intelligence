//! Pre-persistence secret redaction for command text and argv.
//!
//! Scope: this runs *inside the daemon, before the row is written*. It is not a
//! substitute for not capturing a secret, it is the last line of defence, and
//! 0-A/0-B established that shell hooks deliver the raw command line including
//! any inline credential.
//!
//! Two generations are implemented so the false-positive cost of the stricter
//! one is measured rather than guessed.

use crate::util::fnv_hex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// V1: token-shape rules only (secret flags, secret env names, auth header).
    V1,
    /// V2: V1 plus URL userinfo and generic `key: value` heuristics, at the cost
    /// of a higher false-positive rate.
    V2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// Text safe to persist.
    pub text: String,
    /// Which rules fired, for the audit trail.
    pub rules: Vec<&'static str>,
    /// Stable hash of the first secret seen so repeated use is linkable without
    /// storing anything reversible.
    pub secret_digest: Option<String>,
}

impl Redaction {
    pub fn is_clean(&self) -> bool {
        self.rules.is_empty()
    }
}

pub const MASK: &str = "[REDACTED]";

/// Options that take a secret value, with or without `=`.
const SECRET_FLAGS: &[&str] = &[
    "--password",
    "--passwd",
    "--pass",
    "--token",
    "--api-key",
    "--apikey",
    "--secret",
    "--client-secret",
    "--credentials",
];

/// Environment variable names whose value is a secret.
const SECRET_ENV: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "API_TOKEN",
    "AUTH_TOKEN",
    "ACCESS_TOKEN",
    "SECRET",
    "SECRET_KEY",
    "CLIENT_SECRET",
    "PASSWORD",
    "PASSWD",
    "DB_PASSWORD",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "PRIVATE_KEY",
];

// `*_URL` / `*_DSN` names are deliberately NOT in this list. A connection URL
// carries a password only in its userinfo section, so masking the whole value
// is a false positive on the common `postgres://localhost/app` form. Measured:
// see the V2 `url-userinfo` rule, which catches the credentialed form instead.

fn unquote(v: &str) -> &str {
    v.trim_matches(|c| c == '"' || c == '\'')
}

/// A value worth masking: not empty, not a variable reference, not a pure
/// number. Pure numbers are the single largest false-positive source, because
/// `-p3306` is a port and `API_KEY=1` is a test stub.
fn looks_like_secret_value(v: &str) -> bool {
    let v = unquote(v.trim());
    if v.is_empty() || v.starts_with('$') || v.starts_with('<') {
        return false;
    }
    if v.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    v.chars().count() >= 4
}

fn env_name_matches(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    SECRET_ENV.iter().any(|k| up == *k)
        || up.ends_with("_KEY")
        || up.ends_with("_TOKEN")
        || up.ends_with("_SECRET")
        || up.ends_with("_PASSWORD")
        || up.ends_with("_PASSWD")
}

fn is_secret_flag(tok: &str) -> bool {
    let l = tok.to_ascii_lowercase();
    SECRET_FLAGS.iter().any(|f| l == *f)
}

#[derive(Default)]
struct Sink {
    rules: Vec<&'static str>,
    secret: Option<String>,
}

impl Sink {
    fn hit(&mut self, rule: &'static str, secret: &str) {
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
        if self.secret.is_none() {
            self.secret = Some(unquote(secret).to_string());
        }
    }
}

/// Rule 1 — token pass: `-pSECRET`, `--flag=VALUE`, `--flag VALUE`, `NAME=VALUE`.
fn redact_tokens(cmd: &str, sink: &mut Sink, generation: Generation) -> String {
    let tokens: Vec<String> = cmd.split(' ').map(str::to_string).collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = &tokens[i];
        let is_first_token = i == 0;

        // `-pSECRET` glued (mysql client). `-p` is also `--port`'s short form.
        if let Some(rest) = tok.strip_prefix("-p") {
            if !rest.is_empty()
                && !rest.starts_with('-')
                && !rest.chars().all(|c| c.is_ascii_digit())
                && rest.chars().count() >= 4
            {
                sink.hit("flag-short-p-glued", rest);
                out.push(format!("-p{MASK}"));
                i += 1;
                continue;
            }
        }

        // `name=value`
        if let Some((name, value)) = tok.split_once('=') {
            if !name.is_empty() {
                let flag = is_secret_flag(name);
                let env_like = name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && !name.contains('/')
                    && (env_name_matches(name)
                        || (generation == Generation::V2
                            && (name.to_ascii_lowercase().contains("pass")
                                || name.to_ascii_lowercase().contains("secret")
                                || name.to_ascii_lowercase().contains("token"))));
                if (flag || env_like) && looks_like_secret_value(value) {
                    sink.hit(
                        if flag { "flag-value-assign" } else { "env-assign" },
                        value,
                    );
                    out.push(format!("{name}={MASK}"));
                    i += 1;
                    continue;
                }
            }
        }

        // `--flag VALUE`
        if is_secret_flag(tok) && i + 1 < tokens.len() {
            let value = &tokens[i + 1];
            if looks_like_secret_value(value) && !value.starts_with('-') {
                sink.hit("flag-value-separate", value);
                out.push(tok.clone());
                out.push(MASK.to_string());
                i += 2;
                continue;
            }
        }

        let _ = is_first_token;
        out.push(tok.clone());
        i += 1;
    }
    out.join(" ")
}

/// Rule 2 — `Authorization: Bearer xxx` (and basic/token/digest).
fn redact_auth_header(text: &str, sink: &mut Sink) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("authorization") {
        let start = cursor + rel;
        let after = start + "authorization".len();
        let Some(colon_rel) = text[after..].find(':') else {
            break;
        };
        let value_start = after + colon_rel + 1;
        let value = &text[value_start..];
        let lead = value.len() - value.trim_start().len();
        let trimmed = value.trim_start();
        let scheme: String = trimmed
            .split([' ', '\t'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(scheme.as_str(), "bearer" | "basic" | "token" | "digest") {
            out.push_str(&text[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        let scheme_len = scheme.len();
        let secret_begin = value_start + lead + scheme_len;
        let secret: String = text[secret_begin..]
            .chars()
            .skip_while(|c| *c == ' ' || *c == '\t')
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .collect();
        if secret.chars().count() < 6 {
            out.push_str(&text[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        sink.hit("auth-header", &secret);
        out.push_str(&text[cursor..secret_begin]);
        out.push(' ');
        out.push_str(MASK);
        cursor = secret_begin
            + text[secret_begin..]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(char::len_utf8)
                .sum::<usize>()
            + secret.len();
    }
    out.push_str(&text[cursor..]);
    out
}

/// Rule 3 (V2 only) — `scheme://user:password@host`.
fn redact_url_userinfo(text: &str, sink: &mut Sink) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find("://") {
        let schemesep = cursor + rel;
        let auth_start = schemesep + 3;
        let end = text[auth_start..]
            .find(['/', ' ', '"', '\'', '\n'])
            .map(|e| auth_start + e)
            .unwrap_or(text.len());
        let authority = &text[auth_start..end];
        let Some(at) = authority.rfind('@') else {
            out.push_str(&text[cursor..end]);
            cursor = end;
            continue;
        };
        let userinfo = &authority[..at];
        let Some(colon) = userinfo.find(':') else {
            out.push_str(&text[cursor..end]);
            cursor = end;
            continue;
        };
        let secret = &userinfo[colon + 1..];
        if secret.chars().count() < 4 {
            out.push_str(&text[cursor..end]);
            cursor = end;
            continue;
        }
        sink.hit("url-userinfo", secret);
        out.push_str(&text[cursor..auth_start]);
        out.push_str(&userinfo[..colon]);
        out.push(':');
        out.push_str(MASK);
        out.push_str(&authority[at..]);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Rule 4 (V2 only) — `password: xxx` style lines in a pasted snippet.
fn redact_key_colon_value(text: &str, sink: &mut Sink) -> String {
    let mut out = String::new();
    let mut any = false;
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if let Some((k, v)) = line.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let suspicious = kl.ends_with("password")
                || kl.ends_with("passwd")
                || kl.ends_with("token")
                || kl.ends_with("secret")
                || kl.ends_with("api_key")
                || kl == "authorization";
            if suspicious && looks_like_secret_value(v) {
                sink.hit("key-colon-value", v);
                out.push_str(k);
                out.push_str(": ");
                out.push_str(MASK);
                any = true;
                continue;
            }
        }
        out.push_str(line);
    }
    if any {
        out
    } else {
        text.to_string()
    }
}

pub fn redact(cmd: &str, generation: Generation) -> Redaction {
    let mut sink = Sink::default();
    let mut text = redact_tokens(cmd, &mut sink, generation);
    text = redact_auth_header(&text, &mut sink);
    if generation == Generation::V2 {
        text = redact_url_userinfo(&text, &mut sink);
        text = redact_key_colon_value(&text, &mut sink);
    }
    Redaction {
        text,
        rules: sink.rules,
        secret_digest: sink.secret.as_deref().map(fnv_hex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_glued_password() {
        let r = redact("mysql -u root -pS3cr3tPw -h db", Generation::V1);
        assert!(!r.text.contains("S3cr3tPw"), "{}", r.text);
        assert!(r.rules.contains(&"flag-short-p-glued"));
        assert_eq!(r.text, "mysql -u root -p[REDACTED] -h db");
    }

    #[test]
    fn mysql_port_is_not_a_password() {
        let r = redact("mysql -p3306 -h db", Generation::V1);
        assert_eq!(r.text, "mysql -p3306 -h db");
        assert!(r.is_clean());
    }

    #[test]
    fn export_api_key() {
        let r = redact("export API_KEY=abcdef123456", Generation::V1);
        assert!(!r.text.contains("abcdef123456"), "{}", r.text);
        assert!(r.rules.contains(&"env-assign"));
        assert_eq!(r.text, "export API_KEY=[REDACTED]");
    }

    #[test]
    fn export_port_is_left_alone() {
        let r = redact("export PORT=8080", Generation::V1);
        assert_eq!(r.text, "export PORT=8080");
    }

    #[test]
    fn bearer_header() {
        let r = redact(
            "curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x",
            Generation::V1,
        );
        assert!(!r.text.contains("eyJhbGciOiJIUzI1NiJ9.abc"), "{}", r.text);
        assert!(r.rules.contains(&"auth-header"));
    }

    #[test]
    fn plain_curl_is_untouched() {
        let cmd = "curl -sS https://api.example.com/v1/health";
        let r = redact(cmd, Generation::V2);
        assert_eq!(r.text, cmd);
    }

    #[test]
    fn long_flag_separate_value() {
        let r = redact("app --token hunter2hunter2 --verbose", Generation::V1);
        assert!(!r.text.contains("hunter2hunter2"), "{}", r.text);
    }

    #[test]
    fn url_userinfo_is_v2_only() {
        let cmd = "git clone https://bob:sup3rs3cret@example.com/r.git";
        let v1 = redact(cmd, Generation::V1);
        let v2 = redact(cmd, Generation::V2);
        assert_eq!(v1.text, cmd, "V1 must not implement url-userinfo");
        assert!(!v2.text.contains("sup3rs3cret"), "{}", v2.text);
        assert_eq!(
            v2.text,
            "git clone https://bob:[REDACTED]@example.com/r.git"
        );
    }

    #[test]
    fn database_url_without_userinfo_is_untouched() {
        let cmd = "export DATABASE_URL=postgres://localhost/app";
        for g in [Generation::V1, Generation::V2] {
            assert_eq!(redact(cmd, g).text, cmd);
        }
    }

    #[test]
    fn database_url_with_userinfo_is_caught_by_v2_only() {
        let cmd = "export DATABASE_URL=postgres://app:hunter2pw@db.local/app";
        let v1 = redact(cmd, Generation::V1);
        let v2 = redact(cmd, Generation::V2);
        assert!(v1.text.contains("hunter2pw"), "V1 is expected to miss this: {}", v1.text);
        assert!(!v2.text.contains("hunter2pw"), "{}", v2.text);
        assert!(v2.rules.contains(&"url-userinfo"));
    }

    #[test]
    fn digest_is_stable_and_not_reversible() {
        let a = redact("mysql -pS3cr3tPw", Generation::V1);
        let b = redact("mysql -pS3cr3tPw", Generation::V1);
        assert_eq!(a.secret_digest, b.secret_digest);
        let d = a.secret_digest.unwrap();
        assert_eq!(d.len(), 16);
        assert!(!d.contains("S3cr3tPw"));
    }
}
