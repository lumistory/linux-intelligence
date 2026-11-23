//! Filesystem observer probe: raw inotify event sequences, recursion, overflow,
//! permissions, and cost.
//!
//! Usage:
//!   fs_observer watch     --root DIR [--recursive] [--seconds N] [--quiet]
//!   fs_observer overflow  --root DIR --files N [--buffer BYTES] [--slow-ms M]
//!   fs_observer cost      --root DIR --files N
//!   fs_observer watch-count --root DIR

use li_observer_probe::{ino, mean, percentile, Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn arg<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn num<T: std::str::FromStr>(args: &[String], key: &str, default: T) -> T {
    arg(args, key)
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("FATAL: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    match argv.get(1).map(|s| s.as_str()).unwrap_or("help") {
        "watch" => cmd_watch(&argv),
        "overflow" => cmd_overflow(&argv),
        "cost" => cmd_cost(&argv),
        "watch-count" => cmd_watch_count(&argv),
        other => {
            eprintln!("unknown subcommand {other}");
            Err(Error::Usage("bad subcommand".into()))
        }
    }
}

fn mono_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let e = EPOCH.get_or_init(Instant::now);
    e.elapsed().as_nanos() as u64
}

fn rusage_self() -> (f64, f64, i64) {
    use nix::sys::resource::{getrusage, UsageWho};
    match getrusage(UsageWho::RUSAGE_SELF) {
        Ok(u) => {
            let ut = u.user_time();
            let st = u.system_time();
            (
                ut.tv_sec() as f64 + ut.tv_usec() as f64 / 1e6,
                st.tv_sec() as f64 + st.tv_usec() as f64 / 1e6,
                u.max_rss() as i64,
            )
        }
        Err(_) => (0.0, 0.0, -1),
    }
}

/// Recursively add a watch to `root` and every existing subdirectory.
/// Returns wd -> path so events can be resolved back to a directory.
fn add_tree(inst: &ino::Inotify, root: &Path, mask: u32) -> Result<HashMap<i32, PathBuf>> {
    let mut map = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        match inst.add_watch(&dir, mask) {
            Ok(wd) => {
                map.insert(wd, dir.clone());
            }
            Err(e) => {
                eprintln!("add_watch {dir:?} failed: {e}");
            }
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                }
            }
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// watch: print every raw event
// ---------------------------------------------------------------------------

/// `watch --root DIR [--recursive] [--seconds N] [--auto-add]`
///
/// `--auto-add` makes the observer add a watch when it sees a new directory,
/// which is exactly the manual work inotify requires for recursion. Running
/// with and without it measures the events lost in the gap between mkdir and
/// add_watch.
fn cmd_watch(argv: &[String]) -> Result<()> {
    let root = PathBuf::from(arg(argv, "--root").ok_or_else(|| {
        Error::Usage("watch requires --root".into())
    })?);
    let recursive = argv.iter().any(|a| a == "--recursive");
    let auto_add = argv.iter().any(|a| a == "--auto-add");
    let seconds: f64 = num(argv, "--seconds", 3.0);
    let mask = ino::default_mask();

    let inst = ino::Inotify::new(true)?;
    let mut wdmap: HashMap<i32, PathBuf> = if recursive {
        add_tree(&inst, &root, mask)?
    } else {
        let mut m = HashMap::new();
        m.insert(inst.add_watch(&root, mask)?, root.clone());
        m
    };
    println!("# watching {} dir(s) under {}", wdmap.len(), root.display());
    println!("# mask=0x{mask:x} recursive={recursive} auto_add={auto_add}");
    println!("# monots_ns wd dir mask_raw mask_names cookie isdir name");

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    while Instant::now() < deadline {
        if !inst.wait(50)? {
            continue;
        }
        for ev in inst.read(&mut buf)? {
            total += 1;
            let dir = wdmap
                .get(&ev.wd)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("wd{}", ev.wd));
            println!(
                "{} {} {} 0x{:08x} {} {} {} {}",
                mono_ns(),
                ev.wd,
                dir,
                ev.mask,
                ev.mask_names(),
                ev.cookie,
                if ev.has(ino::IN_ISDIR) { 1 } else { 0 },
                if ev.name.is_empty() { "-" } else { &ev.name }
            );
            // Auto-add watches for new directories, mimicking a recursive
            // watcher. Note the race: a file created inside the new directory
            // before this call is invisible.
            if auto_add && ev.has(ino::IN_CREATE) && ev.has(ino::IN_ISDIR) {
                let newdir = wdmap
                    .get(&ev.wd)
                    .map(|p| p.join(&ev.name))
                    .unwrap_or_else(|| PathBuf::from(&ev.name));
                match inst.add_watch(&newdir, mask) {
                    Ok(wd) => {
                        wdmap.insert(wd, newdir.clone());
                        println!("# auto-added watch wd={wd} on {}", newdir.display());
                    }
                    Err(e) => println!("# auto-add failed: {e}"),
                }
            }
        }
    }
    println!("# total_events {total}");
    Ok(())
}

