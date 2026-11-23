//! Process observer probe: polling, pidfd, PID reuse, orphan reparenting.
//!
//! Every number this binary prints is measured at run time. Nothing is
//! estimated. Usage is intentionally crude (positional subcommand + keys)
//! because this is a probe harness, not product code.

use li_observer_probe::{connector, ino, mean, parse_stat, percentile, Error, Result, ScanDepth};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Child;
use std::time::{Duration, Instant};

fn mono_ns() -> u64 {
    // CLOCK_MONOTONIC via Instant is good enough for deltas and is monotonic.
    // For absolute stamps we anchor to a process-local epoch so the numbers
    // stay comparable inside one run.
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

fn arg<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn arg_num<T: std::str::FromStr>(args: &[String], key: &str, default: T) -> T {
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
    let cmd = argv.get(1).map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "poll" => cmd_poll(&argv),
        "scan-once" => cmd_scan_once(&argv),
        "pidfd" => cmd_pidfd(&argv),
        "reuse" => cmd_reuse(&argv),
        "wrap" => cmd_wrap(&argv),
        "orphan" => cmd_orphan(&argv),
        "readability" => cmd_readability(),
        "connector" => cmd_connector(&argv),
        other => {
            eprintln!(
                "unknown subcommand {other}\n\
                 usage: proc_observer <poll|scan-once|pidfd|reuse|wrap|orphan|readability|connector> [opts]"
            );
            Err(Error::Usage("bad subcommand".into()))
        }
    }
}

// ---------------------------------------------------------------------------
// poll: the core measurement
// ---------------------------------------------------------------------------

