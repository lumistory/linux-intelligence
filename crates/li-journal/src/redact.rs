//! Secret redaction, applied **before** the row is inserted.
//!
//! ADR 0003: "Sensitive-value redaction happens before insert; unredacted form is
//! never persisted by default." So this is not a scrubber over a stored payload —
//! by the time a byte reaches SQLite the secret is already gone, which also means
//! the WAL and any `-wal` sidecar never contain it either.
//!
//! The rule set is 0-H §7.6 **V2**, ported because it is the only generation
//! measured against both a secret corpus and an adversarial clean corpus:
//!
//! | generation | clean n | FP | secret n | missed |
//! |---|---|---|---|---|
//! | V1 (flags, env names, auth header) | 53 | 0 | 24 | 6 |
//! | V2 (+ URL userinfo, `key: value`)  | 53 | 0 | 24 | 0 |
//!
//! Two rules of that port are load-bearing and are easy to "improve" into
//! breakage, so they are restated:
//!
//! * **Digits-only values are never secrets.** `mysql -p3306`, `ssh -p 2222`,
//!   `export PORT=8080` survive because of this, and they are the largest
//!   false-positive class.
//! * **`*_URL` / `*_DSN` are not secret *names*.** A credential in a URL lives in
//!   its userinfo section, so masking the whole value would flag the common
//!   `postgres://localhost/app`. The `url-userinfo` rule catches the real shape.
//!
//! What this cannot do, measured in 0-H §7.5 and not fixable by another regex:
//! `./deploy s3cr3tvalue` is untouched. A positional argument is
//! indistinguishable from a filename.

use crate::clock;

/// What a masked value is replaced with. Not `***`: a fixed token is
/// distinguishable from a user's literal `***` in an audit.
pub const MASK: &str = "[REDACTED]";

/// Payload keys whose string values are treated as command text.
///
/// A closed list, deliberately: `payload` is an open vocabulary, and redacting
/// every string in it would mangle path/`cwd` values and any future field. Only
/// keys that carry typed shell text are inspected.
const TEXT_KEYS: &[&str] = &[
    "command",
    "argv",
    "cmdline",
    "script",
    "script_body",
    "invocation",
];

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

/// Outcome of one redaction pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redaction {
    pub text: String,
    /// Which rules fired, in first-hit order. Persisted in
    /// `events.redaction_rules` so a false positive can be audited later.
    pub rules: Vec<&'static str>,
    /// Total rule firings, including a rule firing twice. `rules.len()` cannot
    /// answer this, which is why it is a separate number.
    pub hits: usize,
    /// Linkable 64-bit digest of the first masked secret. Not reversible, not a
    /// KDF (0-H §7.6 / PHASE0-REPORT §5.6).
    pub secret_digest: Option<String>,
}

impl Redaction {
    pub fn is_clean(&self) -> bool {
        self.rules.is_empty()
    }

    /// `a,b,c` for the column, or `None` when nothing fired.
    pub fn rules_csv(&self) -> Option<String> {
        if self.rules.is_empty() {
            None
        } else {
            Some(self.rules.join(","))
        }
    }
}

#[derive(Default)]
struct Sink {
    rules: Vec<&'static str>,
    hits: usize,
    secret: Option<String>,
}

impl Sink {
    fn hit(&mut self, rule: &'static str, secret: &str) {
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
        self.hits += 1;
        if self.secret.is_none() {
            self.secret = Some(unquote(secret).to_string());
        }
    }
}

fn unquote(v: &str) -> &str {
    v.trim_matches(|c| c == '"' || c == '\'')
}

