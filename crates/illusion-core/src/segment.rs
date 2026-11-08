use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentStatus {
    Pending,
    Downloading,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: usize,
    pub start: u64,
    pub current: u64,
    pub end: u64, // inclusive
    pub status: SegmentStatus,
    #[serde(skip)]
    pub speed: f64, // bytes/sec
}

impl Segment {
    pub fn new(id: usize, start: u64, end: u64) -> Self {
        Self {
            id,
            start,
            current: start,
            end,
            status: SegmentStatus::Pending,
            speed: 0.0,
        }
    }

    pub fn downloaded_bytes(&self) -> u64 {
        if self.current >= self.start {
            self.current - self.start
        } else {
            0
        }
    }

    pub fn total_bytes(&self) -> u64 {
        if self.end >= self.start {
            self.end - self.start + 1
        } else {
            0
        }
    }

    pub fn remaining_bytes(&self) -> u64 {
        if self.end >= self.current {
            self.end - self.current + 1
        } else {
            0
        }
    }

    pub fn is_finished(&self) -> bool {
        self.current > self.end || self.status == SegmentStatus::Completed
    }

    pub fn progress_ratio(&self) -> f32 {
        let total = self.total_bytes();
        if total == 0 {
            0.0
        } else {
            (self.downloaded_bytes() as f32 / total as f32).clamp(0.0, 1.0)
        }
    }

    /// Dynamic splitting:
    /// If there are at least `min_split_size` bytes remaining in this segment,
    /// split the remaining upper half into a new segment and return it.
    pub fn split_remaining(&mut self, new_id: usize, min_split_size: u64) -> Option<Segment> {
        if self.status != SegmentStatus::Downloading {
            return None;
        }

        let remaining = self.remaining_bytes();
        if remaining < min_split_size * 2 {
            return None;
        }

        let half = remaining / 2;
        let new_start = self.end - half + 1;
        let new_end = self.end;

        // Shrink current segment's end
        self.end = new_start - 1;

        Some(Segment::new(new_id, new_start, new_end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_metrics() {
        let mut seg = Segment::new(1, 0, 999);
        assert_eq!(seg.total_bytes(), 1000);
        assert_eq!(seg.downloaded_bytes(), 0);
        assert_eq!(seg.remaining_bytes(), 1000);

        seg.current = 500;
        assert_eq!(seg.downloaded_bytes(), 500);
        assert_eq!(seg.remaining_bytes(), 500);
        assert_eq!(seg.progress_ratio(), 0.5);
    }

    #[test]
    fn test_dynamic_splitting() {
        let mut seg = Segment::new(1, 0, 9_999_999); // 10 MB (0..9,999,999 is exactly 10,000,000 bytes)
        seg.status = SegmentStatus::Downloading;
        seg.current = 2_000_000; // 8,000,000 remaining (2,000,000..9,999,999)

        let initial_remaining = seg.remaining_bytes();
        let new_seg = seg.split_remaining(2, 1_000_000).expect("Should split");
        assert_eq!(new_seg.id, 2);
        assert_eq!(new_seg.start, 6_000_000);
        assert_eq!(new_seg.end, 9_999_999);
        assert_eq!(seg.end, 5_999_999);
        assert_eq!(seg.remaining_bytes() + new_seg.total_bytes(), initial_remaining);
    }
}
