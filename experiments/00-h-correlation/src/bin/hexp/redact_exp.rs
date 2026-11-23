//! Experiment 7: pre-persistence secret redaction, with the false-positive cost
//! measured on a corpus of realistic commands that must survive untouched.

use corr0h::redact::{redact, Generation};

use crate::common::R;

/// Commands that must survive untouched, with the reason each is a plausible
/// false positive.
const CLEAN_CORPUS: &[(&str, &str)] = &[
    ("ls -l /tmp", "plain"),
    ("git status", "plain"),
    ("cargo build --release", "plain"),
    ("make -j8", "plain"),
    ("python3 train.py --epochs 10", "numbers"),
    ("docker run -p 8080:80 nginx", "`-p 8080:80` is a port mapping"),
    ("mysql -p3306 -h db", "`-p3306` is a port"),
    ("ssh -p 2222 host", "`-p 2222` is a port"),
    ("kubectl port-forward svc 8080:8080", "numbers"),
    ("export PORT=8080", "digits-only value"),
    ("export TIMEOUT=30", "digits-only value"),
    ("export PATH=$PATH:/usr/local/bin", "variable reference"),
    ("export API_KEY=$API_KEY", "variable reference, not a literal secret"),
    ("apt list --installed", "plain"),
    ("curl -sS https://api.example.com/v1/health", "no credentials"),
    ("curl -H 'Accept: application/json' https://x", "non-auth header"),
    ("curl -H 'Content-Type: application/json' https://x", "non-auth header"),
    ("grep -r 'Authorization' src/", "the word, no value"),
    ("rg --password-file .pgpass", "a filename, not a secret"),
    ("tar -czf backup.tar.gz docs", "plain"),
    ("find . -name '*.rs' -exec wc -l {} +", "braces"),
    ("sed -i 's/password/pass/g' config.rs", "text, not a credential"),
    ("echo 'hello world'", "plain"),
    ("vim notes.md", "plain"),
    ("git commit -m 'fix token parser'", "the word token in prose"),
    ("git log --oneline -20", "numbers"),
    ("systemctl status docker", "plain"),
    ("journalctl -u docker -n 50", "numbers"),
    ("cargo test -- --nocapture", "flags"),
    ("npm run build -- --mode production", "plain"),
    ("rsync -avz ./src/ host:/srv/app/", "path with a colon"),
    ("psql -h localhost -U app -d appdb", "user and db, no password"),
    ("redis-cli -a 127.0.0.1:6379 ping", "looks like a flag but is an address"),
    ("openssl rand -hex 32 > /tmp/k", "generates a secret, stores none in argv"),
    ("chmod 600 .ssh/id_ed25519", "path"),
    ("diff -u a.txt b.txt", "plain"),
    ("ffmpeg -i in.mp4 -c:v libx264 out.mp4", "plain"),
    ("du -sh /var/log/*", "glob"),
    ("htop", "plain"),
    ("git push origin main", "plain"),
    ("python -m http.server 8000", "numbers"),
    ("export DATABASE_URL=postgres://localhost/app", "URL without userinfo"),
    ("go test -run TestAuth ./...", "the word Auth in a test name"),
    ("aws s3 ls s3://bucket/key", "s3 url, no userinfo"),
    ("cat .config/app/settings.toml", "path"),
    ("gpg --list-secret-keys", "the word secret, no value"),
    ("ssh-keygen -t ed25519 -C 'me@example.com'", "plain"),
    ("podman build -t app:1.0 .", "tag with a colon"),
    ("date +%Y-%m-%d", "format string"),
    ("echo $PASSWORD", "shell variable, not a literal"),
    ("mysql --host=db --port=3306 -u app", "flag=value, non-secret"),
    ("git config --get user.email", "plain"),
    ("ls ~/.ssh", "directory name"),
];

