//! Fast collapse detection, latency estimation, and online concurrency admission.
//! Architecture adapted for Illusion from Hydra's lock-free scheduler design
//! (`crates/hydra-core/src/detect.rs` and `admission.rs`).

/// Connection health classification used by the adaptive scheduler to preempt
/// collapsing or stalled TCP streams before the full stall timeout expires.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Health {
    /// Delivering at or near its established rate.
    #[default]
    Healthy,
    /// Dual-window ratio or CUSUM sees early collapse evidence; eligible as repair victim.
    Suspect,
    /// Confirmed sustained rate collapse.
    Degraded,
    /// Silent past `stall_timeout`; range reclaimed immediately.
    Stalled,
    /// Permanently failed for this transfer.
    Dead,
}

impl Health {
    pub fn is_suspect_or_worse(self) -> bool {
        self >= Health::Suspect
    }
}

/// Minimum samples before grading health so initial TCP slow-start noise isn't misclassified.
pub const WARMUP_SAMPLES: u32 = 4;

/// Per-connection dual-window + two-sided CUSUM collapse detector.
///
/// An ordinary EWMA lags behind step collapses because post-collapse samples
/// arrive slowly. Combining a fast EWMA against a frozen long-term reference
/// plus a cumulative sum (CUSUM) detects both sharp drops and slow throttles.
#[derive(Clone, Debug)]
pub struct CollapseDetector {
    long: f64,
    short: f64,
    cusum_down: f64,
    n: u32,
    ratio_suspect: f64,
    ratio_degraded: f64,
    cusum_h: f64,
    cusum_k: f64,
    health: Health,
}

impl Default for CollapseDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl CollapseDetector {
    pub fn new() -> Self {
        Self {
            long: 0.0,
            short: 0.0,
            cusum_down: 0.0,
            n: 0,
            ratio_suspect: 0.55,
            ratio_degraded: 0.30,
            cusum_h: 2.0,
            cusum_k: 0.25,
            health: Health::Healthy,
        }
    }

    /// Record an instantaneous rate sample (bytes/s) measured over a full rate window.
    pub fn observe_rate(&mut self, rate: f64) {
        let r = rate.max(0.0);
        self.n = self.n.saturating_add(1);
        if self.n == 1 {
            self.long = r;
            self.short = r;
            return;
        }

        self.short = 0.45 * r + 0.55 * self.short;
        // Freeze the long-term reference while downward evidence is accumulating so
        // a slow drift cannot adapt the baseline down to match the degradation.
        if self.cusum_down <= 0.0 {
            self.long = 0.08 * r + 0.92 * self.long;
        }

        if self.n <= WARMUP_SAMPLES || self.long <= 0.0 {
            return;
        }

        let dev = (self.long - r) / self.long - self.cusum_k;
        self.cusum_down = (self.cusum_down + dev).max(0.0);

        let ratio = self.short / self.long;
        self.health = if ratio <= self.ratio_degraded || self.cusum_down >= 2.0 * self.cusum_h {
            Health::Degraded
        } else if ratio <= self.ratio_suspect || self.cusum_down >= self.cusum_h {
            Health::Suspect
        } else {
            Health::Healthy
        };
    }

    /// Escalate health based on wall-clock silence when zero bytes are arriving.
    pub fn observe_silence(&mut self, since_progress_s: f64, stall_timeout_s: f64) {
        if since_progress_s >= stall_timeout_s {
            self.health = Health::Stalled;
        } else if since_progress_s >= 0.5 * stall_timeout_s && self.health < Health::Suspect {
            self.health = Health::Suspect;
        }
    }

    pub fn mark_dead(&mut self) {
        self.health = Health::Dead;
    }

    pub fn health(&self) -> Health {
        self.health
    }

    pub fn established_rate(&self) -> f64 {
        self.long
    }
}

/// Online EWMA estimator of per-request setup cost (`delta`, time-to-first-byte in seconds).
/// Ensures repair profitability checks scale accurately across LAN vs high-RTT TLS origins.
#[derive(Clone, Copy, Debug)]
pub struct DeltaEstimator {
    ewma: f64,
    alpha: f64,
    n: u32,
}

impl DeltaEstimator {
    pub fn new(prior_s: f64) -> Self {
        Self {
            ewma: prior_s.max(1e-4),
            alpha: 0.3,
            n: 0,
        }
    }

    pub fn observe(&mut self, ttfb_s: f64) {
        let x = ttfb_s.clamp(1e-4, 30.0);
        if self.n == 0 {
            self.ewma = x;
        } else {
            self.ewma = (1.0 - self.alpha) * self.ewma + self.alpha * x;
        }
        self.n = self.n.saturating_add(1);
    }

