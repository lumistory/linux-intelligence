// Shell-hook process: fail open for the user's command, fail silent for telemetry.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;

use li_hook::{Adapter, QUEUE_CAP_DEFAULT, Queue, Sink, ingest};
use li_proto::SessionId;

fn main() -> ExitCode {
    // Always exit successfully; diagnostics are opt-in and go to stderr.
    match Options::from_env_and_args() {
        Ok(opts) => run(opts),
        Err(why) => note(&format!("li-hook: {why}; nothing sent")),
    }
    ExitCode::SUCCESS
}

fn run(opts: Options) {
    let session = match SessionId::new(opts.session.clone()) {
        Ok(s) => s,
        Err(e) => return note(&format!("li-hook: session id is unusable ({e:?})")),
    };
    let mut sink = match &opts.endpoint {
        Endpoint::None => Sink::dead(),
        Endpoint::Inherited => {
            let mut s = match Sink::inherited() {
                Ok(s) => s,
                Err(e) => {
                    return note(&format!(
                        "li-hook: inherited sink unavailable ({e}); staying silent"
                    ));
                }
            };
            if let Err(e) = s.probe(u64::from(std::process::id())) {
                return note(&format!("li-hook: sink probe failed ({e}); staying silent"));
            }
            s
        }
        Endpoint::Socket(path) => match Sink::connect(path) {
            Ok(s) => s,
            Err(e) => return note(&format!("li-hook: {e}; staying silent")),
        },
    };

    let stdin = std::io::stdin();
    let report = ingest(
        BufReader::new(stdin.lock()),
        &mut sink,
        Queue::new(opts.queue_cap),
        opts.adapter,
        &session,
    );
    note(&report.as_line(opts.adapter)); // stderr: stdout may be the sink
}

fn note(msg: &str) {
    if std::env::var_os("LI_HOOK_VERBOSE").is_some() {
        eprintln!("{msg}");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    Inherited,
    Socket(PathBuf),
    None,
}

#[derive(Debug)]
struct Options {
    adapter: Adapter,
    endpoint: Endpoint,
    session: String,
    queue_cap: usize,
}

// Environment values are separated so argument parsing remains deterministic.
#[derive(Debug, Default)]
struct Exported {
    adapter: Option<String>,
    socket: Option<String>,
    session: Option<String>,
}

impl Options {
    fn from_env_and_args() -> Result<Self, String> {
        let exported = Exported {
            adapter: std::env::var("LI_ADAPTER").ok(),
            // A socket path is never an implicit sink: only an explicit
            // diagnostic/test `--sink=socket` may make li-hook dial one.
            socket: None,
            session: std::env::var(li_hook::SESSION_ENV).ok(),
        };
        Self::parse(std::env::args().skip(1), exported)
    }

    // Arguments win over environment, so a launcher can pin a value.
    fn parse(args: impl Iterator<Item = String>, exported: Exported) -> Result<Self, String> {
        let mut adapter = exported.adapter.as_deref().and_then(Adapter::parse);
        let mut socket = exported.socket.map(PathBuf::from);
        let mut sink_mode: Option<String> = None;
        let mut session = exported.session;
        let mut queue_cap = QUEUE_CAP_DEFAULT;

        for arg in args {
            let (key, value) = arg
                .split_once('=')
                .ok_or_else(|| format!("argument `{arg}` is not `--key=value`"))?;
            match key {
                "--adapter" => {
                    adapter = Some(
                        Adapter::parse(value)
                            .ok_or_else(|| format!("unknown adapter `{value}`"))?,
                    );
                }
                "--socket" => socket = Some(PathBuf::from(value)),
                "--sink" => {
                    if !matches!(value, "inherited" | "socket" | "none") {
                        return Err(format!("unknown sink `{value}`"));
                    }
                    sink_mode = Some(value.to_string());
                }
                "--session" => session = Some(value.to_string()),
                "--queue-cap" => {
                    queue_cap = value
                        .parse()
                        .map_err(|_| format!("queue cap `{value}` is not a number"))?;
                }
                other => return Err(format!("unknown argument `{other}`")),
            }
        }

        let adapter = adapter.ok_or("no adapter: pass --adapter=bash|zsh|fish")?;
        let endpoint = match sink_mode.as_deref() {
            Some("none") => Endpoint::None,
            Some("inherited") => Endpoint::Inherited,
            Some("socket") => match socket {
                Some(p) => Endpoint::Socket(p),
                None => return Err("--sink=socket needs --socket or LI_SOCKET".into()),
            },
            // Production default is launcher inheritance, not client-side open.
            _ => Endpoint::Inherited,
        };
        let session = session.ok_or_else(|| format!("no session: set {}", li_hook::SESSION_ENV))?;
        Ok(Options {
            adapter,
            endpoint,
            session,
            queue_cap,
        })
    }
}
