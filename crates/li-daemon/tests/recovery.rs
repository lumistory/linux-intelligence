//! Phase 5 (P5-04 .. P5-06) 恢复提供者集成测试：
//! - 严格仅在 `/tmp` 沙盒中测试 `BtrfsProvider`（绝不接触宿主机真实 btrfs 子卷）
//! - 测试 `LvmProvider` 宿主机能力探测与清晰降级
//! - 测试 `ProviderRegistry::with_defaults` 非空 caveats 强制约束与完整恢复审计流

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use li_daemon::recovery::{
    Approval, BtrfsProvider, LvmProvider, ProviderRegistry, RecoveryProvider, execute_restore,
};
use li_journal::read::Reader;
use li_journal::{Config, Journal};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("li-recovery-test-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).expect("mkdir scratch");
    p
}

#[test]
fn registry_with_defaults_accepts_btrfs_and_lvm_with_nonempty_caveats() {
    let reg = ProviderRegistry::with_defaults().expect("default registry should register");
    for kind in ["manual", "git", "btrfs", "lvm"] {
        let p = reg.get(kind).expect("provider present");
        let caps = p.capabilities();
        assert!(!caps.caveats.is_empty(), "{kind} must declare caveats");
    }
}

#[test]
fn btrfs_provider_sandbox_create_modify_restore_and_verify() {
    let dir = scratch_dir("btrfs");
    let target = dir.join("workspace");
    let backups = dir.join("backups");
    let nested = target.join("sub");
    fs::create_dir_all(&nested).expect("mkdir workspace/sub");
    fs::write(target.join("alpha.txt"), b"v1-alpha\n").expect("write alpha");
    fs::write(nested.join("beta.bin"), [0_u8, 1, 2, 3, 255]).expect("write beta");

    let provider = BtrfsProvider;
    let cp = provider
        .create_checkpoint(&target, &backups, Some("sandbox-test"))
        .expect("btrfs mkfs.btrfs -r checkpoint in /tmp sandbox");
    assert_eq!(cp.kind, "btrfs");

    let inspected = provider.inspect(&cp).expect("dump-super inspect");
    assert!(
        inspected.contains("_BHRfS_M"),
        "expected Btrfs magic in superblock summary: {inspected}"
    );

    // Mutate target files after checkpoint.
    fs::write(target.join("alpha.txt"), b"CORRUPTED\n").expect("corrupt alpha");
    fs::remove_file(nested.join("beta.bin")).expect("delete beta");

    let scope = provider
        .preview_restore(&cp, &target)
        .expect("preview restore");
    assert_eq!(scope.files.len(), 2);
    assert_eq!(scope.would_overwrite, vec![PathBuf::from("alpha.txt")]);

    let db_path = dir.join("recovery.db");
    let mut journal = Journal::open(&db_path, Config::default()).expect("open journal");
    journal.migrate().expect("migrate journal");
    let approval = Approval {
        approval_id: "appr-btrfs-1".into(),
        uid: 1000,
        auth_method: "explicit_cli",
        scope_hash: scope.scope_hash.clone(),
    };
    let report =
        execute_restore(&provider, &cp, &scope, &approval, &journal).expect("execute_restore");
    assert!(
        report.verified,
        "post-restore verification failed: {:?}",
        report.mismatches
    );
    assert_eq!(
        fs::read(target.join("alpha.txt")).expect("read alpha"),
        b"v1-alpha\n"
    );
    assert_eq!(
        fs::read(nested.join("beta.bin")).expect("read beta"),
        vec![0_u8, 1, 2, 3, 255]
    );

    let reader = Reader::open(&db_path, &Config::default()).expect("open reader");
    let events = reader
        .query_events(None, None, None, 20)
        .expect("query recovery audit");
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(types.contains(&"ApprovalGranted"));
    assert!(types.contains(&"RecoveryStarted"));
    assert!(types.contains(&"RecoveryCompleted"));

    // P5-06: intentionally fail a restore (missing image) and verify readable RecoveryCompleted failure audit.
    let mut missing_cp = cp.clone();
    missing_cp.reference = Some(dir.join("missing.img").to_string_lossy().into_owned());
    let err = execute_restore(&provider, &missing_cp, &scope, &approval, &journal)
        .expect_err("missing btrfs image must fail cleanly");
    assert!(err.contains("btrfs image missing"));
    let post_fail = reader
        .query_events(None, None, None, 20)
        .expect("query post-fail audit");
    assert!(
        post_fail.iter().any(|e| e.event_type == "RecoveryCompleted"
            && e.payload.get("verified").and_then(|v| v.as_bool()) == Some(false)),
        "expected readable failed RecoveryCompleted record"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn lvm_provider_probes_host_and_degrades_honestly() {
    let provider = LvmProvider;
    assert!(!provider.capabilities().caveats.is_empty());
    match LvmProvider::probe_tools() {
        Ok((lvs, lvcreate)) => {
            // Host happens to have lvm2: unconfigured checkpoint must still refuse safely.
            let dir = scratch_dir("lvm-present");
            let err = provider
                .create_checkpoint(&dir, &dir, None)
                .expect_err("unprivileged lvm creation must refuse safely");
            assert!(
                err.contains(&lvs.display().to_string())
                    || err.contains(&lvcreate.display().to_string())
            );
            let _ = fs::remove_dir_all(&dir);
        }
        Err(msg) => {
            assert!(
                msg.contains("lvm2 tools unavailable"),
                "unexpected missing-tool diagnostic: {msg}"
            );
            let dir = scratch_dir("lvm-absent");
            let err = provider
                .create_checkpoint(&dir, &dir, None)
                .expect_err("missing lvm2 must fail cleanly");
            assert!(err.contains("lvm2 tools unavailable"));
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
