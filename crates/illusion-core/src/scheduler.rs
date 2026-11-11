// Illusion Dynamic Chunk Halving & Hydra-Grade Adaptive Range Scheduler
// Combines IDM segment halving with Hydra's CUSUM collapse detection,
// Delta-scaled profitability checks, and atomic client-side range shrinking.

use crate::detect::{CollapseDetector, DeltaEstimator, Health};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const MIN_SPLIT_SIZE: u64 = 512 * 1024; // 512 KB minimum threshold for chunk halving
pub const STEAL_QUANTUM: u64 = 64 * 1024;   // 64 KB minimum worthwhile steal
pub const RATE_WINDOW: f64 = 0.2;           // 200 ms window to avoid TCP buffer-drain spikes
const RATE_ALPHA: f64 = 0.3;
const REPAIR_PERSIST: f64 = 3.0 * RATE_WINDOW;
const REPAIR_PERSIST_DELTAS: f64 = 6.0;
const MAX_REPAIRS_PER_TICK: usize = 4;

#[derive(Debug, Clone)]
pub struct ActiveSegment {
    pub id: usize,
    pub start: u64,
    pub current: Arc<AtomicU64>,
    pub end: Arc<AtomicU64>,
}

impl ActiveSegment {
    pub fn new(id: usize, start: u64, initial_current: u64, end: u64) -> Self {
        Self {
            id,
            start,
            current: Arc::new(AtomicU64::new(initial_current)),
            end: Arc::new(AtomicU64::new(end)),
        }
    }

    pub fn remaining(&self) -> u64 {
        let cur = self.current.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        if end >= cur {
            end - cur + 1
        } else {
            0
        }
    }

    pub fn is_finished(&self) -> bool {
        let cur = self.current.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        cur > end
    }
}

pub struct DynamicScheduler {
    pub segments: Vec<ActiveSegment>,
}

impl DynamicScheduler {
    pub fn new(segments: Vec<ActiveSegment>) -> Self {
        Self { segments }
    }

    /// Scans all active segments to find the one with largest remaining byte range.
    /// If >= MIN_SPLIT_SIZE (512 KB), splits the donor range in half atomically.
    pub fn split_largest_segment(&mut self, next_id: usize) -> Option<ActiveSegment> {
        let mut max_remaining = 0u64;
        let mut donor_idx = None;

        for (i, seg) in self.segments.iter().enumerate() {
            let rem = seg.remaining();
            if rem > max_remaining {
                max_remaining = rem;
                donor_idx = Some(i);
            }
        }

        if max_remaining < MIN_SPLIT_SIZE {
            return None;
        }

        let idx = donor_idx?;
        let donor = &self.segments[idx];
        let current = donor.current.load(Ordering::SeqCst);
        let old_end = donor.end.load(Ordering::SeqCst);

        if old_end <= current || (old_end - current) < MIN_SPLIT_SIZE {
            return None;
        }

        let half = (old_end - current) / 2;
        let midpoint = current + half;

        donor.end.store(midpoint - 1, Ordering::SeqCst);

        let new_segment = ActiveSegment::new(next_id, midpoint, midpoint, old_end);
        self.segments.push(new_segment.clone());

        Some(new_segment)
    }

    pub fn total_downloaded(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| {
                let cur = s.current.load(Ordering::Relaxed);
                if cur >= s.start {
                    cur - s.start
                } else {
                    0
                }
            })
            .sum()
    }
}

/// Action emitted by `AdaptiveRangeScheduler::tick` when stealing work from a slow or stalled worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StealAction {
    ShrinkAndSpawn {
        victim_id: usize,
        new_victim_end: u64,
        new_id: usize,
        steal_start: u64,
        steal_end: u64,
    },
}