/// Commands that must be masked, with the secret that must disappear.
const SECRET_CORPUS: &[(&str, &str, &str)] = &[
    ("mysql -u root -pS3cr3tPw -h db", "S3cr3tPw", "glued -p"),
    ("mysql -u root --password=S3cr3tPw", "S3cr3tPw", "flag=value"),
    ("mysql -u root --password S3cr3tPw", "S3cr3tPw", "flag value"),
    ("mysqldump -pS3cr3tPw appdb > d.sql", "S3cr3tPw", "glued -p"),
    ("export API_KEY=abcdef123456", "abcdef123456", "env assign"),
    ("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI", "wJalrXUtnFEMI", "env assign"),
    ("export GITHUB_TOKEN=ghp_abcdefghijkl", "ghp_abcdefghijkl", "env assign"),
    ("export DB_PASSWORD=hunter2hunter2", "hunter2hunter2", "env assign"),
    ("export CLIENT_SECRET=s3cr3t-value", "s3cr3t-value", "env assign"),
    ("curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x", "eyJhbGciOiJIUzI1NiJ9.abc", "auth header"),
    ("curl -H 'Authorization: Basic dXNlcjpwYXNz' https://x", "dXNlcjpwYXNz", "auth header"),
    ("wget --header='Authorization: Token abcdef123456' https://x", "abcdef123456", "auth header"),
    ("app --token hunter2hunter2", "hunter2hunter2", "flag value"),
    ("app --api-key=abc123def456", "abc123def456", "flag=value"),
    ("deploy --secret s3cr3tvalue", "s3cr3tvalue", "flag value"),
    ("git clone https://bob:sup3rs3cret@example.com/r.git", "sup3rs3cret", "url userinfo"),
    ("psql postgres://app:dbpass123@localhost/app", "dbpass123", "url userinfo"),
    ("password: hunter2hunter2", "hunter2hunter2", "key: value"),
    ("api_token: abcdef123456", "abcdef123456", "key: value"),
    ("export MY_SERVICE_TOKEN=tok_abcdef123", "tok_abcdef123", "env suffix _TOKEN"),
    ("export SOME_APP_PASSWORD=pw12345678", "pw12345678", "env suffix _PASSWORD"),
    ("vault write --token=hvs.abcdefghij", "hvs.abcdefghij", "flag=value"),
    ("export DATABASE_URL=postgres://app:hunter2pw@db.local/app", "hunter2pw", "url userinfo"),
    ("export PG_DSN=postgresql://u:pw123456@10.0.0.5/app", "pw123456", "url userinfo"),
];

