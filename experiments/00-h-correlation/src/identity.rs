//! Process identity and the `(pid, starttime)` collision problem.
//!
//! 0-E established that `/proc/<pid>/stat` field 22 (`starttime`) is measured in
//! clock ticks, `CLK_TCK = 100` on this host, i.e. **10 ms granularity**. Two
//! different processes can therefore present the *same* `(pid, starttime)` pair.
//! This module never assumes the pair is unique; it detects the collision and
//! reports it so the correlator can degrade instead of lying.

pub const CLK_TCK: u64 = 100;
pub const TICK_NS: u64 = 1_000_000_000 / CLK_TCK; // 10_000_000

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcKey {
    pub pid: u32,
    pub starttime: u64,
}

impl ProcKey {
    pub fn new(pid: u32, starttime: u64) -> Self {
        Self { pid, starttime }
    }

    /// The tick window in which a process with this `starttime` begun:
    /// `[starttime * TICK_NS, (starttime+1) * TICK_NS)`.
    pub fn start_window_ns(&self) -> (u64, u64) {
        let lo = self.starttime.saturating_mul(TICK_NS);
        (lo, lo + TICK_NS)
    }

    pub fn as_str(&self) -> String {
        format!("{}:{}", self.pid, self.starttime)
    }
}

/// One observation of a live (or just-dead) process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub key: ProcKey,
    pub comm: String,
    pub ppid: u32,
    /// Hash of the NUL-joined `cmdline`, so a long argv does not bloat the
    /// registry but a re-exec with a different argv is still visible.
    pub cmdline_hash: u64,
    pub observed_at_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncarnationState {
    Alive,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incarnation {
    pub id: u64,
    pub key: ProcKey,
    pub comm: String,
    pub cmdline_hash: u64,
    pub ppid: u32,
    pub first_seen_ns: u64,
    pub state: IncarnationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    /// Known key, consistent description: safe to attribute.
    Stable(u64),
    /// First sight of this key.
    New(u64),
    /// Same key, incompatible descriptions: two processes are aliased.
    Collision { incarnations: Vec<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    CommDiffers {
        known: String,
        seen: String,
    },
    CmdlineDiffers,
    /// The key was seen to exit and then reappear without a tick change: with a
    /// monotonic `starttime` this means two processes shared the tick.
    ExitedThenReappeared {
        exited_at_ns: u64,
        seen_at_ns: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionReport {
    pub key: ProcKey,
    pub incarnation_ids: Vec<u64>,
    pub reason: Reason,
    /// Tick index the processes share, for the human reading the report.
    pub tick_index: u64,
}

#[derive(Debug, Default)]
pub struct ProcRegistry {
    next_id: u64,
    by_key: std::collections::HashMap<ProcKey, Vec<Incarnation>>,
    collisions: Vec<CollisionReport>,
    /// Keys that must be reported as `Unknown` for attribution.
    poisoned: std::collections::HashSet<ProcKey>,
}

impl ProcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn collision_count(&self) -> usize {
        self.collisions.len()
    }

    pub fn collisions(&self) -> &[CollisionReport] {
        &self.collisions
    }

    pub fn is_poisoned(&self, key: &ProcKey) -> bool {
        self.poisoned.contains(key)
    }

    pub fn get(&self, key: &ProcKey) -> Option<&Incarnation> {
        self.by_key.get(key).and_then(|v| v.first())
    }

    pub fn incarnation(&self, id: u64) -> Option<&Incarnation> {
        self.by_key
            .values()
            .flat_map(|v| v.iter())
            .find(|i| i.id == id)
    }

    fn alloc(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn push(&mut self, key: ProcKey, obs: &Observation) -> u64 {
        let id = self.alloc();
        self.by_key.entry(key).or_default().push(Incarnation {
            id,
            key,
            comm: obs.comm.clone(),
            cmdline_hash: obs.cmdline_hash,
            ppid: obs.ppid,
            first_seen_ns: obs.observed_at_ns,
            state: IncarnationState::Alive,
        });
        id
    }

    /// Register a live observation. Returns the incarnation id to attribute to,
    /// or a collision report when the key cannot be trusted.
    pub fn observe(&mut self, obs: &Observation) -> Attach {
        let key = obs.key;
        let existing = self.by_key.get(&key).cloned().unwrap_or_default();

        if existing.is_empty() {
            return Attach::New(self.push(key, obs));
        }

        let live = existing
            .iter()
            .find(|i| i.state == IncarnationState::Alive)
            .cloned();

        let Some(known) = live else {
            // Every previous incarnation exited, yet the key is visible again.
            // `starttime` is monotonic since boot, so the tick is being reused.
            let prev = existing
                .last()
                .cloned()
                .expect("existing is non-empty in this branch");
            let new_id = self.push(key, obs);
            let mut ids: Vec<u64> = existing.iter().map(|i| i.id).collect();
            ids.push(new_id);
            self.record_collision(
                key,
                ids.clone(),
                Reason::ExitedThenReappeared {
                    exited_at_ns: prev.first_seen_ns,
                    seen_at_ns: obs.observed_at_ns,
                },
            );
            return Attach::Collision { incarnations: ids };
        };

        let comm_differs = known.comm != obs.comm;
        let cmdline_differs = known.cmdline_hash != obs.cmdline_hash;
        if comm_differs || cmdline_differs {
            let new_id = self.push(key, obs);
            let mut ids: Vec<u64> = existing.iter().map(|i| i.id).collect();
            ids.push(new_id);
            let reason = if comm_differs {
                Reason::CommDiffers {
                    known: known.comm,
                    seen: obs.comm.clone(),
                }
            } else {
                Reason::CmdlineDiffers
            };
            self.record_collision(key, ids.clone(), reason);
            return Attach::Collision { incarnations: ids };
        }
        Attach::Stable(known.id)
    }

    pub fn mark_exited(&mut self, key: &ProcKey) {
        if let Some(list) = self.by_key.get_mut(key) {
            for inc in list.iter_mut() {
                if inc.state == IncarnationState::Alive {
                    inc.state = IncarnationState::Exited;
                }
            }
        }
    }

    fn record_collision(&mut self, key: ProcKey, ids: Vec<u64>, reason: Reason) {
        self.poisoned.insert(key);
        if !self.collisions.iter().any(|c| c.key == key) {
            self.collisions.push(CollisionReport {
                key,
                incarnation_ids: ids,
                reason,
                tick_index: key.starttime,
            });
        }
    }
}

/// The identity a correlator is allowed to use for an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorIdentity {
    /// `(pid, starttime)`, unique as far as we can tell.
    Keyed(ProcKey),
    /// Bare pid only: safe for "some process with this pid existed", never for
    /// merging two events across time.
    PidOnly(u32),
    /// The key collided; attribution must degrade to `Unknown`.
    Ambiguous(ProcKey),
    Absent,
}

impl ActorIdentity {
    pub fn is_usable_for_merge(&self) -> bool {
        matches!(self, ActorIdentity::Keyed(_))
    }

    pub fn as_str(&self) -> String {
        match self {
            ActorIdentity::Keyed(k) => format!("keyed:{}", k.as_str()),
            ActorIdentity::PidOnly(p) => format!("pid-only:{p}"),
            ActorIdentity::Ambiguous(k) => format!("ambiguous:{}", k.as_str()),
            ActorIdentity::Absent => "absent".to_string(),
        }
    }
}

/// Decide what identity an event may contribute, given the registry state.
pub fn actor_for(registry: &ProcRegistry, event: &crate::envelope::Event) -> ActorIdentity {
    match event.pid_key() {
        Some(key) if registry.is_poisoned(&key) => ActorIdentity::Ambiguous(key),
        Some(key) => ActorIdentity::Keyed(key),
        None => match event.payload_u64("pid") {
            Some(pid) => ActorIdentity::PidOnly(pid as u32),
            None => ActorIdentity::Absent,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(pid: u32, start: u64, comm: &str, cmd: u64, at: u64) -> Observation {
        Observation {
            key: ProcKey::new(pid, start),
            comm: comm.to_string(),
            ppid: 1,
            cmdline_hash: cmd,
            observed_at_ns: at,
        }
    }

    #[test]
    fn same_key_same_description_is_stable() {
        let mut r = ProcRegistry::new();
        assert!(matches!(
            r.observe(&obs(100, 5000, "bash", 7, 0)),
            Attach::New(_)
        ));
        let Attach::Stable(id) = r.observe(&obs(100, 5000, "bash", 7, 1)) else {
            panic!("expected stable");
        };
        assert_eq!(r.incarnation(id).map(|i| i.id), Some(id));
        assert_eq!(r.collision_count(), 0);
    }

    #[test]
    fn same_key_different_comm_is_collision() {
        let mut r = ProcRegistry::new();
        r.observe(&obs(100, 5000, "bash", 7, 0));
        let a = r.observe(&obs(100, 5000, "make", 8, 1));
        assert!(matches!(a, Attach::Collision { .. }));
        assert!(r.is_poisoned(&ProcKey::new(100, 5000)));
        assert_eq!(r.collision_count(), 1);
    }

    #[test]
    fn exit_then_reappear_same_tick_is_collision() {
        let mut r = ProcRegistry::new();
        let k = ProcKey::new(7, 42);
        r.observe(&obs(7, 42, "bash", 1, 0));
        r.mark_exited(&k);
        match r.observe(&obs(7, 42, "bash", 1, 2)) {
            Attach::Collision { incarnations } => assert_eq!(incarnations.len(), 2),
            other => panic!("expected collision, got {other:?}"),
        }
    }

    #[test]
    fn tick_window_is_ten_ms() {
        let k = ProcKey::new(1, 3);
        assert_eq!(k.start_window_ns(), (30_000_000, 40_000_000));
        assert_eq!(TICK_NS, 10_000_000);
    }
}