    pub fn get(&self) -> f64 {
        self.ewma
    }

    pub fn samples(&self) -> u32 {
        self.n
    }
}

/// Decision returned by incremental concurrency admission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Admit {
    Add,
    Stop,
}

/// Online marginal goodput controller preventing excessive concurrency on saturated
/// or rate-limited (`429`) origins.
#[derive(Clone, Debug)]
pub struct Admission {
    samples: Vec<f64>,
    levels: Vec<usize>,
    min_gain_frac: f64,
    max_conns: usize,
    settled: Option<usize>,
}

impl Admission {
    pub fn new(min_gain_frac: f64, max_conns: usize) -> Self {
        Self {
            samples: Vec::new(),
            levels: Vec::new(),
            min_gain_frac,
            max_conns: max_conns.max(1),
            settled: None,
        }
    }

    pub fn observe_at(&mut self, level: usize, goodput: f64) -> Admit {
        let level = level.max(1);
        self.samples.push(goodput.max(0.0));
        self.levels.push(level);
        let n = self.samples.len();

        if level >= self.max_conns {
            self.settled = Some(self.best_level());
            return Admit::Stop;
        }
        if n == 1 {
            return Admit::Add;
        }

        if !self.step_pays(n - 1) {
            self.settled = Some(self.best_level());
            Admit::Stop
        } else {
            Admit::Add
        }
    }

    fn step_pays(&self, i: usize) -> bool {
        if i == 0 || i >= self.samples.len() {
            return false;
        }
        let prev_rate = self.samples[i - 1].max(1.0);
        let prev_level = self.levels[i - 1].max(1) as f64;
        let this_level = self.levels[i].max(1) as f64;
        if this_level <= prev_level {
            return false;
        }
        let ideal = prev_rate * (this_level / prev_level);
        let headroom = (ideal - prev_rate).max(1e-9);
        (self.samples[i] - prev_rate) / headroom >= self.min_gain_frac
    }

    fn best_level(&self) -> usize {
        if self.samples.is_empty() {
            return 1;
        }
        let mut best = self.levels.first().copied().unwrap_or(1);
        for i in 1..self.samples.len() {
            if self.step_pays(i) {
                best = self.levels[i];
            } else {
                break;
            }
        }
        best.clamp(1, self.max_conns)
    }

    /// Lower the concurrency ceiling irreversibly for this transfer (e.g., on `429`).
    pub fn clamp_max(&mut self, max: usize) {
        let max = max.max(1);
        if max < self.max_conns {
            self.max_conns = max;
            if let Some(n) = self.settled {
                self.settled = Some(n.min(max));
            }
        }
    }

    pub fn max_conns(&self) -> usize {
        self.max_conns
    }

    pub fn settled(&self) -> Option<usize> {
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_noisy_connection_is_not_flagged() {
        let mut det = CollapseDetector::new();
        let rates = [1_000_000.0, 920_000.0, 1_080_000.0, 950_000.0, 1_050_000.0, 910_000.0, 1_020_000.0];
        for r in rates {
            det.observe_rate(r);
            assert_eq!(det.health(), Health::Healthy);
        }
    }

    #[test]
    fn sustained_collapse_detected_by_cusum() {
        let mut det = CollapseDetector::new();
        for _ in 0..6 {
            det.observe_rate(1_000_000.0);
        }
        assert_eq!(det.health(), Health::Healthy);

        // Sustained 55% throughput collapse
        for _ in 0..6 {
            det.observe_rate(450_000.0);
        }
        assert!(det.health().is_suspect_or_worse());
    }

    #[test]
    fn stalled_silence_escalates() {
        let mut det = CollapseDetector::new();
        det.observe_silence(0.6, 1.0);
        assert_eq!(det.health(), Health::Suspect);
        det.observe_silence(1.1, 1.0);
        assert_eq!(det.health(), Health::Stalled);
    }

    #[test]
    fn admission_clamps_on_saturation() {
        let mut adm = Admission::new(0.15, 16);
        assert_eq!(adm.observe_at(1, 10_000_000.0), Admit::Add);
        assert_eq!(adm.observe_at(2, 19_000_000.0), Admit::Add);
        // Doubling 2 -> 4 yields only +2% gain (saturated bottleneck)
        assert_eq!(adm.observe_at(4, 19_400_000.0), Admit::Stop);
        assert_eq!(adm.settled(), Some(2));
    }
}
