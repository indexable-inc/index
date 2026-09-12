//! Child supervision for the production build worker. The host owns processes,
//! channels and weak goal references; this state owns admission and wake policy.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Resource class used for independent build and transfer admission limits.
pub enum Category {
    Build,
    Substitution,
    Administration,
}

#[derive(Clone, Copy, Debug, Default)]
/// Worker policy. Zero build slots disables local builds; zero substitution
/// slots still permits one transfer. Zero timeouts are disabled, while a zero
/// lock polling interval is rounded up to one second.
pub struct Config {
    pub max_builds: u64,
    pub max_substitutions: u64,
    pub silent_seconds: u64,
    pub build_seconds: u64,
    pub monitor_progress: bool,
    pub poll_seconds: u64,
}

#[derive(Debug)]
struct Child {
    category: Category,
    occupies_slot: bool,
    respect_timeouts: bool,
    started_ms: u64,
    last_output_ms: u64,
    timeout_sent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// The first applicable timeout observed during a poll.
pub enum TimeoutKind {
    Silent,
    NoProgress,
    Build,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A timeout effect for the host to deliver to its child process.
pub struct Timeout {
    pub kind: TimeoutKind,
    pub seconds: u64,
}

#[derive(Debug)]
/// Single-worker child supervision. Timestamps are monotonically increasing
/// elapsed milliseconds from one clock, and child IDs belong to this instance.
/// This object does not own, terminate, or join host processes.
pub struct Scheduler {
    config: Config,
    children: BTreeMap<u64, Child>,
    next_child: u64,
    builds: u64,
    substitutions: u64,
    lock_deadline_ms: Option<u128>,
    last_now_ms: u64,
}

impl Scheduler {
    /// Start with no registered children or pending lock wakeups.
    pub fn new(mut config: Config) -> Self {
        // Zero used to round up to one second in the host poll loop. Preserve
        // a finite retry interval rather than spinning on a contended lock.
        config.poll_seconds = config.poll_seconds.max(1);
        Self {
            config,
            children: BTreeMap::new(),
            next_child: 1,
            builds: 0,
            substitutions: 0,
            lock_deadline_ms: None,
            last_now_ms: 0,
        }
    }

    fn clock(&mut self, now_ms: u64) -> Result<(), &'static str> {
        if now_ms < self.last_now_ms {
            return Err("build scheduler monotonic clock moved backwards");
        }
        self.last_now_ms = now_ms;
        Ok(())
    }

    /// Occupied slots, excluding remote builds and administrative children.
    pub fn running(&self, category: Category) -> u64 {
        match category {
            Category::Build => self.builds,
            Category::Substitution => self.substitutions,
            Category::Administration => 0,
        }
    }

    /// Whether the host may admit another child of this resource class.
    pub fn slot_available(&self, category: Category) -> bool {
        match category {
            Category::Build => self.builds < self.config.max_builds,
            // A zero substitution limit still permits one transfer. Admission
            // and wakeup must use the same limit or a waiting transfer can stall.
            Category::Substitution => self.substitutions < self.config.max_substitutions.max(1),
            Category::Administration => true,
        }
    }

    /// Register a child that the host has started. Admission is separate from
    /// registration: the host checks `slot_available` before launching work.
    /// Returned IDs are never reused within this scheduler.
    pub fn start(
        &mut self,
        category: Category,
        occupies_slot: bool,
        respect_timeouts: bool,
        now_ms: u64,
    ) -> Result<u64, &'static str> {
        self.clock(now_ms)?;
        let next = self
            .next_child
            .checked_add(1)
            .ok_or("build child IDs exhausted")?;
        if occupies_slot {
            match category {
                Category::Build => {
                    self.builds = self
                        .builds
                        .checked_add(1)
                        .ok_or("build slot count overflow")?;
                }
                Category::Substitution => {
                    self.substitutions = self
                        .substitutions
                        .checked_add(1)
                        .ok_or("substitution slot count overflow")?;
                }
                Category::Administration => {}
            }
        }
        let id = self.next_child;
        self.next_child = next;
        self.children.insert(
            id,
            Child {
                category,
                occupies_slot,
                respect_timeouts,
                started_ms: now_ms,
                last_output_ms: now_ms,
                timeout_sent: false,
            },
        );
        Ok(id)
    }

