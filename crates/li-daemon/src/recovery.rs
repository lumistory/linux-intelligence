//! Recovery provider abstraction and safe scratch-only `file` / `git` providers
//! (Phase 5: P5-01 .. P5-03, P5-06, ADR 0006).
//!
//! Every provider MUST declare non-empty `caveats`; registering a provider with
//! empty caveats is rejected. Every restore MUST pass `preview -> overwrite_check
//! -> explicit approval -> post-restore verify`, recording audit events on both
//! success and failure.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use li_journal::write::Checkpoint;
use li_journal::{Journal, clock};
use li_proto::{Event, EventId, EventType, EvidenceStatus, Source};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub kind: &'static str,
    pub atomic_snapshot: bool,
    pub caveats: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreScope {
    pub checkpoint_id: String,
    pub target_root: PathBuf,
    pub files: Vec<PathBuf>,
    pub would_overwrite: Vec<PathBuf>,
    pub scope_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub approval_id: String,
    pub uid: u32,
    pub auth_method: &'static str,
    pub scope_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub verified: bool,
    pub checked_files: Vec<PathBuf>,
    pub mismatches: Vec<String>,
}

pub trait RecoveryProvider {
    fn capabilities(&self) -> ProviderCapabilities;

    fn create_checkpoint(
        &self,
        target_root: &Path,
        backup_root: &Path,
        note: Option<&str>,
    ) -> Result<Checkpoint, String>;

    fn inspect(&self, cp: &Checkpoint) -> Result<String, String>;

    fn preview_restore(&self, cp: &Checkpoint, target_root: &Path) -> Result<RestoreScope, String>;

    fn restore(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<(), String>;

    fn verify(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<VerifyReport, String>;
}

/// Enforces ADR 0006: no provider without non-empty `caveats` may register.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Box<dyn RecoveryProvider>>,
}

impl ProviderRegistry {
    /// Registers default providers (`manual`, `git`, `btrfs`, `lvm`) respecting ADR 0006 caveats.
    pub fn with_defaults() -> Result<Self, String> {
        let mut reg = Self::default();
        reg.register(Box::new(FileBackupProvider))?;
        reg.register(Box::new(GitProvider))?;
        reg.register(Box::new(BtrfsProvider))?;
        reg.register(Box::new(LvmProvider))?;
        Ok(reg)
    }

    pub fn register(&mut self, provider: Box<dyn RecoveryProvider>) -> Result<(), String> {
        let caps = provider.capabilities();
        if caps.caveats.is_empty() || caps.caveats.iter().all(|c| c.trim().is_empty()) {
            return Err(format!(
                "refusing provider `{}`: ADR 0006 requires non-empty caveats",
                caps.kind
            ));
        }
        self.providers.insert(caps.kind.to_string(), provider);
        Ok(())
    }