pub fn run() -> R<()> {
    println!();
    println!("## 7. Secret redaction before persistence");
    println!();
    println!("Two rule generations on the same corpus. V1 = flag/env/auth-header shapes.");
    println!("V2 = V1 + URL userinfo + `key: value` heuristics.");
    println!();
    println!("| generation | clean n | false positives | FP rate | secret n | missed | miss rate |");
    println!("|---|---|---|---|---|---|---|");

    let mut fp_v2: Vec<(&str, &str)> = Vec::new();
    let mut miss_v2: Vec<&str> = Vec::new();
    let mut fp_v1 = 0;
    let mut miss_v1 = 0;
    for gen in [Generation::V1, Generation::V2] {
        let mut fp = 0usize;
        for (cmd, _why) in CLEAN_CORPUS {
            let r = redact(cmd, gen);
            if !r.is_clean() {
                fp += 1;
                if gen == Generation::V2 {
                    fp_v2.push((cmd, r.rules.first().copied().unwrap_or("?")));
                }
            }
        }
        let mut missed = 0usize;
        for (cmd, secret, _rule) in SECRET_CORPUS {
            let r = redact(cmd, gen);
            if r.text.contains(secret) {
                missed += 1;
                if gen == Generation::V2 {
                    miss_v2.push(cmd);
                }
            }
        }
        if gen == Generation::V1 {
            fp_v1 = fp;
            miss_v1 = missed;
        }
        println!("| {} | {} | {} | {:.4} | {} | {} | {:.4} |",
            if gen == Generation::V1 { "V1" } else { "V2" },
            CLEAN_CORPUS.len(), fp, fp as f64 / CLEAN_CORPUS.len() as f64,
            SECRET_CORPUS.len(), missed, missed as f64 / SECRET_CORPUS.len() as f64);
    }

    println!();
    println!("### 7.1 V2 false positives, with the rule that fired");
    println!();
    if fp_v2.is_empty() {
        println!("none on this corpus.");
    } else {
        println!("| command | rule |");
        println!("|---|---|");
        for (c, r) in &fp_v2 {
            println!("| `{c}` | {r} |");
        }
    }

    println!();
    println!("### 7.2 V2 misses");
    println!();
    if miss_v2.is_empty() {
        println!("none on this corpus.");
    } else {
        for c in &miss_v2 {
            println!("- `{c}`");
        }
    }

    println!();
    println!("### 7.3 The three required cases, verbatim");
    println!();
    for cmd in [
        "mysql -u root -pS3cr3tPw -h db",
        "export API_KEY=abcdef123456",
        "curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x",
    ] {
        let r = redact(cmd, Generation::V2);
        println!("- in : `{cmd}`");
        println!("  out: `{}`", r.text);
        println!("  rules {:?}, digest {}",
            r.rules, r.secret_digest.clone().unwrap_or_else(|| "-".into()));
        match cmd.split(' ').nth(0) {
            _ => {}
        }
    }

    println!();
    println!("### 7.4 Per-rule coverage");
    println!();
    let mut rule_hits: std::collections::BTreeMap<&str, usize> = Default::default();
    for (cmd, _, _) in SECRET_CORPUS {
        for r in redact(cmd, Generation::V2).rules {
            *rule_hits.entry(r).or_insert(0) += 1;
        }
    }
    println!("| rule | secrets caught by it |");
    println!("|---|---|");
    for (r, n) in &rule_hits {
        println!("| {r} | {n} |");
    }

    println!();
    println!("### 7.5 Residual risk: a secret as a bare positional argument");
    println!();
    let r = redact("./deploy s3cr3tvalue", Generation::V2);
    println!("`./deploy s3cr3tvalue` -> `{}` (rules {:?})", r.text, r.rules);
    println!("A positional value is indistinguishable from a filename or a hostname. Neither");
    println!("generation masks it. This is a shaping limit, not a tuning miss: see §6 of the");
    println!("report. The mitigation is to not capture such a command line, or to let the");
    println!("user mark an argument position as secret.");

    println!();
    println!("### 7.6 Digest linkability");
    println!();
    let a = redact("mysql -pS3cr3tPw", Generation::V2);
    let b = redact("mysql -pS3cr3tPw", Generation::V2);
    println!("same secret twice -> digest {} / {} equal={}",
        a.secret_digest.as_deref().unwrap_or("-"),
        b.secret_digest.as_deref().unwrap_or("-"),
        a.secret_digest == b.secret_digest);
    println!("The digest is 64-bit FNV-1a: linkable, not reversible, and NOT a key-derivation");
    println!("function. It answers \"the same credential was used again\", nothing more.");

    println!();
    println!("Summary: V1 FP {} / miss {}; V2 FP {} / miss {} (of {} clean and {} secret commands).",
        fp_v1, miss_v1, fp_v2.len(), miss_v2.len(), CLEAN_CORPUS.len(), SECRET_CORPUS.len());

    // A miss on the three required cases is a hard failure, not a statistic.
    for cmd in [
        "mysql -u root -pS3cr3tPw -h db",
        "export API_KEY=abcdef123456",
        "curl -H \"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc\" https://x",
    ] {
        let out = redact(cmd, Generation::V2);
        for token in ["S3cr3tPw", "abcdef123456", "eyJhbGciOiJIUzI1NiJ9.abc"] {
            if cmd.contains(token) && out.text.contains(token) {
                return Err(format!("required case not redacted: {cmd}").into());
            }
        }
    }
    Ok(())
}
