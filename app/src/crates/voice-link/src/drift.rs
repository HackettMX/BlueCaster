//! Keeps a queue between two clocks (the Bluetooth controller's and a Windows
//! audio device's) near its target level. The level is sampled at every pull;
//! its minimum over a window (the trough, just before the next burst of
//! input arrives) is compared with the target, and when it is off by more
//! than the tolerance a few samples are dropped or repeated.

/// Window over which the trough is taken, in milliseconds of pulled audio
const WINDOW_MS: usize = 200;

/// What to do with the next pull
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adjust {
    None,
    /// Discard this many queued samples first (the queue is too full)
    Drop(usize),
    /// Repeat a sample this many times (the queue is running low)
    Duplicate(usize),
    /// Far below the target (more than twice the tolerance, e.g. after the
    /// consumer's first burst): silence until the prefill is queued again,
    /// one short gap instead of seconds of repeated samples
    Reprime,
}

#[derive(Debug)]
pub(crate) struct Drift {
    target: usize,
    tolerance: usize,
    window: usize,
    /// Most samples corrected per window (0.25 % of the rate)
    max_step: usize,
    pulled: usize,
    trough: usize,
    /// The first window after priming: the level is brought to the target
    /// plus half the tolerance in one step (where the priming left it
    /// depends on when the input's blocks came, often at the tolerance's
    /// edge, which the slow steps would take seconds of repeated samples to
    /// leave, and where jitter would cross it again and again)
    fresh: bool,
}

impl Drift {
    /// `target` and `tolerance` in samples at `rate`
    pub fn new(rate: u32, target: usize, tolerance: usize) -> Drift {
        let rate = rate as usize;
        Drift {
            target,
            tolerance,
            window: (rate * WINDOW_MS / 1000).max(1),
            max_step: (rate / 2000).max(1),
            pulled: 0,
            trough: usize::MAX,
            fresh: false,
        }
    }

    /// Primed (again): a new window, the first one corrects in one step
    pub fn restart(&mut self) {
        self.reset();
        self.fresh = true;
    }

    /// Starts a new window (after a re-prime: the old trough says nothing)
    pub fn reset(&mut self) {
        self.pulled = 0;
        self.trough = usize::MAX;
    }

    /// Before a pull of `n` samples with `level` samples queued
    pub fn observe(&mut self, level: usize, n: usize) -> Adjust {
        self.trough = self.trough.min(level);
        self.pulled += n;
        if self.pulled < self.window {
            return Adjust::None;
        }
        let trough = self.trough;
        self.reset();
        if std::mem::take(&mut self.fresh) && trough + 2 * self.tolerance >= self.target {
            // To the target plus half the tolerance, at most a tolerance's
            // worth: a late input block lowers the trough, nothing raises
            // it but the clocks, so the level keeps its room below where
            // the jitter of a loaded machine would otherwise cross the
            // tolerance now and then (one small step each time)
            let aim = self.target + self.tolerance / 2;
            let slack = self.tolerance / 4;
            return if trough > aim + slack {
                Adjust::Drop((trough - aim).min(self.tolerance))
            } else if trough + slack < aim {
                Adjust::Duplicate((aim - trough).min(self.tolerance))
            } else {
                Adjust::None
            };
        }
        if trough > self.target + self.tolerance {
            let excess = trough - self.target - self.tolerance;
            Adjust::Drop((excess / 2).clamp(1, self.max_step))
        } else if trough + 2 * self.tolerance < self.target {
            Adjust::Reprime
        } else if trough + self.tolerance < self.target {
            let short = self.target - self.tolerance - trough;
            Adjust::Duplicate((short / 2).clamp(1, self.max_step))
        } else {
            Adjust::None
        }
    }
}

/// What one [`Primed::pull`] did
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pulled {
    /// Samples taken from the queue into the output
    pub got: usize,
    /// Zero samples in the output (priming or underrun)
    pub zeros: usize,
    /// The queue ran dry after it had been primed
    pub underrun: bool,
    pub dropped: usize,
    pub duplicated: usize,
    /// Far below the target: primed again ([`Adjust::Reprime`])
    pub reprimed: bool,
}