    pub fn get(&self, kind: &str) -> Option<&dyn RecoveryProvider> {
        self.providers.get(kind).map(|b| b.as_ref())
    }
}

/// File-copy backup provider (P5-02).
pub struct FileBackupProvider;

impl RecoveryProvider for FileBackupProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: "manual",
            atomic_snapshot: false,
            caveats: vec![
                "not a filesystem snapshot: no atomic point-in-time and no open-file consistency",
            ],
        }
    }

    fn create_checkpoint(
        &self,
        target_root: &Path,
        backup_root: &Path,
        note: Option<&str>,
    ) -> Result<Checkpoint, String> {
        if !target_root.is_dir() {
            return Err(format!(
                "target_root `{}` is not a directory",
                target_root.display()
            ));
        }
        let mono = clock::mono_ns().unwrap_or(0);
        let wall = clock::wall_now_rfc3339_ms();
        let cp_id = format!("cp-file-{}-{mono}", std::process::id());
        let dest = backup_root.join(&cp_id);
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        let count = copy_dir_files(target_root, &dest)?;

        Ok(Checkpoint {
            checkpoint_id: cp_id,
            session_id: None,
            operation_id: None,
            created_wall: wall,
            kind: "manual".into(),
            reference: Some(dest.to_string_lossy().into_owned()),
            file_count: Some(i64::try_from(count).unwrap_or(i64::MAX)),
            note: Some(match note {
                Some(n) => format!("{n} | {}", self.capabilities().caveats[0]),
                None => self.capabilities().caveats[0].to_string(),
            }),
        })
    }

    fn inspect(&self, cp: &Checkpoint) -> Result<String, String> {
        Ok(format!(
            "checkpoint={} kind={} ref={:?} caveats={:?}",
            cp.checkpoint_id,
            cp.kind,
            cp.reference,
            self.capabilities().caveats
        ))
    }

    fn preview_restore(&self, cp: &Checkpoint, target_root: &Path) -> Result<RestoreScope, String> {
        let src_dir = cp
            .reference
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| "checkpoint missing backup directory reference".to_string())?;
        if !src_dir.is_dir() {
            return Err(format!("backup directory missing: {}", src_dir.display()));
        }
        let rel_files = collect_relative_files(src_dir, src_dir)?;
        let mut would_overwrite = Vec::new();
        for rel in &rel_files {
            if target_root.join(rel).exists() {
                would_overwrite.push(rel.clone());
            }
        }
        let scope_hash = compute_scope_hash(&cp.checkpoint_id, target_root, &rel_files);
        Ok(RestoreScope {
            checkpoint_id: cp.checkpoint_id.clone(),
            target_root: target_root.to_path_buf(),
            files: rel_files,
            would_overwrite,
            scope_hash,
        })
    }

    fn restore(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<(), String> {
        let src_dir = cp
            .reference
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| "checkpoint missing reference".to_string())?;
        for rel in &scope.files {
            let src = src_dir.join(rel);
            let dst = scope.target_root.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::copy(&src, &dst)
                .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
        }
        Ok(())
    }

    fn verify(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<VerifyReport, String> {
        let src_dir = cp
            .reference
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| "checkpoint missing reference".to_string())?;
        let mut mismatches = Vec::new();
        let mut checked = Vec::new();
        for rel in &scope.files {
            let src_bytes = fs::read(src_dir.join(rel)).map_err(|e| e.to_string())?;
            let dst_path = scope.target_root.join(rel);
            match fs::read(&dst_path) {
                Ok(dst_bytes) if dst_bytes == src_bytes => checked.push(rel.clone()),
                Ok(_) => mismatches.push(format!("content mismatch: {}", rel.display())),
                Err(e) => mismatches.push(format!("missing/unreadable {}: {e}", rel.display())),
            }
        }
        Ok(VerifyReport {
            verified: mismatches.is_empty(),
            checked_files: checked,
            mismatches,
        })
    }
}

/// Git commit-level provider (P5-03).
pub struct GitProvider;