// ---------------------------------------------------------------------------
// overflow
// ---------------------------------------------------------------------------

/// `overflow --root DIR --files N [--buffer BYTES] [--slow-ms MS]`
///
/// Creates N files in `root` while reading events. `--slow-ms` inserts a delay
/// before draining, which is what actually fills the kernel queue; a fast
/// reader never sees overflow no matter how many events are generated.
fn cmd_overflow(argv: &[String]) -> Result<()> {
    let root = PathBuf::from(arg(argv, "--root").ok_or_else(|| {
        Error::Usage("overflow requires --root".into())
    })?);
    let files: usize = num(argv, "--files", 50000);
    let buf_size: usize = num(argv, "--buffer", 1 << 16);
    let slow_ms: u64 = num(argv, "--slow-ms", 0);
    let mask = ino::default_mask();

    let inst = ino::Inotify::new(true)?;
    let wd = inst.add_watch(&root, mask)?;
    let queued_max = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unreadable".into());

    println!("root                     {}", root.display());
    println!("max_queued_events        {queued_max}");
    println!("read_buffer_bytes        {buf_size}");
    println!("slow_ms_before_drain     {slow_ms}");
    println!("files_target             {files}");

    // Create in a thread; the main thread drains. With slow_ms > 0 we sleep
    // first, so events accumulate in the kernel queue unbounded until the
    // limit is hit.
    let root2 = root.clone();
    let writer = std::thread::spawn(move || {
        let mut created = 0usize;
        let mut errors = 0usize;
        for i in 0..files {
            let p = root2.join(format!("f{i:07}"));
            match std::fs::File::create(&p) {
                Ok(_) => created += 1,
                Err(_) => errors += 1,
            }
        }
        (created, errors)
    });

    if slow_ms > 0 {
        std::thread::sleep(Duration::from_millis(slow_ms));
    }

    let mut buf = vec![0u8; buf_size];
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut overflow_events = 0u64;
    let mut drains = 0u64;
    let mut total_events = 0u64;
    let mut ignores = 0u64;
    let start = Instant::now();
    loop {
        let ready = inst.wait(100)?;
        if ready {
            let evs = inst.read(&mut buf)?;
            if !evs.is_empty() {
                drains += 1;
            }
            for ev in evs {
                total_events += 1;
                if ev.has(ino::IN_Q_OVERFLOW) {
                    overflow_events += 1;
                    println!(
                        "OVERFLOW monots_ns={} wd={} mask=0x{:08x} cookie={} name={:?}",
                        mono_ns(),
                        ev.wd,
                        ev.mask,
                        ev.cookie,
                        ev.name
                    );
                }
                if ev.has(ino::IN_IGNORED) {
                    ignores += 1;
                }
                for name in ev.mask_names().split('+') {
                    *counts.entry(name.to_string()).or_insert(0) += 1;
                }
            }
        } else if writer.is_finished() {
            // One final drain attempt then stop.
            break;
        }
        let _ = wd;
        if start.elapsed() > Duration::from_secs(180) {
            println!("# timeout after 180s");
            break;
        }
    }
    // Drain whatever is left.
    while inst.wait(10)? {
        let evs = inst.read(&mut buf)?;
        if evs.is_empty() {
            break;
        }
        drains += 1;
        for ev in evs {
            total_events += 1;
            if ev.has(ino::IN_Q_OVERFLOW) {
                overflow_events += 1;
            }
        }
    }
    let (created, errors) = writer.join().map_err(|_| Error::Usage("writer panicked".into()))?;
    let wall = start.elapsed();

    println!();
    println!("files_created            {created}");
    println!("create_errors            {errors}");
    println!("expected_events_min      {} (one IN_CREATE per file)", created);
    println!("events_received          {total_events}");
    println!("drain_calls              {drains}");
    println!("overflow_events          {overflow_events}");
    println!("ignored_events           {ignores}");
    println!("wall_s                   {:.3}", wall.as_secs_f64());
    println!();
    println!("event_type_counts:");
    for (k, v) in &counts {
        println!("  {k:<24} {v}");
    }
    println!();
    if overflow_events > 0 {
        println!(
            "conclusion               queue overflowed; {} IN_Q_OVERFLOW event(s); events lost = unobservable quantity",
            overflow_events
        );
    } else {
        println!("conclusion               no overflow observed with this reader speed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cost
// ---------------------------------------------------------------------------

/// `cost --root DIR --files N`
///
/// Observer-side CPU for N file creations. The writer and observer are in the
/// same process here, so the number is an upper bound on observer cost and is
/// reported as such.
fn cmd_cost(argv: &[String]) -> Result<()> {
    let root = PathBuf::from(arg(argv, "--root").ok_or_else(|| {
        Error::Usage("cost requires --root".into())
    })?);
    let files: usize = num(argv, "--files", 1000);
    let mask = ino::default_mask();
    let inst = ino::Inotify::new(true)?;
    let _wd = inst.add_watch(&root, mask)?;

    let (u0, s0, _) = rusage_self();
    let t0 = Instant::now();
    let mut buf = vec![0u8; 1 << 16];
    let mut events = 0u64;
    let mut drains = 0u64;
    let mut drain_lat: Vec<u64> = Vec::new();

    // Interleave create + drain so the queue never overflows.
    for i in 0..files {
        let p = root.join(format!("c{i:06}"));
        std::fs::File::create(&p).map_err(|e| Error::io(p.display().to_string(), e))?;
        if i % 50 == 0 {
            let d0 = Instant::now();
            while inst.wait(0)? {
                let evs = inst.read(&mut buf)?;
                if evs.is_empty() {
                    break;
                }
                drains += 1;
                events += evs.len() as u64;
            }
            drain_lat.push(d0.elapsed().as_nanos() as u64);
        }
    }
    // Final drain.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && inst.wait(20)? {
        let evs = inst.read(&mut buf)?;
        if evs.is_empty() {
            break;
        }
        drains += 1;
        events += evs.len() as u64;
    }
    let wall = t0.elapsed();
    let (u1, s1, rss) = rusage_self();

    println!("files_created            {files}");
    println!("events_received          {events}");
    println!("events_per_file          {:.3}", events as f64 / files as f64);
    println!("drain_calls              {drains}");
    println!("drain_latency_ns_p50     {}", percentile(&drain_lat, 50.0));
    println!("drain_latency_ns_p99     {}", percentile(&drain_lat, 99.0));
    println!("drain_latency_ns_mean    {:.0}", mean(&drain_lat));
    println!("wall_s                   {:.4}", wall.as_secs_f64());
    println!("cpu_user_s               {:.4}", u1 - u0);
    println!("cpu_system_s             {:.4}", s1 - s0);
    println!("cpu_total_s              {:.4}", (u1 - u0) + (s1 - s0));
    println!("max_rss_kb               {rss}");
    println!(
        "cpu_us_per_event         {:.3}",
        1e6 * ((u1 - u0) + (s1 - s0)) / events.max(1) as f64
    );
    println!("NOTE: writer and observer share this process; the CPU figure includes file creation.");
    Ok(())
}

// ---------------------------------------------------------------------------
// watch-count: how watches scale with directory count
// ---------------------------------------------------------------------------

/// `watch-count --root DIR`
///
/// Reports the number of directories under `root` and the inotify watch count
/// needed for full recursion, plus the current global watch usage from
/// `/proc/sys/fs/inotify/max_user_watches`.
fn cmd_watch_count(argv: &[String]) -> Result<()> {
    let root = PathBuf::from(arg(argv, "--root").ok_or_else(|| {
        Error::Usage("watch-count requires --root".into())
    })?);
    let mut dirs = 0u64;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.clone()];
    let mut max_depth = 0usize;
    while let Some(d) = stack.pop() {
        let depth = d.components().count();
        max_depth = max_depth.max(depth);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                match e.metadata() {
                    Ok(md) => {
                        if md.is_dir() {
                            dirs += 1;
                            stack.push(p);
                        } else {
                            files += 1;
                            bytes += md.size();
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
    println!("root                     {}", root.display());
    println!("directories              {dirs}");
    println!("files                    {files}");
    println!("bytes                    {bytes}");
    println!("max_depth_components     {max_depth}");
    println!("watches_needed_recursive {dirs}");
    println!(
        "max_user_watches         {}",
        std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unreadable".into())
    );
    Ok(())
}