/// The consumer side of a jittery queue: silence until `prefill` samples
/// are queued (at the start and after an underrun), then the queue's
/// samples, with [`Drift`] keeping the level near the prefill
#[derive(Debug)]
pub(crate) struct Primed {
    prefill: usize,
    primed: bool,
    drift: Drift,
    last: i16,
    /// Repeats the drift control asked for beyond one pull (the step after
    /// priming may be larger than a pull)
    owed: usize,
}

impl Primed {
    pub fn new(rate: u32, prefill: usize, tolerance: usize) -> Primed {
        Primed { prefill, primed: false, drift: Drift::new(rate, prefill, tolerance), last: 0, owed: 0 }
    }

    /// Fills all of `out`
    pub fn pull(&mut self, queue: &mut rtrb::Consumer<i16>, out: &mut [i16]) -> Pulled {
        let mut r = Pulled::default();
        let level = queue.slots();
        if !self.primed {
            if level < self.prefill.max(out.len()).max(1) {
                out.fill(0);
                r.zeros = out.len();
                return r;
            }
            self.primed = true;
            self.owed = 0;
            self.drift.restart();
        }
        let mut want = out.len();
        let adjust = match self.drift.observe(level, out.len()) {
            Adjust::None if self.owed > 0 => Adjust::Duplicate(0),
            a => a,
        };
        match adjust {
            Adjust::None => {}
            Adjust::Drop(n) => {
                let n = n.min(level.saturating_sub(want));
                if n > 0
                    && let Ok(chunk) = queue.read_chunk(n)
                {
                    chunk.commit_all();
                }
                r.dropped = n;
            }
            Adjust::Duplicate(n) => {
                let n = n + std::mem::take(&mut self.owed);
                r.duplicated = n.min(want);
                self.owed = n - r.duplicated;
                want -= r.duplicated;
            }
            Adjust::Reprime => {
                self.primed = false;
                self.owed = 0;
                out.fill(0);
                r.zeros = out.len();
                r.reprimed = true;
                return r;
            }
        }
        r.got = pop_into(queue, &mut out[..want]);
        if let Some(&l) = out[..r.got].last() {
            self.last = l;
        }
        if r.got < want {
            out[r.got..].fill(0);
            r.zeros = out.len() - r.got;
            r.duplicated = 0;
            r.underrun = true;
            self.primed = false;
            self.owed = 0;
        } else if r.duplicated > 0 {
            out[want..].fill(self.last);
        }
        r
    }
}

/// Moves up to `out.len()` samples from `queue`; returns how many
pub(crate) fn pop_into(queue: &mut rtrb::Consumer<i16>, out: &mut [i16]) -> usize {
    let n = queue.slots().min(out.len());
    if n == 0 {
        return 0;
    }
    let Ok(chunk) = queue.read_chunk(n) else { return 0 };
    let (a, b) = chunk.as_slices();
    out[..a.len()].copy_from_slice(a);
    out[a.len()..a.len() + b.len()].copy_from_slice(b);
    chunk.commit_all();
    n
}