impl RecoveryProvider for GitProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: "git",
            atomic_snapshot: false,
            caveats: vec![
                "only restores tracked files up to recorded commit; untracked files are excluded",
            ],
        }
    }

    fn create_checkpoint(
        &self,
        target_root: &Path,
        _backup_root: &Path,
        note: Option<&str>,
    ) -> Result<Checkpoint, String> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(target_root)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| format!("git rev-parse: {e}"))?;
        if !out.status.success() {
            return Err("not a git repository with a HEAD commit".into());
        }
        let rev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let tracked = Command::new("git")
            .args(["-C"])
            .arg(target_root)
            .args(["ls-files"])
            .output()
            .map_err(|e| format!("git ls-files: {e}"))?;
        let count = String::from_utf8_lossy(&tracked.stdout).lines().count();
        let mono = clock::mono_ns().unwrap_or(0);
        Ok(Checkpoint {
            checkpoint_id: format!("cp-git-{}-{mono}", std::process::id()),
            session_id: None,
            operation_id: None,
            created_wall: clock::wall_now_rfc3339_ms(),
            kind: "git".into(),
            reference: Some(rev),
            file_count: Some(i64::try_from(count).unwrap_or(i64::MAX)),
            note: Some(match note {
                Some(n) => format!("{n} | {}", self.capabilities().caveats[0]),
                None => self.capabilities().caveats[0].to_string(),
            }),
        })
    }

    fn inspect(&self, cp: &Checkpoint) -> Result<String, String> {
        Ok(format!(
            "checkpoint={} kind=git commit={:?} caveats={:?}",
            cp.checkpoint_id,
            cp.reference,
            self.capabilities().caveats
        ))
    }

    fn preview_restore(&self, cp: &Checkpoint, target_root: &Path) -> Result<RestoreScope, String> {
        let rev = cp
            .reference
            .as_deref()
            .ok_or_else(|| "missing git commit reference".to_string())?;
        let out = Command::new("git")
            .args(["-C"])
            .arg(target_root)
            .args(["ls-tree", "-r", "--name-only", rev])
            .output()
            .map_err(|e| format!("git ls-tree: {e}"))?;
        if !out.status.success() {
            return Err(format!("git ls-tree {rev} failed"));
        }
        let files: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        let would_overwrite: Vec<PathBuf> = files
            .iter()
            .filter(|f| target_root.join(f).exists())
            .cloned()
            .collect();
        let scope_hash = compute_scope_hash(&cp.checkpoint_id, target_root, &files);
        Ok(RestoreScope {
            checkpoint_id: cp.checkpoint_id.clone(),
            target_root: target_root.to_path_buf(),
            files,
            would_overwrite,
            scope_hash,
        })
    }

    fn restore(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<(), String> {
        let rev = cp
            .reference
            .as_deref()
            .ok_or_else(|| "missing git commit reference".to_string())?;
        let out = Command::new("git")
            .args(["-C"])
            .arg(&scope.target_root)
            .args(["checkout", rev, "--", "."])
            .output()
            .map_err(|e| format!("git checkout: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    }

    fn verify(&self, _cp: &Checkpoint, scope: &RestoreScope) -> Result<VerifyReport, String> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&scope.target_root)
            .args(["diff", "--name-only"])
            .output()
            .map_err(|e| format!("git diff: {e}"))?;
        let dirty: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| format!("unrestored diff: {l}"))
            .collect();
        Ok(VerifyReport {
            verified: dirty.is_empty(),
            checked_files: scope.files.clone(),
            mismatches: dirty,
        })
    }
}