/// Lock-free atomic state for one worker slot inside `AdaptiveRangeScheduler`.
#[derive(Debug, Clone)]
pub struct WorkerSlot {
    pub id: usize,
    pub start: u64,
    pub pos: Arc<AtomicU64>,
    pub end: Arc<AtomicU64>, // inclusive byte offset
    pub active: bool,
    pub finished: bool,
    pub stalled: bool,
    pub rate_est: f64,
    pub detector: CollapseDetector,
    pub last_progress: f64,
    pub setup_end: f64,
    rate_acc_bytes: u64,
    rate_acc_dt: f64,
}

impl WorkerSlot {
    pub fn new(id: usize, start: u64, initial_pos: u64, end: u64, now: f64) -> Self {
        Self {
            id,
            start,
            pos: Arc::new(AtomicU64::new(initial_pos)),
            end: Arc::new(AtomicU64::new(end)),
            active: initial_pos <= end,
            finished: initial_pos > end,
            stalled: false,
            rate_est: 0.0,
            detector: CollapseDetector::new(),
            last_progress: now,
            setup_end: now,
            rate_acc_bytes: 0,
            rate_acc_dt: 0.0,
        }
    }

    #[inline]
    pub fn remaining(&self) -> u64 {
        let p = self.pos.load(Ordering::Acquire);
        let e = self.end.load(Ordering::Acquire);
        if e >= p {
            e - p + 1
        } else {
            0
        }
    }

    #[inline]
    pub fn busy(&self) -> bool {
        self.active && !self.finished && self.remaining() > 0
    }

    pub fn eta(&self) -> f64 {
        let rem = self.remaining();
        if rem == 0 {
            return 0.0;
        }
        if self.rate_est <= 0.0 {
            return f64::INFINITY;
        }
        rem as f64 / self.rate_est
    }
}

/// Hydra-inspired adaptive scheduler combining wall-clock silence detection,
/// CUSUM rate-collapse grading, `DeltaEstimator`-scaled deadband (`theta`), and
/// atomic victim range shrinking (`Action::Shrink`).
pub struct AdaptiveRangeScheduler {
    pub slots: Vec<WorkerSlot>,
    pub delta_est: DeltaEstimator,
    pub max_concurrent: usize,
    pub stall_timeout_s: f64,
    repair_candidate: Option<(usize, f64)>,
}

impl AdaptiveRangeScheduler {
    pub fn new(slots: Vec<WorkerSlot>, max_concurrent: usize, stall_timeout_s: f64) -> Self {
        Self {
            slots,
            delta_est: DeltaEstimator::new(0.05),
            max_concurrent: max_concurrent.max(1),
            stall_timeout_s: stall_timeout_s.max(0.5),
            repair_candidate: None,
        }
    }

    pub fn observe_ttfb(&mut self, ttfb_s: f64) {
        self.delta_est.observe(ttfb_s);
    }

    /// Record batched byte arrivals for `slot_id` over `dt` seconds.
    pub fn on_bytes(&mut self, slot_id: usize, bytes: u64, now: f64, dt: f64) {
        let Some(slot) = self.slots.iter_mut().find(|s| s.id == slot_id) else {
            return;
        };
        if bytes == 0 {
            return;
        }
        slot.last_progress = now;
        slot.stalled = false;

        if dt > 0.0 {
            slot.rate_acc_bytes += bytes;
            slot.rate_acc_dt += dt;
            if slot.rate_acc_dt >= RATE_WINDOW {
                let sample = slot.rate_acc_bytes as f64 / slot.rate_acc_dt;
                slot.rate_acc_bytes = 0;
                slot.rate_acc_dt = 0.0;
                slot.detector.observe_rate(sample);
                slot.rate_est = if slot.rate_est <= 0.0 {
                    sample
                } else {
                    RATE_ALPHA * sample + (1.0 - RATE_ALPHA) * slot.rate_est
                };
            }
        }
    }