    /// Cleanup can run from both a normal completion and a destructor. The
    /// original category is retained here; no virtual call on a dying goal is
    /// needed, and a repeated cleanup never releases a second slot.
    pub fn stop(&mut self, id: u64, wake_sleepers: bool) -> bool {
        let Some(child) = self.children.remove(&id) else {
            return false;
        };
        if child.occupies_slot {
            match child.category {
                Category::Build => self.builds -= 1,
                Category::Substitution => self.substitutions -= 1,
                Category::Administration => {}
            }
        }
        wake_sleepers
    }

    /// Reset the silence clock after reading output from a registered child.
    pub fn note_output(&mut self, id: u64, now_ms: u64) -> Result<(), &'static str> {
        self.clock(now_ms)?;
        self.children
            .get_mut(&id)
            .ok_or("unknown build child")?
            .last_output_ms = now_ms;
        Ok(())
    }

    /// Whether the host should sample this child's process activity.
    pub fn monitors_progress(&self, id: u64) -> Result<bool, &'static str> {
        let child = self.children.get(&id).ok_or("unknown build child")?;
        Ok(self.config.monitor_progress && child.respect_timeouts && !child.timeout_sent)
    }

    fn deadline(start_ms: u64, seconds: u64) -> u128 {
        u128::from(start_ms) + u128::from(seconds) * 1_000
    }

    /// The result is a relative poll timeout. None means unbounded, zero means
    /// immediately due. Cap to signed poll(2)'s range; longer waits replan later.
    pub fn wait_plan(
        &mut self,
        now_ms: u64,
        lock_waiters: bool,
        gc_poll: bool,
    ) -> Result<Option<u32>, &'static str> {
        self.clock(now_ms)?;
        if lock_waiters {
            self.lock_deadline_ms
                .get_or_insert_with(|| Self::deadline(now_ms, self.config.poll_seconds));
        } else {
            self.lock_deadline_ms = None;
        }

        let mut deadline = self.lock_deadline_ms;
        let mut include = |candidate| {
            deadline = Some(deadline.map_or(candidate, |current| current.min(candidate)));
        };
        if gc_poll {
            include(Self::deadline(now_ms, 10));
        }
        for child in self.children.values() {
            if !child.respect_timeouts || child.timeout_sent {
                continue;
            }
            if self.config.silent_seconds != 0 {
                include(Self::deadline(
                    child.last_output_ms,
                    self.config.silent_seconds,
                ));
            }
            if self.config.build_seconds != 0 {
                include(Self::deadline(child.started_ms, self.config.build_seconds));
            }
            if self.config.monitor_progress {
                include(Self::deadline(now_ms, 1));
            }
        }
        deadline
            .map(|deadline| {
                let remaining = deadline.saturating_sub(u128::from(now_ms));
                u32::try_from(remaining.min(2_147_483_647))
                    .map_err(|_| "unrepresentable build poll timeout")
            })
            .transpose()
    }

    /// Select at most one timeout effect per child. The host supplies an
    /// optional expired no-progress duration from its process activity sample.
    pub fn inspect_child(
        &mut self,
        id: u64,
        now_ms: u64,
        busy: bool,
        no_progress_seconds: Option<u64>,
    ) -> Result<Option<Timeout>, &'static str> {
        self.clock(now_ms)?;
        let child = self.children.get_mut(&id).ok_or("unknown build child")?;
        if !busy || !child.respect_timeouts || child.timeout_sent {
            return Ok(None);
        }
        let now = u128::from(now_ms);
        // Preserve deliberate precedence when multiple failures become visible
        // in the same poll: silence, observed lack of progress, total duration.
        let timeout = if self.config.silent_seconds != 0
            && now >= Self::deadline(child.last_output_ms, self.config.silent_seconds)
        {
            Some(Timeout {
                kind: TimeoutKind::Silent,
                seconds: self.config.silent_seconds,
            })
        } else if self.config.monitor_progress
            && no_progress_seconds.is_some_and(|seconds| seconds != 0)
        {
            no_progress_seconds.map(|seconds| Timeout {
                kind: TimeoutKind::NoProgress,
                seconds,
            })
        } else if self.config.build_seconds != 0
            && now >= Self::deadline(child.started_ms, self.config.build_seconds)
        {
            Some(Timeout {
                kind: TimeoutKind::Build,
                seconds: self.config.build_seconds,
            })
        } else {
            None
        };
        child.timeout_sent = timeout.is_some();
        Ok(timeout)
    }

    /// Return whether the host should wake its lock waiters, consuming that
    /// pending wakeup. A subsequent wait plan arms the next retry interval.
    pub fn finish_poll(&mut self, now_ms: u64, lock_waiters: bool) -> Result<bool, &'static str> {
        self.clock(now_ms)?;
        if !lock_waiters {
            self.lock_deadline_ms = None;
            return Ok(false);
        }
        if self
            .lock_deadline_ms
            .is_some_and(|deadline| u128::from(now_ms) >= deadline)
        {
            self.lock_deadline_ms = None;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_deadline_beats_lock_poll_and_gc() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            build_seconds: 2,
            poll_seconds: 60,
            ..Config::default()
        });
        scheduler.start(Category::Build, true, true, 0)?;
        assert_eq!(scheduler.wait_plan(500, true, true)?, Some(1_500));
        assert!(!scheduler.finish_poll(2_000, true)?);
        assert_eq!(scheduler.wait_plan(2_000, true, true)?, Some(0));
        Ok(())
    }

    #[test]
    fn zero_limits_disable_builds_but_allow_one_substitution() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config::default());
        assert!(!scheduler.slot_available(Category::Build));
        assert!(scheduler.slot_available(Category::Administration));
        assert!(scheduler.slot_available(Category::Substitution));
        let id = scheduler.start(Category::Substitution, true, false, 0)?;
        assert!(!scheduler.slot_available(Category::Substitution));
        assert!(scheduler.stop(id, true));
        assert!(scheduler.slot_available(Category::Substitution));
        assert!(!scheduler.stop(id, true));
        assert_eq!(scheduler.running(Category::Substitution), 0);
        Ok(())
    }

    #[test]
    fn cancellations_unregister_once_and_preserve_other_categories() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            max_builds: 1,
            ..Config::default()
        });
        let build = scheduler.start(Category::Build, true, true, 0)?;
        let remote = scheduler.start(Category::Build, false, false, 0)?;
        let transfer = scheduler.start(Category::Substitution, true, false, 0)?;
        let administrative = scheduler.start(Category::Administration, true, false, 0)?;
        assert!(!scheduler.slot_available(Category::Build));
        assert!(!scheduler.stop(remote, false));
        assert_eq!(scheduler.running(Category::Build), 1);
        assert!(scheduler.stop(build, true));
        assert!(!scheduler.stop(build, true));
        assert_eq!(scheduler.running(Category::Build), 0);
        assert_eq!(scheduler.running(Category::Substitution), 1);
        assert!(scheduler.stop(transfer, true));
        assert!(scheduler.stop(administrative, true));
        assert!(scheduler.children.is_empty());
        assert_eq!(scheduler.wait_plan(1, false, false)?, None);
        // Stale IDs cannot remove the next child, including address-reuse cases
        // in the C++ host bindings.
        let next = scheduler.start(Category::Build, true, true, 1)?;
        assert_ne!(build, next);
        assert!(!scheduler.stop(build, true));
        assert_eq!(scheduler.running(Category::Build), 1);
        assert!(scheduler.stop(next, true));
        Ok(())
    }

    #[test]
    fn output_resets_silence_but_not_total_duration() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            silent_seconds: 2,
            build_seconds: 3,
            ..Config::default()
        });
        let id = scheduler.start(Category::Build, true, true, 0)?;
        scheduler.note_output(id, 1_500)?;
        assert_eq!(scheduler.wait_plan(1_500, false, false)?, Some(1_500));
        assert_eq!(scheduler.inspect_child(id, 2_000, true, None)?, None);
        assert_eq!(
            scheduler.inspect_child(id, 3_000, true, None)?,
            Some(Timeout {
                kind: TimeoutKind::Build,
                seconds: 3
            })
        );
        assert_eq!(scheduler.inspect_child(id, 4_000, true, None)?, None);
        Ok(())
    }

    #[test]
    fn timeout_priority_and_remote_exemption() -> Result<(), &'static str> {
        let config = Config {
            silent_seconds: 1,
            build_seconds: 1,
            monitor_progress: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(config);
        let id = scheduler.start(Category::Build, true, true, 0)?;
        let remote = scheduler.start(Category::Build, false, false, 0)?;
        assert!(scheduler.monitors_progress(id)?);
        assert!(!scheduler.monitors_progress(remote)?);
        assert_eq!(scheduler.inspect_child(remote, 1_000, true, Some(1))?, None);
        assert_eq!(
            scheduler.inspect_child(id, 1_000, true, Some(1))?,
            Some(Timeout {
                kind: TimeoutKind::Silent,
                seconds: 1
            })
        );
        assert!(!scheduler.monitors_progress(id)?);

        let mut scheduler = Scheduler::new(Config {
            silent_seconds: 0,
            ..config
        });
        let id = scheduler.start(Category::Build, true, true, 0)?;
        assert_eq!(
            scheduler.inspect_child(id, 1_000, true, Some(2))?,
            Some(Timeout {
                kind: TimeoutKind::NoProgress,
                seconds: 2
            })
        );
        Ok(())
    }

    #[test]
    fn lock_wait_is_armed_once_and_rearmed_after_wake() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            poll_seconds: 5,
            ..Config::default()
        });
        assert_eq!(scheduler.wait_plan(10, true, false)?, Some(5_000));
        assert_eq!(scheduler.wait_plan(1_010, true, false)?, Some(4_000));
        assert!(!scheduler.finish_poll(5_009, true)?);
        assert!(scheduler.finish_poll(5_010, true)?);
        assert!(!scheduler.finish_poll(5_010, true)?);
        assert_eq!(scheduler.wait_plan(5_010, true, false)?, Some(5_000));
        assert_eq!(scheduler.wait_plan(5_011, false, false)?, None);
        assert_eq!(scheduler.wait_plan(5_012, true, false)?, Some(5_000));
        Ok(())
    }

    #[test]
    fn deadlines_do_not_overflow_or_turn_into_infinite_poll() -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            build_seconds: u64::MAX,
            ..Config::default()
        });
        let id = scheduler.start(Category::Build, true, true, u64::MAX - 1)?;
        assert_eq!(
            scheduler.wait_plan(u64::MAX, false, false)?,
            Some(2_147_483_647)
        );
        assert_eq!(scheduler.inspect_child(id, u64::MAX, true, None)?, None);
        assert!(scheduler.wait_plan(0, false, false).is_err());
        Ok(())
    }

    #[test]
    fn zero_lock_interval_does_not_spin_and_completed_children_do_not_timeout()
    -> Result<(), &'static str> {
        let mut scheduler = Scheduler::new(Config {
            build_seconds: 1,
            ..Config::default()
        });
        assert_eq!(scheduler.wait_plan(0, true, false)?, Some(1_000));
        let child = scheduler.start(Category::Build, true, true, 0)?;
        assert_eq!(scheduler.inspect_child(child, 2_000, false, None)?, None);
        assert!(!scheduler.stop(child, false));
        assert_eq!(scheduler.wait_plan(2_000, false, false)?, None);
        Ok(())
    }
}