fn find_host_binary(name: &str) -> Option<PathBuf> {
    for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Sandboxed Btrfs image provider (Phase 5: P5-04, D2).
///
/// Builds isolated `.img` Btrfs images via `mkfs.btrfs -r` and inspects/restores via
/// `btrfs inspect-internal dump-super` + `btrfs restore` without requiring `CAP_SYS_ADMIN`
/// (`mount -o loop`) and without ever mutating host root/home Btrfs subvolumes.
pub struct BtrfsProvider;

impl BtrfsProvider {
    fn require_tools() -> Result<(PathBuf, PathBuf), String> {
        let mkfs = find_host_binary("mkfs.btrfs")
            .ok_or_else(|| "btrfs-progs unavailable on host (`mkfs.btrfs` missing)".to_string())?;
        let btrfs = find_host_binary("btrfs")
            .ok_or_else(|| "btrfs-progs unavailable on host (`btrfs` missing)".to_string())?;
        Ok((mkfs, btrfs))
    }

    fn run_btrfs_restore(btrfs: &Path, img: &Path, out_dir: &Path) -> Result<(), String> {
        if out_dir.exists() {
            fs::remove_dir_all(out_dir).map_err(|e| e.to_string())?;
        }
        fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
        let out = Command::new(btrfs)
            .args(["restore", "-o"])
            .arg(img)
            .arg(out_dir)
            .output()
            .map_err(|e| format!("exec `btrfs restore`: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "`btrfs restore` failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    }

    fn img_path(cp: &Checkpoint) -> Result<&Path, String> {
        let p = cp
            .reference
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| "btrfs checkpoint missing image reference".to_string())?;
        if !p.is_file() {
            return Err(format!("btrfs image missing: {}", p.display()));
        }
        Ok(p)
    }

    fn temp_staging_dir(img: &Path, tag: &str) -> PathBuf {
        let parent = img
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("/tmp"));
        let mono = clock::mono_ns().unwrap_or(0);
        parent.join(format!(".li-btrfs-{tag}-{}-{mono}", std::process::id()))
    }
}

impl RecoveryProvider for BtrfsProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: "btrfs",
            atomic_snapshot: true,
            caveats: vec![
                "sandbox image mode (`mkfs.btrfs -r` + `btrfs restore`) reads directory tree without kernel freeze; unprivileged users cannot `mount -o loop` or invoke privileged host subvolume operations without CAP_SYS_ADMIN",
                "snapshots/images do not protect against underlying storage hardware failure",
            ],
        }
    }

    fn create_checkpoint(
        &self,
        target_root: &Path,
        backup_root: &Path,
        note: Option<&str>,
    ) -> Result<Checkpoint, String> {
        if !target_root.is_dir() {
            return Err(format!(
                "target_root `{}` is not a directory",
                target_root.display()
            ));
        }
        let (mkfs, btrfs) = Self::require_tools()?;
        fs::create_dir_all(backup_root).map_err(|e| e.to_string())?;
        let mono = clock::mono_ns().unwrap_or(0);
        let wall = clock::wall_now_rfc3339_ms();
        let cp_id = format!("cp-btrfs-{}-{mono}", std::process::id());
        let img = backup_root.join(format!("{cp_id}.img"));

        let mk_out = Command::new(&mkfs)
            .args(["-q", "-r"])
            .arg(target_root)
            .arg(&img)
            .output()
            .map_err(|e| format!("exec `mkfs.btrfs`: {e}"))?;
        if !mk_out.status.success() {
            return Err(format!(
                "`mkfs.btrfs` failed ({}): {}",
                mk_out.status,
                String::from_utf8_lossy(&mk_out.stderr)
            ));
        }

        // Verify written superblock using `btrfs inspect-internal dump-super`.
        let sup = Command::new(&btrfs)
            .args(["inspect-internal", "dump-super"])
            .arg(&img)
            .output()
            .map_err(|e| format!("exec `btrfs inspect-internal`: {e}"))?;
        if !sup.status.success() {
            return Err(format!(
                "`btrfs inspect-internal dump-super` failed: {}",
                String::from_utf8_lossy(&sup.stderr)
            ));
        }
        let count = collect_relative_files(target_root, target_root)?.len();

        Ok(Checkpoint {
            checkpoint_id: cp_id,
            session_id: None,
            operation_id: None,
            created_wall: wall,
            kind: "btrfs".into(),
            reference: Some(img.to_string_lossy().into_owned()),
            file_count: Some(i64::try_from(count).unwrap_or(i64::MAX)),
            note: Some(match note {
                Some(n) => format!("{n} | {}", self.capabilities().caveats[0]),
                None => self.capabilities().caveats[0].to_string(),
            }),
        })
    }

    fn inspect(&self, cp: &Checkpoint) -> Result<String, String> {
        let (_, btrfs) = Self::require_tools()?;
        let img = Self::img_path(cp)?;
        let out = Command::new(&btrfs)
            .args(["inspect-internal", "dump-super"])
            .arg(img)
            .output()
            .map_err(|e| format!("exec `btrfs inspect-internal`: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "dump-super failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let summary: String = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.starts_with("magic") || l.starts_with("fsid") || l.starts_with("csum\t"))
            .collect::<Vec<_>>()
            .join(" | ");
        Ok(format!(
            "checkpoint={} kind=btrfs img={} super=({})",
            cp.checkpoint_id,
            img.display(),
            summary
        ))
    }

    fn preview_restore(&self, cp: &Checkpoint, target_root: &Path) -> Result<RestoreScope, String> {
        let (_, btrfs) = Self::require_tools()?;
        let img = Self::img_path(cp)?;
        let staging = Self::temp_staging_dir(img, "preview");
        Self::run_btrfs_restore(&btrfs, img, &staging)?;
        let rel_files = collect_relative_files(&staging, &staging)?;
        let _ = fs::remove_dir_all(&staging);

        let mut would_overwrite = Vec::new();
        for rel in &rel_files {
            if target_root.join(rel).exists() {
                would_overwrite.push(rel.clone());
            }
        }
        let scope_hash = compute_scope_hash(&cp.checkpoint_id, target_root, &rel_files);
        Ok(RestoreScope {
            checkpoint_id: cp.checkpoint_id.clone(),
            target_root: target_root.to_path_buf(),
            files: rel_files,
            would_overwrite,
            scope_hash,
        })
    }

    fn restore(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<(), String> {
        let (_, btrfs) = Self::require_tools()?;
        let img = Self::img_path(cp)?;
        let staging = Self::temp_staging_dir(img, "restore");
        let res = (|| {
            Self::run_btrfs_restore(&btrfs, img, &staging)?;
            for rel in &scope.files {
                let src = staging.join(rel);
                let dst = scope.target_root.join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::copy(&src, &dst)
                    .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&staging);
        res
    }

    fn verify(&self, cp: &Checkpoint, scope: &RestoreScope) -> Result<VerifyReport, String> {
        let (_, btrfs) = Self::require_tools()?;
        let img = Self::img_path(cp)?;
        let staging = Self::temp_staging_dir(img, "verify");
        Self::run_btrfs_restore(&btrfs, img, &staging)?;
        let mut mismatches = Vec::new();
        for rel in &scope.files {
            let expected = fs::read(staging.join(rel));
            let actual = fs::read(scope.target_root.join(rel));
            match (expected, actual) {
                (Ok(a), Ok(b)) if a == b => {}
                (Ok(_), Ok(_)) => mismatches.push(format!("content mismatch: {}", rel.display())),
                (Err(e), _) | (_, Err(e)) => {
                    mismatches.push(format!("read error `{}`: {e}", rel.display()));
                }
            }
        }
        let _ = fs::remove_dir_all(&staging);
        Ok(VerifyReport {
            verified: mismatches.is_empty(),
            checked_files: scope.files.clone(),
            mismatches,
        })
    }
}

/// LVM thin/snapshot provider with host capability probing (Phase 5: P5-05, D2).
pub struct LvmProvider;

impl LvmProvider {
    /// Checks whether both `lvs` and `lvcreate` binaries exist on this host.
    pub fn probe_tools() -> Result<(PathBuf, PathBuf), String> {
        let lvs = find_host_binary("lvs");
        let lvcreate = find_host_binary("lvcreate");
        match (lvs, lvcreate) {
            (Some(a), Some(b)) => Ok((a, b)),
            (a, b) => Err(format!(
                "lvm2 tools unavailable on host (lvs={}, lvcreate={}); refusing per capability probe (C12/P5-05)",
                a.is_some(),
                b.is_some()
            )),
        }
    }
}

impl RecoveryProvider for LvmProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: "lvm",
            atomic_snapshot: true,
            caveats: vec![
                "requires lvm2 CLI tools (`lvs`, `lvcreate`) and device-mapper privileges (typically CAP_SYS_ADMIN)",
                "absent tools cause explicit capability-probe degradation rather than guessing or mutating host volumes",
            ],
        }
    }

    fn create_checkpoint(
        &self,
        _target_root: &Path,
        _backup_root: &Path,
        _note: Option<&str>,
    ) -> Result<Checkpoint, String> {
        let (lvs, lvcreate) = Self::probe_tools()?;
        Err(format!(
            "lvm tools found ({}, {}), but unconfigured/non-privileged thinpool creation is disabled by safety policy",
            lvs.display(),
            lvcreate.display()
        ))
    }

    fn inspect(&self, cp: &Checkpoint) -> Result<String, String> {
        let _ = Self::probe_tools()?;
        Ok(format!(
            "checkpoint={} kind=lvm ref={:?}",
            cp.checkpoint_id, cp.reference
        ))
    }

    fn preview_restore(
        &self,
        _cp: &Checkpoint,
        _target_root: &Path,
    ) -> Result<RestoreScope, String> {
        let _ = Self::probe_tools()?;
        Err("lvm preview_restore requires active thin-volume configuration".into())
    }

    fn restore(&self, _cp: &Checkpoint, _scope: &RestoreScope) -> Result<(), String> {
        let _ = Self::probe_tools()?;
        Err("lvm restore requires CAP_SYS_ADMIN and configured volume group".into())
    }

    fn verify(&self, _cp: &Checkpoint, _scope: &RestoreScope) -> Result<VerifyReport, String> {
        let _ = Self::probe_tools()?;
        Err("lvm verify requires mounted volume snapshot".into())
    }
}