/// Pushes as much of `samples` as fits; returns how many
pub(crate) fn push_from(queue: &mut rtrb::Producer<i16>, samples: &[i16]) -> usize {
    let n = queue.slots().min(samples.len());
    if n > 0
        && let Ok(chunk) = queue.write_chunk_uninit(n)
    {
        return chunk.fill_from_iter(samples.iter().copied());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_the_tolerance_nothing_happens() {
        let mut d = Drift::new(16_000, 640, 160);
        for i in 0..10_000 {
            assert_eq!(d.observe(640 + (i % 160), 120), Adjust::None);
        }
    }

    #[test]
    fn a_full_queue_drops_and_a_low_one_repeats() {
        let mut d = Drift::new(16_000, 640, 160);
        let adj: Vec<_> = (0..100).map(|_| d.observe(1200, 120)).filter(|a| *a != Adjust::None).collect();
        assert!(!adj.is_empty() && adj.iter().all(|a| matches!(a, Adjust::Drop(1..=8))), "{adj:?}");
        let mut d = Drift::new(16_000, 640, 160);
        let adj: Vec<_> = (0..100).map(|_| d.observe(400, 120)).filter(|a| *a != Adjust::None).collect();
        assert!(!adj.is_empty() && adj.iter().all(|a| matches!(a, Adjust::Duplicate(1..=8))), "{adj:?}");
        // Far below: primed again at once
        let mut d = Drift::new(16_000, 640, 160);
        let adj: Vec<_> = (0..100).map(|_| d.observe(100, 120)).filter(|a| *a != Adjust::None).collect();
        assert!(!adj.is_empty() && adj.iter().all(|a| *a == Adjust::Reprime), "{adj:?}");
    }

    #[test]
    fn the_first_window_after_priming_corrects_in_one_step() {
        // Aim: the target plus half the tolerance (720). Primed past the
        // tolerance's edge: one repeat of a tolerance's worth at most
        let mut d = Drift::new(16_000, 640, 160);
        d.restart();
        let adj: Vec<_> = (0..30).map(|_| d.observe(400, 120)).filter(|a| *a != Adjust::None).collect();
        assert_eq!(adj[0], Adjust::Duplicate(160));
        // Then the slow steps again
        assert!(adj[1..].iter().all(|a| matches!(a, Adjust::Duplicate(1..=8))), "{adj:?}");
        // At the target: brought up to the aim once, then left alone
        let mut d = Drift::new(16_000, 640, 160);
        d.restart();
        let adj: Vec<_> = (0..30).map(|_| d.observe(640, 120)).filter(|a| *a != Adjust::None).collect();
        assert_eq!(adj, [Adjust::Duplicate(80)]);
        // Too full: down to the aim
        let mut d = Drift::new(16_000, 640, 160);
        d.restart();
        assert_eq!((0..27).map(|_| d.observe(900, 120)).find(|a| *a != Adjust::None), Some(Adjust::Drop(160)));
        // Near the aim already: nothing
        let mut d = Drift::new(16_000, 640, 160);
        d.restart();
        assert!((0..30).all(|_| d.observe(700, 120) == Adjust::None));
        assert!((0..30).all(|_| d.observe(750, 120) == Adjust::None));
    }

    #[test]
    fn a_late_block_now_and_then_needs_no_step_after_priming() {
        // The uplink at 16 kHz (prefill 30 ms, tolerance 10 ms): 10 ms
        // capture blocks, 7.5 ms pulls, and every 25th block comes 10 ms
        // late, with the next one (a loaded machine)
        let (mut producer, mut consumer) = rtrb::RingBuffer::<i16>::new(4000);
        let mut primed = Primed::new(16_000, 480, 160);
        let mut out = [0i16; 120];
        let block = [1i16; 160];
        let (mut credit, mut blocks, mut held) = (0.0, 0u32, 0usize);
        let (mut steps, mut first_window) = (0, 0);
        for i in 0..8000 {
            credit += 120.0;
            while credit >= 160.0 {
                credit -= 160.0;
                blocks += 1;
                if blocks.is_multiple_of(25) {
                    held += 1;
                    continue;
                }
                for _ in 0..=held {
                    assert_eq!(push_from(&mut producer, &block), 160);
                }
                held = 0;
            }
            let r = primed.pull(&mut consumer, &mut out);
            assert!(!r.underrun && !r.reprimed || i < 10, "pull {i}: {r:?}");
            // The first window after priming may take one step
            if i < 40 {
                first_window += r.dropped + r.duplicated;
            } else {
                steps += r.dropped + r.duplicated;
            }
        }
        assert!(first_window <= 160, "{first_window}");
        assert_eq!(steps, 0, "corrective steps after the first window");
    }

    #[test]
    fn the_trough_decides_not_the_peaks() {
        // Bursts lift the level far above the target, but it falls back to
        // it before each burst
        let mut d = Drift::new(16_000, 640, 160);
        for i in 0..10_000 {
            let level = if i % 4 == 0 { 650 } else { 1400 };
            assert_eq!(d.observe(level, 120), Adjust::None);
        }
    }
}