fn looks_like_secret_value(v: &str) -> bool {
    let v = unquote(v.trim());
    if v.is_empty() || v.starts_with('$') || v.starts_with('<') {
        return false;
    }
    // The defence that the zero-false-positive result depends on: a port is not
    // a password.
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

/// Redacts one piece of command text.
///
/// Rules run in a fixed order — tokens, then auth headers, then URL userinfo,
/// then `key: value` — because each one can only make the text *less* secret than
/// the previous pass left it, so running them twice is stable and running them
/// in the other order would let a token pass re-split an auth header.
pub fn redact(text: &str) -> Redaction {
    let mut sink = Sink::default();
    let mut out = redact_tokens(text, &mut sink);
    out = redact_auth_header(&out, &mut sink);
    out = redact_url_userinfo(&out, &mut sink);
    out = redact_key_colon_value(&out, &mut sink);
    Redaction {
        text: out,
        rules: sink.rules,
        hits: sink.hits,
        secret_digest: sink
            .secret
            .as_deref()
            .map(|s| clock::fnv1a_hex(s.as_bytes())),
    }
}

/// Redacts the known command-text keys of a JSON payload object.
///
/// Returns whether anything changed, so the caller can skip the audit columns for
/// the overwhelming majority of events (file modifications, process exits) that
/// carry no text at all.
pub fn redact_payload(
    payload: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<Redaction> {
    let mut combined: Option<Redaction> = None;
    for key in TEXT_KEYS {
        let Some(value) = payload.get_mut(*key) else {
            continue;
        };
        match value {
            serde_json::Value::String(s) => {
                let r = redact(s);
                if !r.is_clean() {
                    *s = r.text.clone();
                    merge(&mut combined, r);
                }
            }
            // `argv` is an array of words, and redacting it element by element
            // cannot see a secret that is split across two elements
            // (`["--password", "hunter2"]`), which is exactly how argv carries one.
            // So the two cross-element rules are applied on the array itself, then
            // each element gets the single-token rules. Rebuilding the array (join,
            // redact, split) is rejected: an element containing a space would be
            // split in two and the shape a consumer relies on would change.
            serde_json::Value::Array(items) => {
                let mut mask_next = false;
                for item in items.iter_mut() {
                    if let serde_json::Value::String(s) = item {
                        if mask_next && !s.starts_with('-') && looks_like_secret_value(s) {
                            sink_hit(&mut combined, "flag-value-separate", s);
                            *s = MASK.to_string();
                            mask_next = false;
                            continue;
                        }
                        mask_next = is_secret_flag(s);
                        let r = redact(s);
                        if !r.is_clean() {
                            *s = r.text.clone();
                            merge(&mut combined, r);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    combined
}

fn merge(into: &mut Option<Redaction>, r: Redaction) {
    match into {
        None => *into = Some(r),
        Some(acc) => {
            for rule in r.rules {
                if !acc.rules.contains(&rule) {
                    acc.rules.push(rule);
                }
            }
            acc.hits += r.hits;
            if acc.secret_digest.is_none() {
                acc.secret_digest = r.secret_digest;
            }
        }
    }
}

/// Records a hit that was detected across array elements rather than inside one
/// piece of text, where there is no `Sink` in scope.
fn sink_hit(into: &mut Option<Redaction>, rule: &'static str, secret: &str) {
    let acc = into.get_or_insert_with(Redaction::default);
    if !acc.rules.contains(&rule) {
        acc.rules.push(rule);
    }
    acc.hits += 1;
    if acc.secret_digest.is_none() {
        acc.secret_digest = Some(clock::fnv1a_hex(secret.as_bytes()));
    }
}

fn redact_tokens(cmd: &str, sink: &mut Sink) -> String {
    let tokens: Vec<&str> = cmd.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = tokens[i];

        // `-pSECRET` glued (mysql client). `-p` is also `--port`'s short form,
        // which is why a digits-only tail is left alone.
        if let Some(rest) = tok.strip_prefix("-p")
            && !rest.is_empty()
            && !rest.starts_with('-')
            && !rest.chars().all(|c| c.is_ascii_digit())
            && rest.chars().count() >= 4
        {
            sink.hit("flag-short-p-glued", rest);
            out.push(format!("-p{MASK}"));
            i += 1;
            continue;
        }

        // `name=value`
        if let Some((name, value)) = tok.split_once('=')
            && !name.is_empty()
        {
            let flag = is_secret_flag(name);
            let lower = name.to_ascii_lowercase();
            let env_like = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && !name.contains('/')
                && (env_name_matches(name)
                    || lower.contains("pass")
                    || lower.contains("secret")
                    || lower.contains("token"));
            if (flag || env_like) && looks_like_secret_value(value) {
                sink.hit(
                    if flag {
                        "flag-value-assign"
                    } else {
                        "env-assign"
                    },
                    value,
                );
                out.push(format!("{name}={MASK}"));
                i += 1;
                continue;
            }
        }

        // `--flag VALUE`
        if is_secret_flag(tok)
            && i + 1 < tokens.len()
            && looks_like_secret_value(tokens[i + 1])
            && !tokens[i + 1].starts_with('-')
        {
            sink.hit("flag-value-separate", tokens[i + 1]);
            out.push(tok.to_string());
            out.push(MASK.to_string());
            i += 2;
            continue;
        }

        out.push(tok.to_string());
        i += 1;
    }
    out.join(" ")
}

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
        let scheme = trimmed
            .split([' ', '\t'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(scheme.as_str(), "bearer" | "basic" | "token" | "digest") {
            out.push_str(&text[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        // Skip the whitespace between the scheme and the credential; without
        // this, `Bearer xxx` yields a zero-length secret and nothing is masked.
        let after_scheme = value_start + lead + scheme.len();
        let gap: usize = text[after_scheme..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(char::len_utf8)
            .sum();
        let secret_begin = after_scheme + gap;
        let secret_len: usize = text[secret_begin..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .map(char::len_utf8)
            .sum();
        if secret_len < 6 {
            out.push_str(&text[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        let secret = &text[secret_begin..secret_begin + secret_len];
        sink.hit("auth-header", secret);
        // Keep the single space the scheme and the mask were separated by.
        out.push_str(&text[cursor..after_scheme]);
        out.push(' ');
        out.push_str(MASK);
        cursor = secret_begin + secret_len;
    }
    out.push_str(&text[cursor..]);
    out
}

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
    if any { out } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_0_h_cases_are_masked_verbatim() {
        assert_eq!(
            redact("mysql -u root -pS3cr3tPw -h db").text,
            "mysql -u root -p[REDACTED] -h db"
        );
        assert_eq!(
            redact("export API_KEY=abcdef123456").text,
            "export API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact("curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x").text,
            "curl -H \"Authorization: Bearer [REDACTED]\" https://x"
        );
    }

    #[test]
    fn url_userinfo_is_masked_but_a_bare_url_is_not() {
        assert_eq!(
            redact("git clone https://bob:sup3rs3cret@example.com/r.git").text,
            "git clone https://bob:[REDACTED]@example.com/r.git"
        );
        // The false positive that made `*_URL` leave the secret-name list.
        let plain = "export DATABASE_URL=postgres://localhost/app";
        assert_eq!(redact(plain).text, plain);
        assert!(redact(plain).is_clean());
    }

    #[test]
    fn ports_variables_and_prose_are_not_secrets() {
        for clean in [
            "mysql -p3306 -h db",
            "ssh -p 2222 host",
            "export PORT=8080",
            // These two are the cases that actually exercise the digits-only
            // defence: the *name* is a secret name, only the value saves it.
            // Without them, deleting `looks_like_secret_value`'s digit test is
            // invisible to this suite — measured by mutating the rule and watching
            // the suite stay green.
            "export API_KEY=1234567890",
            "mysql --password 3306 -h db",
            "export PATH=$PATH:/usr/local/bin",
            "export API_KEY=$API_KEY",
            r#"grep -r 'Authorization' src/"#,
            "rg --password-file .pgpass",
            "sed -i 's/password/pass/g' config.rs",
            "curl -sS https://api.example.com/v1/health",
            "docker run -p 8080:80 nginx",
        ] {
            let r = redact(clean);
            assert!(r.is_clean(), "false positive on `{clean}`: {:?}", r.rules);
            assert_eq!(r.text, clean);
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        for cmd in [
            "mysql -u root -pS3cr3tPw -h db",
            "export API_KEY=abcdef123456",
            "git clone https://bob:sup3rs3cret@example.com/r.git",
        ] {
            let once = redact(cmd).text;
            let twice = redact(&once).text;
            assert_eq!(once, twice, "second pass changed the text");
        }
    }

    #[test]
    fn digest_is_stable_and_not_reversible() {
        let a = redact("mysql -pS3cr3tPw");
        let b = redact("mysql -pS3cr3tPw");
        assert_eq!(a.secret_digest, b.secret_digest);
        let d = a.secret_digest.unwrap();
        assert_eq!(d.len(), 16);
        assert!(!d.contains("S3cr3tPw"));
    }

    #[test]
    fn payload_redaction_keeps_shape_and_skips_unrelated_keys() {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "command".into(),
            serde_json::json!("export API_KEY=abcdef123456"),
        );
        payload.insert("path".into(), serde_json::json!("/home/u/app.conf"));
        payload.insert(
            "argv".into(),
            serde_json::json!(["docker", "login", "--password", "hunter2hunter2"]),
        );
        let r = redact_payload(&mut payload).expect("rules must have fired");
        assert!(!r.is_clean());
        assert_eq!(payload["command"], "export API_KEY=[REDACTED]");
        // untouched keys survive verbatim
        assert_eq!(payload["path"], "/home/u/app.conf");
        let argv = payload["argv"].as_array().unwrap();
        assert_eq!(argv.len(), 4, "the array shape is preserved");
        assert_eq!(argv[3], "[REDACTED]");
    }

    #[test]
    fn payload_without_text_is_untouched_and_costs_nothing() {
        let mut payload = serde_json::Map::new();
        payload.insert("path".into(), serde_json::json!("/tmp/x"));
        assert!(redact_payload(&mut payload).is_none());
    }
}