/// Runs the audited restore pipeline (`preview -> approval check -> restore -> verify`),
/// journaling `ApprovalGranted`, `RecoveryStarted`, and `RecoveryCompleted` (P5-06).
pub fn execute_restore(
    provider: &dyn RecoveryProvider,
    cp: &Checkpoint,
    scope: &RestoreScope,
    approval: &Approval,
    journal: &Journal,
) -> Result<VerifyReport, String> {
    if approval.scope_hash != scope.scope_hash {
        return Err("approval scope_hash does not match previewed scope".into());
    }
    let mono = clock::mono_ns().unwrap_or(0);
    let wall = clock::wall_now_rfc3339_ms();

    let emit = |kind: &str, payload: serde_json::Map<String, serde_json::Value>| {
        if let Ok(eid) = EventId::new(format!("rec-{kind}-{}-{mono}", std::process::id()))
            && let Ok(et) = EventType::new(kind)
            && let Ok(ev) = Event::new(
                eid,
                Source::Recovery,
                None,
                0,
                mono,
                &wall,
                et,
                payload,
                EvidenceStatus::Observed,
            )
        {
            let _ = journal.event(&ev);
        }
    };

    let mut ap_payload = serde_json::Map::new();
    ap_payload.insert(
        "approval_id".into(),
        serde_json::json!(approval.approval_id),
    );
    ap_payload.insert("scope_hash".into(), serde_json::json!(approval.scope_hash));
    ap_payload.insert("uid".into(), serde_json::json!(approval.uid));
    ap_payload.insert(
        "auth_method".into(),
        serde_json::json!(approval.auth_method),
    );
    emit("ApprovalGranted", ap_payload);

    let mut st_payload = serde_json::Map::new();
    st_payload.insert("provider".into(), serde_json::json!(cp.kind));
    st_payload.insert("checkpoint".into(), serde_json::json!(cp.checkpoint_id));
    emit("RecoveryStarted", st_payload);

    match provider.restore(cp, scope) {
        Ok(()) => {
            let report = provider.verify(cp, scope)?;
            let mut end_payload = serde_json::Map::new();
            end_payload.insert("provider".into(), serde_json::json!(cp.kind));
            end_payload.insert("checkpoint".into(), serde_json::json!(cp.checkpoint_id));
            end_payload.insert("verified".into(), serde_json::json!(report.verified));
            end_payload.insert("mismatches".into(), serde_json::json!(report.mismatches));
            emit("RecoveryCompleted", end_payload);
            let _ = journal.flush();
            Ok(report)
        }
        Err(err) => {
            let mut fail_payload = serde_json::Map::new();
            fail_payload.insert("provider".into(), serde_json::json!(cp.kind));
            fail_payload.insert("checkpoint".into(), serde_json::json!(cp.checkpoint_id));
            fail_payload.insert("verified".into(), serde_json::json!(false));
            fail_payload.insert("error".into(), serde_json::json!(err));
            emit("RecoveryCompleted", fail_payload);
            let _ = journal.flush();
            Err(err)
        }
    }
}

fn copy_dir_files(src: &Path, dst: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut stack = vec![src.to_path_buf()];
    while let Some(cur) = stack.pop() {
        for entry in fs::read_dir(&cur).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if entry.file_type().is_ok_and(|t| t.is_file()) {
                let rel = path.strip_prefix(src).map_err(|e| e.to_string())?;
                let out = dst.join(rel);
                if let Some(p) = out.parent() {
                    fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                fs::copy(&path, &out).map_err(|e| e.to_string())?;
                count += 1;
            }
        }
    }
    Ok(count)
}

fn collect_relative_files(root: &Path, cur: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(cur).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(collect_relative_files(root, &p)?);
        } else if entry.file_type().is_ok_and(|t| t.is_file()) {
            out.push(
                p.strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_path_buf(),
            );
        }
    }
    out.sort();
    Ok(out)
}

fn compute_scope_hash(cp_id: &str, target: &Path, files: &[PathBuf]) -> String {
    let mut data = format!("{cp_id}|{}|", target.display());
    for f in files {
        data.push_str(&f.to_string_lossy());
        data.push('\n');
    }
    clock::fnv1a_hex(data.as_bytes())
}