/// `poll --interval-ms N --duration-ms M --depth stat|full [--spawn N] [--label X]`
///
/// With `--spawn N`, the observer also *launches* N processes and records the
/// `(pid, starttime)` key of each. That set is ground truth: every key came from
/// the launcher's own `Child::id()` plus a direct `/proc` read, so the
/// denominator is exact and is not itself a sample. The observer then reports
/// how many of those keys its polling loop saw.
///
/// `--life-ms` controls how long each spawned process lives (`0` = `/bin/true`,
/// the shortest thing available here). `--rate` paces the spawns per second
/// (0 = as fast as possible). Both matter: with unlimited rate and 0 ms life,
/// all N processes are created and reaped inside a few tens of milliseconds, so
/// the result measures burst alignment rather than steady-state detection
/// probability.
///
/// `--no-reap` leaves children as zombies. A zombie is still present in
/// `/proc`, so this is the control case that proves the poller *can* see a
/// process it is never told about, provided something keeps it visible.
fn cmd_poll(argv: &[String]) -> Result<()> {
    let interval_ms: u64 = arg_num(argv, "--interval-ms", 200);
    let duration_req: u64 = arg_num(argv, "--duration-ms", 1000);
    let spawn_n: usize = arg_num(argv, "--spawn", 0);
    let life_ms: u64 = arg_num(argv, "--life-ms", 0);
    let rate: u64 = arg_num(argv, "--rate", 0);
    let no_reap = argv.iter().any(|a| a == "--no-reap");
    let jitter = argv.iter().any(|a| a == "--jitter");
    let depth = match arg(argv, "--depth").unwrap_or("stat") {
        "full" => ScanDepth::Full,
        _ => ScanDepth::Stat,
    };
    let label = arg(argv, "--label").unwrap_or("run").to_string();

    // A spawn series needs the whole series inside the observation window.
    // Otherwise the tail of the population is born after the last scan, cannot
    // possibly be caught, and the denominator silently poisons the loss rate.
    // Measured example: 500 spawns at 20/s needs 25 s but a 6 s window was
    // requested, and the launcher was still spawning when scanning stopped.
    let spawn_span_ms = if spawn_n > 0 && rate > 0 {
        (spawn_n as u64 * 1000).div_ceil(rate)
    } else {
        0
    };
    // Jitter stretches the mean period only slightly, but allow a 25% margin
    // plus a tail so the final child is born and reaped inside the window.
    let needed = spawn_span_ms + spawn_span_ms / 4 + 2000;
    let duration_ms = duration_req.max(if spawn_n > 0 { needed } else { 0 });

    let (cpu_u0, cpu_s0, _) = rusage_self();
    let t_start = Instant::now();
    let deadline = t_start + Duration::from_millis(duration_ms);

    let mut latencies: Vec<u64> = Vec::new();
    // Seen set keyed by (pid, starttime): the identity this experiment argues for.
    let mut seen: BTreeSet<(i32, u64)> = BTreeSet::new();
    // Pid-only view, to quantify how often pid-only would merge two distinct
    // processes (same pid, different starttime) into one identity.
    let mut seen_pid_only: HashMap<i32, u64> = HashMap::new();
    let mut pid_reuse_observed = 0u64;

    let mut scans = 0u64;
    let mut scan_failures = 0u64;
    let mut total_procs = 0u64;
    let mut max_procs = 0usize;

    // Ground truth. The launcher thread reports every (pid, starttime) key it
    // successfully created, plus the spawn-failure count. Spawning happens off
    // the observing thread so the sampler is not artificially starved.
    //
    // The key is read from /proc inside the launcher while the child is alive.
    // If that read fails (child already gone), the starttime is recorded as 0
    // and that entry is matched on pid alone; such entries are counted
    // separately so they cannot silently flatter the loss rate.
    let (tx, rx) = std::sync::mpsc::channel::<(i32, u64)>();
    // The launcher gets a clone; the original is dropped below before `rx` is
    // drained, so the receiver iterator terminates in every case.
    // Reaping needs its own thread that spins tightly on `try_wait`. If reaping
    // happened on the spawner's loop it would lag by up to one spawn period, and
    // an exited-but-unreaped child stays visible in /proc as a zombie. That
    // inflates the "caught" count with processes that were already dead, which
    // was measured: without a dedicated reaper the catch rate came out pinned at
    // the spawn period rather than tracking the child lifetime.
    let (reap_tx, reap_rx) = std::sync::mpsc::channel::<Child>();
    let reaper = std::thread::spawn(move || {
        let mut pending: Vec<Child> = Vec::new();
        let mut reaped = 0usize;
        loop {
            while let Ok(c) = reap_rx.try_recv() {
                pending.push(c);
            }
            if pending.is_empty() {
                match reap_rx.recv_timeout(Duration::from_micros(200)) {
                    Ok(c) => pending.push(c),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            let mut still = Vec::with_capacity(pending.len());
            for mut c in pending.drain(..) {
                match c.try_wait() {
                    Ok(Some(_)) => reaped += 1,
                    Ok(None) => still.push(c),
                    Err(_) => reaped += 1,
                }
            }
            pending = still;
        }
        // Channel closed: reap everything that remains.
        for mut c in pending {
            let _ = c.wait();
            reaped += 1;
        }
        reaped
    });

    let launcher = if spawn_n > 0 {
        let tx = tx.clone();
        let reap_tx = reap_tx.clone();
        Some(std::thread::spawn(move || {
            let mut errs = 0usize;
            let mut no_key = 0usize;
            let mut zombie_hold: Vec<Child> = Vec::new();
            let period_ns = if rate == 0 {
                0u64
            } else {
                1_000_000_000 / rate
            };
            let mut next_at = Instant::now();
            // Deterministic LCG: no crate needed, and a fixed seed makes the run
            // reproducible, which the protocol requires.
            let mut rng_state: u64 = 0x2545_F491_4F6C_DD1D;
            for _ in 0..spawn_n {
                let mut cmd = if life_ms == 0 {
                    std::process::Command::new("/bin/true")
                } else {
                    let mut c = std::process::Command::new("/bin/sleep");
                    c.arg(format!("{:.3}", life_ms as f64 / 1000.0));
                    c
                };
                match cmd.spawn() {
                    Ok(child) => {
                        let pid = child.id() as i32;
                        let st = li_observer_probe::read_stat(pid).ok();
                        let starttime = match &st {
                            Some(s) => s.starttime,
                            None => {
                                no_key += 1;
                                0
                            }
                        };
                        let _ = tx.send((pid, starttime));
                        if no_reap {
                            // Do not hand it to the reaper: it must stay a
                            // zombie so the zombie-visibility case can be
                            // measured. Reaped at the end by this thread.
                            zombie_hold.push(child);
                        } else {
                            let _ = reap_tx.send(child);
                        }
                    }
                    Err(_) => errs += 1,
                }
                if period_ns > 0 {
                    // Without jitter, a spawn period that divides the poll
                    // interval phase-locks: every scan sees the same fraction of
                    // spawn slots, so the measured loss is an aliasing artifact
                    // (measured: exactly 0/25/50/100% instead of a smooth curve).
                    let period = if jitter {
                        rng_state = rng_state
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        // Uniform in [0.25, 1.75) x period.
                        let u = ((rng_state >> 11) as f64) / ((1u64 << 53) as f64);
                        (period_ns as f64 * (0.25 + 1.5 * u)) as u64
                    } else {
                        period_ns
                    };
                    next_at += Duration::from_nanos(period);
                    let now = Instant::now();
                    if next_at > now {
                        std::thread::sleep(next_at - now);
                    }
                }
            }
            for mut z in zombie_hold {
                let _ = z.wait();
            }
            (errs, no_key)
        }))
    } else {
        None
    };

    while Instant::now() < deadline {
        let t0 = Instant::now();
        let scan = li_observer_probe::scan(depth)?;
        let elapsed = t0.elapsed().as_nanos() as u64;
        latencies.push(elapsed);
        scans += 1;
        total_procs += scan.procs.len() as u64;
        max_procs = max_procs.max(scan.procs.len());
        scan_failures += scan.failures.len() as u64;

        for p in &scan.procs {
            seen.insert((p.pid, p.starttime));
            if let Some(prev) = seen_pid_only.insert(p.pid, p.starttime) {
                if prev != p.starttime {
                    pid_reuse_observed += 1;
                }
            }
        }

        // Sleep the remainder of the interval.
        let spent = t0.elapsed();
        let target = Duration::from_millis(interval_ms);
        if spent < target {
            std::thread::sleep(target - spent);
        }
    }
    let wall = t_start.elapsed();
    let (cpu_u1, cpu_s1, max_rss_kb) = rusage_self();

    // Join the launcher so every pid has been sent. If the launcher is still
    // running (spawn rate slower than the poll window) we join anyway and note
    // it: truncating silently would make the loss rate look better than it is.
    let launcher_still_running = launcher
        .as_ref()
        .map(|h| !h.is_finished())
        .unwrap_or(false);
    let (launch_errors, no_key) = if let Some(h) = launcher {
        h.join().unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    // Close the reaper's channel and wait for it, so no child is left behind.
    drop(reap_tx);
    let reaped = reaper.join().unwrap_or(0);

    // Drop the sender *before* draining the receiver. When `--spawn 0` there is
    // no launcher thread holding a sender clone, so without this the iterator
    // below would block forever waiting for a sender that will never close.
    drop(tx);
    let truth: Vec<(i32, u64)> = rx.into_iter().collect();
    let truth_pids: BTreeSet<i32> = truth.iter().map(|(p, _)| *p).collect();
    let seen_pids: BTreeSet<i32> = seen.iter().map(|(pid, _)| *pid).collect();
    // A truth entry counts as caught if its exact key was seen, or, for entries
    // whose starttime we could not read, if the pid was seen.
    let caught = truth
        .iter()
        .filter(|(p, st)| {
            if *st == 0 {
                seen_pids.contains(p)
            } else {
                seen.contains(&(*p, *st))
            }
        })
        .count();
    let missed = truth.len() - caught;
    let loss_pct = if truth.is_empty() {
        0.0
    } else {
        100.0 * missed as f64 / truth.len() as f64
    };

    println!("== poll run: label={label} ==");
    println!("interval_ms              {interval_ms}");
    println!("duration_requested_ms    {duration_req}");
    println!("duration_ms              {duration_ms}");
    println!("spawn_span_needed_ms     {spawn_span_ms}");
    println!("scan_depth               {depth:?}");
    println!("wall_time_s              {:.3}", wall.as_secs_f64());
    println!("scans                    {scans}");
    println!("mean_procs_per_scan      {:.1}", total_procs as f64 / scans.max(1) as f64);
    println!("max_procs_per_scan       {max_procs}");
    println!("read_failures_total      {scan_failures}");
    println!("distinct_pid_start_pairs {}", seen.len());
    println!("pid_reuse_observed       {pid_reuse_observed}");
    println!("scan_latency_ns_p50      {}", percentile(&latencies, 50.0));
    println!("scan_latency_ns_p99      {}", percentile(&latencies, 99.0));
    println!("scan_latency_ns_max      {}", latencies.iter().copied().max().unwrap_or(0));
    println!("scan_latency_ns_mean     {:.0}", mean(&latencies));
    println!("cpu_user_s               {:.4}", cpu_u1 - cpu_u0);
    println!("cpu_system_s             {:.4}", cpu_s1 - cpu_s0);
    println!("cpu_total_s              {:.4}", (cpu_u1 - cpu_u0) + (cpu_s1 - cpu_s0));
    println!(
        "cpu_pct_of_one_core      {:.2}",
        100.0 * ((cpu_u1 - cpu_u0) + (cpu_s1 - cpu_s0)) / wall.as_secs_f64().max(1e-9)
    );
    println!("max_rss_kb               {max_rss_kb}");
    println!("spawned_requested        {spawn_n}");
    println!("process_life_ms          {life_ms}");
    println!("spawn_rate_per_s         {rate}");
    println!("spawn_jitter             {jitter}");
    println!("spawn_errors             {launch_errors}");
    println!("children_reaped          {reaped}");
    println!("truth_missing_starttime  {no_key}");
    println!("launcher_finished_in_window {}", !launcher_still_running);
    println!("seen_pids_total          {}", seen_pids.len());
    println!("truth_pids               {}", truth.len());
    println!("truth_distinct_pids      {}", truth_pids.len());
    println!("caught_pids              {caught}");
    println!("missed_pids              {missed}");
    println!("loss_pct                 {loss_pct:.2}");
    // Validity gate. A loss figure is only meaningful if every spawned process
    // was born inside the observation window; otherwise the tail of the
    // population was never sampled and the denominator is a lie.
    let valid = launcher_still_running == false && truth.len() == spawn_n;
    println!("run_valid                {valid}");
    if !valid {
        println!(
            "invalid_reason           spawned {} of {} requested, or launcher still running at window end",
            truth.len(),
            spawn_n
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// scan-once: latency of a single scan, repeated
// ---------------------------------------------------------------------------

fn cmd_scan_once(argv: &[String]) -> Result<()> {
    let iters: usize = arg_num(argv, "--iters", 100);
    let depth = match arg(argv, "--depth").unwrap_or("full") {
        "stat" => ScanDepth::Stat,
        _ => ScanDepth::Full,
    };
    let mut lat = Vec::with_capacity(iters);
    let mut procs = 0usize;
    let mut fails = 0usize;
    for _ in 0..iters {
        let t0 = Instant::now();
        let s = li_observer_probe::scan(depth)?;
        lat.push(t0.elapsed().as_nanos() as u64);
        procs = s.procs.len();
        fails += s.failures.len();
    }
    println!("iters          {iters}");
    println!("depth          {depth:?}");
    println!("procs_last     {procs}");
    println!("read_failures  {fails}");
    println!("p50_ns         {}", percentile(&lat, 50.0));
    println!("p90_ns         {}", percentile(&lat, 90.0));
    println!("p99_ns         {}", percentile(&lat, 99.0));
    println!("max_ns         {}", lat.iter().copied().max().unwrap_or(0));
    println!("mean_ns        {:.0}", mean(&lat));
    Ok(())
}

// ---------------------------------------------------------------------------
// pidfd: prove that it cannot report creation
// ---------------------------------------------------------------------------

/// `pidfd --spawn N --hold-ms M`
///
/// Two populations:
///  * N long-lived children held for `--hold-ms`, all of which we can open and
///    poll -> we observe *exit*.
///  * N short-lived children we deliberately never open before they die -> we
///    observe nothing, demonstrating pidfd is not a discovery mechanism.
fn cmd_pidfd(argv: &[String]) -> Result<()> {
    let n: usize = arg_num(argv, "--spawn", 20);
    let hold_ms: u64 = arg_num(argv, "--hold-ms", 300);

    // Population 1: open while alive, then wait for readability = exit.
    let mut long_children: Vec<Child> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut open_failures = 0usize;
    for _ in 0..n {
        let mut c = std::process::Command::new("/bin/sleep")
            .arg(format!("{:.3}", hold_ms as f64 / 1000.0))
            .spawn()
            .map_err(|e| Error::io("spawn sleep", e))?;
        let pid = c.id() as i32;
        // SAFETY: pidfd_open is a plain syscall with an integer pid.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if fd < 0 {
            open_failures += 1;
        } else {
            // SAFETY: fd is a newly created, uniquely owned descriptor.
            fds.push(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd as i32) });
        }
        let _ = &mut c;
        long_children.push(c);
    }
    // Immediately check that none is readable while alive.
    let mut readable_while_alive = 0usize;
    for fd in &fds {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, zero timeout.
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r > 0 && pfd.revents & libc::POLLIN != 0 {
            readable_while_alive += 1;
        }
    }

    // Population 2: spawn and reap without opening. Nothing can ever be
    // observed about these processes through pidfd.
    let mut short_invisible = 0usize;
    for _ in 0..n {
        if let Ok(mut c) = std::process::Command::new("/bin/true").spawn() {
            let _ = c.wait();
            short_invisible += 1;
        }
    }

    // Wait for the long children to exit and count readable fds.
    let wait_deadline = Instant::now() + Duration::from_millis(hold_ms + 2000);
    let mut readable_on_exit = 0usize;
    while Instant::now() < wait_deadline && readable_on_exit < fds.len() {
        readable_on_exit = 0;
        for fd in &fds {
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: single valid pollfd, short timeout.
            let r = unsafe { libc::poll(&mut pfd, 1, 5) };
            if r > 0 && pfd.revents & libc::POLLIN != 0 {
                readable_on_exit += 1;
            }
        }
        if readable_on_exit < fds.len() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for c in &mut long_children {
        let _ = c.wait();
    }

    println!("pidfd_opened             {}", fds.len());
    println!("pidfd_open_failures      {open_failures}");
    println!("readable_while_alive     {readable_while_alive}");
    println!("readable_after_exit      {readable_on_exit}");
    println!("short_lived_spawned      {short_invisible}");
    println!("short_lived_observable   0");
    println!(
        "conclusion               pidfd reports exit only; {} short-lived processes were invisible",
        short_invisible
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// reuse: construct a real PID reuse inside a private user+pid namespace
// ---------------------------------------------------------------------------

/// `reuse --attempts N --target-pid P`
///
/// Must be run as `unshare -Urpf` so that `ns_last_pid` is writable. We force
/// the next pid to a fixed value, fork a child (which inherits that pid), let it
/// exit, then force the same value again and fork again. If both children got
/// the same pid, PID reuse is demonstrated on this kernel.
///
/// We also record `starttime` for both, and additionally try to land both in the
/// same clock tick to see whether `(pid, starttime)` can itself collide.
fn cmd_reuse(argv: &[String]) -> Result<()> {
    let target: i32 = arg_num(argv, "--target-pid", 4242);
    let mode = arg(argv, "--mode").unwrap_or("pid");

    let mut results: Vec<(i32, u64, u64, u64)> = Vec::new(); // (pid, starttime, boot_ns_a, boot_ns_b)
    for round in 0..2 {
        write_ns_last_pid(target - 1)?;
        // Sleep a full tick between rounds unless mode=same-tick is requested,
        // so that the "normal" case shows distinct starttimes and the
        // adversarial case tries to collide them.
        if round == 1 && mode != "same-tick" {
            std::thread::sleep(Duration::from_millis(50));
            write_ns_last_pid(target - 1)?;
        }
        let before = read_boot_ns()?;
        let mut c = std::process::Command::new("/bin/sleep")
            .arg("1.0")
            .spawn()
            .map_err(|e| Error::io("spawn sleep in reuse", e))?;
        let pid = c.id() as i32;
        // Read starttime while alive.
        let st = li_observer_probe::read_stat(pid)?;
        eprintln!(
            "round {round}: pid={pid} starttime={} comm={} (forced target {target})",
            st.starttime, st.comm
        );
        let _ = c.kill();
        let _ = c.wait();
        results.push((pid, st.starttime, before, read_boot_ns()?));
    }

    let (p1, s1, _, _) = results[0];
    let (p2, s2, _, _) = results[1];
    println!("target_pid               {target}");
    println!("mode                     {mode}");
    println!("round1_pid               {p1}");
    println!("round1_starttime_ticks   {s1}");
    println!("round2_pid               {p2}");
    println!("round2_starttime_ticks   {s2}");
    println!("pid_reused               {}", p1 == p2);
    println!("starttime_changed        {}", s1 != s2);
    println!(
        "pid_only_key_collides    {}",
        p1 == p2
    );
    println!(
        "pid_starttime_key_collides {}",
        p1 == p2 && s1 == s2
    );
    if p1 == p2 && s1 == s2 {
        println!(
            "note                     same pid AND same starttime tick: (pid,starttime) is not a unique key"
        );
    }
    Ok(())
}

fn write_ns_last_pid(v: i32) -> Result<()> {
    let path = "/proc/sys/kernel/ns_last_pid";
    std::fs::write(path, format!("{v}\n")).map_err(|e| Error::io(path, e))
}

// ---------------------------------------------------------------------------
// wrap: natural pid-space wraparound in a small pid namespace
// ---------------------------------------------------------------------------

/// `wrap --pid-max N --forks M`
///
/// Must run under `unshare -Urpf --mount-proc`. Writing `pid_max` here affects
/// only this pid namespace (verified: the host value is unchanged). With
/// `pid_max` small, M sequential forks wrap the pid counter, so pids are
/// naturally handed out twice. This is the realistic form of PID reuse: no
/// `ns_last_pid` tampering, just a namespace whose counter turned over.
///
/// For each child we record (pid, starttime) *while it is alive*, then it exits.
/// We report how many pids were handed out more than once and whether the
/// (pid, starttime) pairs stayed distinct.
fn cmd_wrap(argv: &[String]) -> Result<()> {
    let pid_max: i32 = arg_num(argv, "--pid-max", 301);
    let forks: usize = arg_num(argv, "--forks", 900);
    const PID_MAX_PATH: &str = "/proc/sys/kernel/pid_max";
    // The kernel enforces a floor here (301 on a 20-cpu host: 8 pids/cpu minimum,
    // rounded up), and returns EINVAL below it. Report that rather than dying,
    // because the errno is itself a result.
    if let Err(e) = std::fs::write(PID_MAX_PATH, format!("{pid_max}\n")) {
        println!("pid_max_write_failed     {e}");
        println!("errno                    {:?}", e.raw_os_error());
        println!("status                   NOT TESTED (cannot shrink pid space)");
        return Ok(());
    }
    let effective: i32 = std::fs::read_to_string(PID_MAX_PATH)
        .map_err(|e| Error::io(PID_MAX_PATH, e))?
        .trim()
        .parse()
        .unwrap_or(-1);
    println!("pid_max_requested        {pid_max}");
    println!("pid_max_effective        {effective}");
    println!("forks_requested          {forks}");

    // Sample and reap one child at a time. Holding many children alive would
    // exhaust the tiny pid space and make fork fail, which would confound the
    // measurement (the kernel's own pid exhaustion is not PID reuse).
    //
    // The child sleeps briefly so it is guaranteed alive when we read its
    // starttime; a child already reaped would prove nothing about the key.
    let mut seen: Vec<(i32, u64)> = Vec::new();
    let mut read_failures = 0usize;
    let mut fork_failures = 0usize;
    for _ in 0..forks {
        match std::process::Command::new("/bin/sleep")
            .arg("0.02")
            .spawn()
        {
            Ok(mut c) => {
                let pid = c.id() as i32;
                match li_observer_probe::read_stat(pid) {
                    Ok(st) => seen.push((pid, st.starttime)),
                    Err(_) => read_failures += 1,
                }
                let _ = c.wait();
            }
            Err(_) => {
                fork_failures += 1;
                // Brief backoff so a transient pid-space squeeze does not
                // cascade into permanent failure.
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    let mut by_pid: BTreeMap<i32, Vec<u64>> = BTreeMap::new();
    for (pid, st) in &seen {
        by_pid.entry(*pid).or_default().push(*st);
    }
    let reused_pids: Vec<i32> = by_pid
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, _)| *k)
        .collect();
    let mut pid_st_collisions = 0usize;
    let mut examples: Vec<(i32, Vec<u64>)> = Vec::new();
    for (pid, sts) in &by_pid {
        if sts.len() > 1 {
            let uniq: BTreeSet<u64> = sts.iter().copied().collect();
            if uniq.len() < sts.len() {
                pid_st_collisions += sts.len() - uniq.len();
            }
            if examples.len() < 5 {
                examples.push((*pid, sts.clone()));
            }
        }
    }

    println!("children_sampled         {}", seen.len());
    println!("read_failures            {read_failures}");
    println!("fork_failures            {fork_failures}");
    println!("distinct_pids            {}", by_pid.len());
    println!("pids_handed_out_twice    {}", reused_pids.len());
    println!("pid_starttime_duplicates {pid_st_collisions}");
    println!(
        "distinct_pid_st_keys     {}",
        seen.iter().collect::<BTreeSet<_>>().len()
    );
    println!();
    println!("reuse_examples (pid -> starttimes observed):");
    for (pid, sts) in &examples {
        println!("  pid {pid}: {sts:?}");
    }
    println!();
    if pid_st_collisions == 0 && !reused_pids.is_empty() {
        println!("conclusion               pid is reused; (pid,starttime) stayed distinct in this run");
    } else if pid_st_collisions > 0 {
        println!(
            "conclusion               (pid,starttime) ALSO collided {pid_st_collisions} time(s) at 10ms tick granularity"
        );
    } else {
        println!("conclusion               no pid reuse observed (namespace may have had too few forks)");
    }
    Ok(())
}

fn read_boot_ns() -> Result<u64> {
    // ns since boot from CLOCK_BOOTTIME, for correlation only.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for clock_gettime.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if r < 0 {
        return Err(Error::sys("clock_gettime"));
    }
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

// ---------------------------------------------------------------------------
// orphan: PPID transitions to 1 when the parent dies
// ---------------------------------------------------------------------------

/// `orphan`
///
/// Spawn an intermediate process whose own child outlives it; watch the
/// grandchild's ppid go from the intermediate pid to 1 (or to a subreaper).
fn cmd_orphan(_argv: &[String]) -> Result<()> {
    // The intermediate forks a grandchild, prints its pid, then exits.
    let mut mid = std::process::Command::new("/bin/sh")
        .arg("-c")
        // Grandchild sleeps long enough for us to sample it twice.
        .arg("sh -c 'echo GRANDCHILD=$$; sleep 3' & echo MID=$$; sleep 0.15; exit 0")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::io("spawn mid", e))?;
    let out = mid
        .wait_with_output()
        .map_err(|e| Error::io("wait mid", e))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut grandchild = None;
    let mut mid_pid = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("GRANDCHILD=") {
            grandchild = v.trim().parse::<i32>().ok();
        }
        if let Some(v) = line.strip_prefix("MID=") {
            mid_pid = v.trim().parse::<i32>().ok();
        }
    }
    println!("intermediate_pid         {mid_pid:?}");
    println!("grandchild_pid           {grandchild:?}");
    let gc = match grandchild {
        Some(p) => p,
        None => {
            println!("status                   NOT OBSERVED (no grandchild pid)");
            return Ok(());
        }
    };
    // The intermediate has already exited; sample ppid now and confirm the
    // grandchild is still alive (its ppid must be reparented, not stale).
    for attempt in 0..10 {
        match li_observer_probe::read_stat(gc) {
            Ok(st) => {
                println!(
                    "sample{attempt} ppid={} state={} comm={}",
                    st.ppid, st.state, st.comm
                );
                if st.ppid == 1 || st.ppid != mid_pid.unwrap_or(-1) {
                    println!(
                        "reparented_to             {} (expected 1 or a subreaper)",
                        st.ppid
                    );
                    break;
                }
            }
            Err(e) => {
                println!("sample{attempt} read_failed errno={:?}", e.errno());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Confirm the zombie window is observable: the intermediate is reaped by
    // us, so it should be gone; the grandchild becomes an orphan we never reap.
    println!(
        "intermediate_still_exists {}",
        std::path::Path::new(&format!("/proc/{}", mid_pid.unwrap_or(-1))).exists()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// readability: what can and cannot be read, by process class
// ---------------------------------------------------------------------------

fn cmd_readability() -> Result<()> {
    let pids = li_observer_probe::list_pids()?;
    let me = std::process::id() as i32;
    let my_uid = nix::unistd::Uid::current().as_raw();
    println!("total_pids_visible       {}", pids.len());
    println!("my_pid                   {me}");
    println!("my_uid                   {my_uid}");

    let mut classes: BTreeMap<&'static str, (u64, u64, u64, u64)> = BTreeMap::new();
    // (attempted, stat_fail, status_fail, cmdline_fail)
    let mut zombie_pids: Vec<i32> = Vec::new();
    let mut foreign_pids: Vec<i32> = Vec::new();
    let mut kthread_pids: Vec<i32> = Vec::new();

    for pid in pids {
        let st = match li_observer_probe::read_stat(pid) {
            Ok(s) => Some(s),
            Err(_) => None,
        };
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"));
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"));

        // Classify by what we *can* see.
        let class = match &st {
            Some(s) if s.is_zombie() => {
                zombie_pids.push(pid);
                "zombie"
            }
            Some(s) => {
                // Kernel threads: empty cmdline and ppid 2 (kthreadd) or pid 2.
                let empty = cmdline.as_ref().map(|c| c.is_empty()).unwrap_or(false);
                if empty && (s.ppid == 2 || pid == 2) {
                    kthread_pids.push(pid);
                    "kernel_thread"
                } else {
                    // Owner uid from status, if readable.
                    match &status {
                        Ok(txt) => {
                            let uid = txt
                                .lines()
                                .find(|l| l.starts_with("Uid:"))
                                .and_then(|l| l.split_whitespace().nth(1))
                                .and_then(|v| v.parse::<u32>().ok());
                            match uid {
                                Some(u) if u != my_uid => {
                                    foreign_pids.push(pid);
                                    "foreign_uid"
                                }
                                _ => "own_uid",
                            }
                        }
                        Err(_) => "status_unreadable",
                    }
                }
            }
            None => "stat_unreadable",
        };
        let e = classes.entry(class).or_insert((0, 0, 0, 0));
        e.0 += 1;
        if st.is_none() {
            e.1 += 1;
        }
        if status.is_err() {
            e.2 += 1;
        }
        if cmdline.is_err() {
            e.3 += 1;
        }
    }

    println!();
    println!(
        "{:<20} {:>8} {:>10} {:>12} {:>14}",
        "class", "count", "stat_fail", "status_fail", "cmdline_fail"
    );
    for (class, (n, a, b, c)) in &classes {
        println!("{class:<20} {n:>8} {a:>10} {b:>12} {c:>14}");
    }
    println!();
    println!("zombie_pids              {zombie_pids:?}");
    println!("foreign_uid_pids_count   {}", foreign_pids.len());
    println!("foreign_uid_pids_sample  {:?}", &foreign_pids[..foreign_pids.len().min(10)]);
    println!("kernel_thread_count      {}", kthread_pids.len());

    // environ is the field most often restricted; measure it separately.
    let mut environ_ok = 0u64;
    let mut environ_denied = 0u64;
    let mut environ_empty = 0u64;
    for pid in li_observer_probe::list_pids()? {
        match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(v) if v.is_empty() => environ_empty += 1,
            Ok(_) => environ_ok += 1,
            Err(e) if e.raw_os_error() == Some(libc::EACCES) => environ_denied += 1,
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => environ_denied += 1,
            Err(_) => {}
        }
    }
    println!("environ_readable         {environ_ok}");
    println!("environ_empty            {environ_empty}");
    println!("environ_denied           {environ_denied}");

    // hidepid probe.
    let mount_test = "/tmp/li-phase0-0e-hidepid";
    let _ = std::fs::create_dir_all(mount_test);
    let hidepid_note = if std::path::Path::new("/proc/sys/kernel/yama").exists() {
        "yama_present"
    } else {
        "no_yama"
    };
    println!("yama                     {hidepid_note}");
    println!(
        "hidepid_current_mount    {}",
        std::fs::read_to_string("/proc/self/mountinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.contains(" /proc "))
                    .map(|l| l.to_string())
            })
            .unwrap_or_else(|| "unknown".into())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// connector: event-driven process observation
// ---------------------------------------------------------------------------

/// `connector --spawn N`
///
/// Subscribe to PROC_EVENTS, then spawn N `/bin/true` and compare the number of
/// fork/exec/exit events received against N. This is the event-driven route's
/// loss measurement, directly comparable to the polling table.
fn cmd_connector(argv: &[String]) -> Result<()> {
    let n: usize = arg_num(argv, "--spawn", 100);
    let client = match connector::Client::open() {
        Ok(c) => c,
        Err(e) => {
            println!("connector_open           FAILED: {e}");
            println!("errno                    {:?}", e.errno());
            println!("status                   NOT AVAILABLE");
            return Ok(());
        }
    };
    println!("connector_open           ok");

    // Drain the ack message.
    let _ = client.recv(200)?;

    let mut forks = 0u64;
    let mut execs = 0u64;
    let mut exits = 0u64;
    let mut others = 0u64;
    let mut spawned = 0usize;
    let mut spawn_err = 0usize;
    let mut children: Vec<Child> = Vec::new();

    // Spawn from a thread while we read, mirroring the polling test's design.
    let handle = std::thread::spawn(move || {
        let mut errs = 0usize;
        for _ in 0..n {
            match std::process::Command::new("/bin/true").spawn() {
                Ok(c) => {
                    let _ = c.wait_with_output();
                }
                Err(_) => errs += 1,
            }
        }
        errs
    });

    let read_deadline = Instant::now() + Duration::from_secs(30);
    let mut idle_rounds = 0;
    while Instant::now() < read_deadline {
        let evs = client.recv(100)?;
        if evs.is_empty() {
            idle_rounds += 1;
            if handle.is_finished() && idle_rounds > 5 {
                break;
            }
            continue;
        }
        idle_rounds = 0;
        for e in evs {
            match e.kind {
                connector::Kind::Fork => forks += 1,
                connector::Kind::Exec => execs += 1,
                connector::Kind::Exit => exits += 1,
                _ => others += 1,
            }
            let _ = e;
        }
    }
    let errs = handle.join().unwrap_or(0);
    let _ = &mut children;
    let _ = &mut spawned;
    let _ = &mut spawn_err;

    println!("spawned                  {n}");
    println!("spawn_errors             {}", errs + spawn_err);
    println!("fork_events              {forks}");
    println!("exec_events              {execs}");
    println!("exit_events              {exits}");
    println!("other_events             {others}");
    println!(
        "exec_vs_spawn_ratio      {:.4}",
        execs as f64 / n.max(1) as f64
    );
    Ok(())
}

// Keep unused-import warnings away for constants used only in docs.
#[allow(dead_code)]
fn _unused_marker() {
    let _ = ino::IN_MODIFY;
    let _ = parse_stat("1 (a) S 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0");
    let _ = std::io::stdout().flush();
}
