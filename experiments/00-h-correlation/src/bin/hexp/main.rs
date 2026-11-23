//! Logic experiments. Every number printed here comes from code in this
//! repository, on synthetic streams whose ground truth is known by
//! construction. Anything synthetic is labelled SYNTHETIC in the output and in
//! REPORT.md.

mod cause_exp;
mod common;
mod conf_exp;
mod ctx_exp;
mod dedup_exp;
mod order_exp;
mod pid_exp;
mod redact_exp;

/// A named step: command-line name plus the function that runs it. Named so the
/// dispatch table is readable and clippy stops calling it a complex type.
type Step = (&'static str, fn() -> common::R<()>);

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let steps: &[Step] = &[
        ("dedup", dedup_exp::run),
        ("reorder", order_exp::run),
        ("pidreuse", pid_exp::run),
        ("confidence", conf_exp::run),
        ("causality", cause_exp::run),
        ("context", ctx_exp::run),
        ("redact", redact_exp::run),
    ];
    let selected: Vec<&Step> = if cmd == "all" {
        steps.iter().collect()
    } else {
        let s: Vec<_> = steps.iter().filter(|(n, _)| *n == cmd).collect();
        if s.is_empty() {
            eprintln!("unknown subcommand {cmd}; known: all, dedup, reorder, pidreuse, confidence, causality, context, redact");
            std::process::exit(2);
        }
        s
    };
    for (name, f) in selected {
        if let Err(e) = f() {
            eprintln!("STEP {name} FAILED: {e}");
            std::process::exit(1);
        }
    }
}