    pub fn mark_finished(&mut self, slot_id: usize) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.id == slot_id) {
            slot.active = false;
            slot.finished = true;
        }
    }

    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.busy()).count()
    }

    fn theta(&self) -> f64 {
        let delta = self.delta_est.get();
        let active: Vec<&WorkerSlot> = self.slots.iter().filter(|s| s.busy()).collect();
        let n = active.len().max(1) as f64;
        let rem_bytes: u64 = active.iter().map(|s| s.remaining()).sum();
        let agg_rate: f64 = active.iter().map(|s| s.rate_est).sum();
        let t_rem = if agg_rate > 0.0 {
            rem_bytes as f64 / agg_rate
        } else {
            1.0
        };
        (delta * t_rem / n).sqrt().max(delta * 2.0)
    }

    /// Evaluate divergence and rate-collapse across workers, returning any
    /// profitable shrink/steal actions while guarding against repair storms.
    pub fn tick(&mut self, now: f64) -> Vec<StealAction> {
        let mut actions = Vec::new();
        let stall_timeout = self.stall_timeout_s;

        // 1. Feed wall-clock silence to CollapseDetectors
        for slot in &mut self.slots {
            if slot.busy() && now >= slot.setup_end {
                let quiet = (now - slot.last_progress.max(slot.setup_end)).max(0.0);
                slot.detector.observe_silence(quiet, stall_timeout);
                if quiet >= stall_timeout {
                    slot.stalled = true;
                }
            }
        }

        // 2. Steal work when either an idle slot is available or capacity < max_concurrent
        let theta = self.theta();
        let delta = self.delta_est.get();
        let mut any_divergent = false;

        for _ in 0..MAX_REPAIRS_PER_TICK {
            if self.active_count() >= self.max_concurrent {
                break;
            }

            // Pick a reference taker rate: best healthy active worker's speed (or fast prior)
            let best_healthy = self
                .slots
                .iter()
                .filter(|s| s.rate_est > 0.0 && s.detector.health() == Health::Healthy)
                .map(|s| s.rate_est)
                .fold(0.0f64, f64::max);

            // Pick worst victim: Stalled/Degraded/Suspect first, then highest finite/inf ETA with enough bytes left
            let victim_idx = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.busy() && s.remaining() >= STEAL_QUANTUM * 2)
                .max_by(|(_, a), (_, b)| {
                    let ha = a.detector.health();
                    let hb = b.detector.health();
                    ha.cmp(&hb).then_with(|| {
                        let ea = a.eta();
                        let eb = b.eta();
                        ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
                    })
                })
                .map(|(i, _)| i);

            let Some(vi) = victim_idx else {
                break;
            };

            let collapsing = self.slots[vi].detector.health().is_suspect_or_worse();
            let v_eta = self.slots[vi].eta();
            let rv = self.slots[vi].rate_est;
            let rt = if best_healthy > 0.0 {
                best_healthy
            } else {
                rv.max(256_000.0)
            };
            let left = self.slots[vi].remaining() as f64;
            let t_eta = left / (2.0 * rt);

            // Hysteresis: skip non-collapsing connections below deadband theta
            if !collapsing && !matches!((v_eta - t_eta).partial_cmp(&theta), Some(std::cmp::Ordering::Greater)) {
                break;
            }
            any_divergent = true;

            // Require non-collapsing busy divergence to persist before splitting
            if !collapsing {
                let persist = REPAIR_PERSIST.max(REPAIR_PERSIST_DELTAS * delta);
                match self.repair_candidate {
                    Some((v, since)) if v == vi => {
                        if now - since < persist {
                            break;
                        }
                    }
                    _ => {
                        self.repair_candidate = Some((vi, now));
                        break;
                    }
                }
            }

            // Compute optimal split `x` equalizing finish time, charging setup cost `delta`
            let x = if rv <= 0.0 || self.slots[vi].stalled {
                left / 2.0
            } else {
                ((left / rv - delta) * (rv * rt) / (rv + rt)).clamp(0.0, left - STEAL_QUANTUM as f64)
            };

            if x < STEAL_QUANTUM as f64 {
                break;
            }

            // Profitability test: verify makespan improves strictly more than `delta`
            if !collapsing {
                let makespan_now = left / rv.max(1.0);
                let v_after = (left - x) / rv.max(1.0);
                let t_after = delta + x / rt.max(1.0);
                let makespan_after = v_after.max(t_after);
                if !(makespan_now - makespan_after > delta) {
                    break;
                }
            }

            let cur = self.slots[vi].pos.load(Ordering::Acquire);
            let old_end = self.slots[vi].end.load(Ordering::Acquire);
            if old_end <= cur || (old_end - cur + 1) < STEAL_QUANTUM * 2 {
                break;
            }

            let steal_len = (x as u64).min((old_end - cur + 1) / 2).max(STEAL_QUANTUM);
            let steal_start = old_end - steal_len + 1;
            if steal_start <= cur {
                break;
            }
            let new_victim_end = steal_start - 1;

            // Atomically shrink victim's upper bound so its read loop halts cleanly at new_victim_end
            self.slots[vi].end.store(new_victim_end, Ordering::Release);

            let new_id = self.slots.len();
            let new_slot = WorkerSlot::new(new_id, steal_start, steal_start, old_end, now);
            self.slots.push(new_slot);
            self.repair_candidate = None;

            actions.push(StealAction::ShrinkAndSpawn {
                victim_id: self.slots[vi].id,
                new_victim_end,
                new_id,
                steal_start,
                steal_end: old_end,
            });
        }

        if !any_divergent {
            self.repair_candidate = None;
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_chunk_halving() {
        let seg0 = ActiveSegment::new(0, 0, 100_000, 2_000_000);
        let mut sched = DynamicScheduler::new(vec![seg0]);

        assert_eq!(sched.segments.len(), 1);
        let new_seg = sched.split_largest_segment(1).expect("Should split 1.9MB range");

        assert_eq!(sched.segments.len(), 2);
        assert_eq!(sched.segments[0].end.load(Ordering::Relaxed), 1_049_999);
        assert_eq!(new_seg.start, 1_050_000);
        assert_eq!(new_seg.end.load(Ordering::Relaxed), 2_000_000);
    }

    #[test]
    fn adaptive_scheduler_preempts_collapsed_worker() {
        let mut slot0 = WorkerSlot::new(0, 0, 0, 4_000_000, 0.0);
        let mut slot1 = WorkerSlot::new(1, 4_000_001, 4_000_001, 8_000_000, 0.0);
        slot0.rate_est = 2_000_000.0;
        slot1.rate_est = 2_000_000.0;

        let mut sched = AdaptiveRangeScheduler::new(vec![slot0, slot1], 8, 2.0);
        sched.observe_ttfb(0.05);

        // Warm up slot0 healthy samples, then collapse slot1
        for i in 1..=6 {
            sched.on_bytes(0, 500_000, i as f64 * 0.25, 0.25);
            sched.on_bytes(1, 500_000, i as f64 * 0.25, 0.25);
        }
        // Collapse slot1 heavily
        for i in 7..=12 {
            sched.on_bytes(0, 500_000, i as f64 * 0.25, 0.25);
            sched.on_bytes(1, 15_000, i as f64 * 0.25, 0.25);
        }

        let actions = sched.tick(3.1);
        assert!(!actions.is_empty(), "Expected scheduler to rescue collapsing worker");
        match &actions[0] {
            StealAction::ShrinkAndSpawn { victim_id, new_victim_end, steal_start, steal_end, .. } => {
                assert_eq!(*victim_id, 1);
                assert_eq!(*steal_start, *new_victim_end + 1);
                assert_eq!(*steal_end, 8_000_000);
            }
        }
    }

    #[test]
    fn adaptive_scheduler_prevents_unprofitable_repair_storm() {
        let slot0 = WorkerSlot::new(0, 0, 0, 90_000, 0.0); // Below 2*STEAL_QUANTUM (128KB)
        let mut sched = AdaptiveRangeScheduler::new(vec![slot0], 8, 2.0);
        sched.on_bytes(0, 10_000, 0.25, 0.25);
        let actions = sched.tick(2.0);
        assert!(actions.is_empty(), "Should never split tiny span below 2*STEAL_QUANTUM");
    }
}
