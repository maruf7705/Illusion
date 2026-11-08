// Illusion Dynamic Chunk Halving Scheduler
// Reverse Engineered from IDMan.exe Core State Machine

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const MIN_SPLIT_SIZE: u64 = 512 * 1024; // 512 KB minimum threshold for chunk halving

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

    /// IDM In-Flight Dynamic Byte Halving Algorithm:
    /// Scans all active segments to find the one with largest remaining byte range.
    /// If >= MIN_SPLIT_SIZE (512 KB), splits the donor range in half atomically.
    /// Returns: Some((new_worker_start, new_worker_end))
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

        // Halve the remaining range
        let half = (old_end - current) / 2;
        let midpoint = current + half;

        // Atomically truncate the donor segment's boundary to midpoint - 1
        donor.end.store(midpoint - 1, Ordering::SeqCst);

        // Create new segment taking [midpoint .. old_end]
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
        // Donor end should be midpoint - 1
        // remaining was 1,900,001. half = 950,000. midpoint = 1,050,000
        assert_eq!(sched.segments[0].end.load(Ordering::Relaxed), 1_049_999);
        assert_eq!(new_seg.start, 1_050_000);
        assert_eq!(new_seg.end.load(Ordering::Relaxed), 2_000_000);
    }
}
