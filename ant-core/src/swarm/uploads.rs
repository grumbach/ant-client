//! How many records a client sends other clients at once: as many as its
//! upload rate can give each the rate a peer needs, the way a BitTorrent
//! client sizes its upload slots by its upload rate, and now and then one
//! more to find out whether its line carries more.
//!
//! A peer gives up on a record coming slower than [`MIN_PEER_RATE`], and
//! asks the nodes; so a client sending records at once, each at less than
//! that, sends them for nothing. It sends `1 + rate / SLOT_RATE` at once
//! (one at first), where [`SLOT_RATE`] is twice what a peer needs, so the
//! sends its rate carries each get at least that, with one more sharing the
//! line. Its rate is what its line carried: every [`SAMPLE_EVERY`] while it
//! sends, the swarm counts the bytes that left all of its links' channels
//! (natively, that its peers acknowledged), and the whole of any such tick
//! in which bytes left or a send held a place counts as time spent sending.
//! A rate so taken is never above what the line carried: bytes never count
//! without the time they took, though a short burst inside a tick counts
//! slower than it went. A rate is taken once at least [`MIN_SAMPLE`] of
//! sending time has gathered.
//!
//! The client keeps the best rate seen, fading by [`FADE`] for every
//! [`PERIOD`] of time spent sending since: peers asking little, or reading
//! slowly, do not make it forget what its line carried, and a line slowed
//! for good is followed within a minute or so of sending. Only the time when
//! it sends counts, as asks come when peers want records. A link that cannot
//! carry a full rate alone (a far peer, natively, whose channel waits on its
//! acknowledgements) would never show what several carry together: after
//! every [`PROBE_EVERY`] of holding places, a client whose places are all
//! taken has one more for [`PROBE_WINDOW`], and keeps it if the rate it
//! shows earns it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use web_time::Instant;

/// The slowest a peer takes a record at before it asks the nodes instead,
/// in bytes a second.
pub(super) const MIN_PEER_RATE: u64 = 512 * 1024;
/// The rate a client's sends at once each get at the least, in bytes a
/// second: twice what a peer needs, so that one more send at once, sharing
/// the line, still leaves each enough.
const SLOT_RATE: f64 = 2.0 * MIN_PEER_RATE as f64;
/// How often the swarm counts the bytes that left its links while it sends.
pub(super) const SAMPLE_EVERY: Duration = Duration::from_millis(100);
/// The sending time a rate is taken over at the least.
const MIN_SAMPLE: Duration = Duration::from_secs(1);
/// The sending time over which the best rate seen fades by [`FADE`].
const PERIOD: Duration = Duration::from_secs(4);
/// How much of the best rate seen a [`PERIOD`] of sending keeps.
const FADE: f64 = 0.8;
/// The time spent holding places between tries of one more.
const PROBE_EVERY: Duration = Duration::from_secs(8);
/// How long a try of one more lasts, in time spent holding places: long
/// enough for the rate it shows to be taken.
const PROBE_WINDOW: Duration = Duration::from_secs(2);

/// The places a client has for sending records at once, and the rate its
/// sends have shown.
#[derive(Debug)]
pub(super) struct UploadLimit {
    max: usize,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Sends holding a place now.
    running: usize,
    /// When `held` was last brought up to date.
    at: Instant,
    /// Time spent with at least one send holding a place so far.
    held: Duration,
    /// `held` from which one more place may be tried, and until which a try
    /// lasts.
    probe_at: Duration,
    probe_until: Duration,
    /// Bytes and sending time counted since the last rate was taken.
    pending_bytes: u64,
    pending_time: Duration,
    /// The best rate seen, in bytes a second, faded as of now.
    best_rate: f64,
}

impl UploadLimit {
    /// Places for one record at a time, and up to `max` as the rate shows
    /// the line carries more.
    pub(super) fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            state: Mutex::new(State {
                running: 0,
                at: Instant::now(),
                held: Duration::ZERO,
                probe_at: PROBE_EVERY,
                probe_until: Duration::ZERO,
                pending_bytes: 0,
                pending_time: Duration::ZERO,
                best_rate: 0.0,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the client may send at once now, besides a try of one more.
    pub(super) fn current(&self) -> usize {
        allowed(self.state().best_rate, self.max)
    }

    /// Sends holding a place now.
    pub(super) fn running(&self) -> usize {
        self.state().running
    }

    /// Time spent with at least one send holding a place so far.
    pub(super) fn held(&self) -> Duration {
        let mut state = self.state();
        state.advance(Instant::now());
        state.held
    }

    /// A place to send a record now, or `None` if every place is taken.
    pub(super) fn try_start(self: &Arc<Self>) -> Option<UploadSlot> {
        self.try_start_at(Instant::now())
    }

    fn try_start_at(self: &Arc<Self>, now: Instant) -> Option<UploadSlot> {
        let mut state = self.state();
        state.advance(now);
        let allowed = allowed(state.best_rate, self.max);
        let trying = state.held < state.probe_until;
        let places = (allowed + usize::from(trying)).min(self.max);
        if state.running >= places {
            // Every place taken, and no more than that: one more, for a
            // while, once a while of sending.
            if trying
                || state.running != allowed
                || allowed >= self.max
                || state.held < state.probe_at
            {
                return None;
            }
            state.probe_until = state.held.saturating_add(PROBE_WINDOW);
            state.probe_at = state.held.saturating_add(PROBE_EVERY);
        }
        state.running += 1;
        Some(UploadSlot {
            limit: Arc::clone(self),
            released: false,
        })
    }

    fn release_at(&self, now: Instant) {
        let mut state = self.state();
        state.advance(now);
        state.running = state.running.saturating_sub(1);
    }

    /// Counts `bytes` that left the client's links in `time` spent sending.
    pub(super) fn sampled(&self, bytes: u64, time: Duration) {
        let mut state = self.state();
        state.pending_bytes = state.pending_bytes.saturating_add(bytes);
        state.pending_time = state.pending_time.saturating_add(time);
        if state.pending_time < MIN_SAMPLE {
            return;
        }
        let seconds = state.pending_time.as_secs_f64();
        let faded = state.best_rate * FADE.powf(seconds / PERIOD.as_secs_f64());
        let faded = if faded.is_finite() { faded } else { 0.0 };
        let rate = state.pending_bytes as f64 / seconds;
        state.best_rate = if rate.is_finite() {
            faded.max(rate)
        } else {
            faded
        };
        state.pending_bytes = 0;
        state.pending_time = Duration::ZERO;
    }

    /// Takes `rate` as the best seen, as a test needs.
    #[cfg(test)]
    pub(super) fn set_best_rate(&self, rate: f64) {
        self.state().best_rate = rate;
    }
}

impl State {
    /// Brings the time spent holding places up to `now`.
    fn advance(&mut self, now: Instant) {
        if self.running > 0 {
            self.held = self
                .held
                .saturating_add(now.saturating_duration_since(self.at));
        }
        self.at = self.at.max(now);
    }
}

/// Sends at once the best rate `rate` allows: one, and one more for each
/// [`SLOT_RATE`] it carries, up to `max`.
fn allowed(rate: f64, max: usize) -> usize {
    let slots = (rate / SLOT_RATE).floor();
    if !slots.is_finite() || slots < 0.0 {
        return 1;
    }
    if slots >= max as f64 {
        return max;
    }
    (1 + slots as usize).min(max)
}

/// A send's place among those running at once, given back when dropped.
#[derive(Debug)]
pub(super) struct UploadSlot {
    limit: Arc<UploadLimit>,
    /// Given back already, at a time a test chose.
    released: bool,
}

impl UploadSlot {
    /// Gives the place back at `now`.
    #[cfg(test)]
    fn release_at(mut self, now: Instant) {
        self.released = true;
        self.limit.release_at(now);
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        if !self.released {
            self.limit.release_at(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn at(zero: Instant, seconds: f64) -> Instant {
        zero + Duration::from_secs_f64(seconds)
    }

    fn limit() -> (Arc<UploadLimit>, Instant) {
        let limit = Arc::new(UploadLimit::new(16));
        let zero = limit.state().at;
        (limit, zero)
    }

    /// A place taken at `from` seconds after `zero`.
    fn start(limit: &Arc<UploadLimit>, zero: Instant, from: f64) -> UploadSlot {
        limit
            .try_start_at(at(zero, from))
            .unwrap_or_else(|| panic!("no place at {from} s"))
    }

    /// `seconds` of sending at `rate` bytes a second, counted a tenth of a
    /// second at a time.
    fn sending(limit: &UploadLimit, seconds: u32, rate: u64) {
        for _ in 0..seconds {
            for tick in 0..10 {
                let rest = if tick == 9 { rate % 10 } else { 0 };
                limit.sampled(rate / 10 + rest, Duration::from_millis(100));
            }
        }
    }

    fn best_rate(limit: &UploadLimit) -> f64 {
        limit.state().best_rate
    }

    /// Whether `rate` is `expected`, but for the bytes a tick leaves out
    /// when a rate does not divide into tenths.
    fn about(rate: f64, expected: f64) -> bool {
        (rate - expected).abs() <= expected * 1e-5 + 1.0
    }

    #[test]
    fn the_places_follow_the_rate_one_more_than_it_carries() {
        let max = 16;
        assert_eq!(allowed(0.0, max), 1);
        assert_eq!(allowed(SLOT_RATE * 0.9, max), 1);
        assert_eq!(allowed(SLOT_RATE, max), 2);
        assert_eq!(allowed(SLOT_RATE * 4.5, max), 5);
        assert_eq!(allowed(SLOT_RATE * 100.0, max), max);
        assert_eq!(allowed(f64::NAN, max), 1);
        assert_eq!(allowed(f64::INFINITY, max), 1);
        assert_eq!(allowed(-1.0, max), 1);
    }

    #[test]
    fn one_record_at_a_time_until_the_line_shows_more() {
        let (limit, zero) = limit();
        let first = start(&limit, zero, 0.0);
        assert!(limit.try_start_at(at(zero, 0.0)).is_none());
        sending(&limit, 2, 4 * MIB);
        first.release_at(at(zero, 2.0));
        assert!(about(best_rate(&limit), 4.0 * MIB as f64));
        assert_eq!(limit.current(), 5);
        assert_eq!(limit.running(), 0);
    }

    #[test]
    fn what_the_line_carried_counts_not_how_many_sent() {
        let (limit, _) = limit();
        limit.set_best_rate(4.0 * SLOT_RATE);
        // However many send, a 2 MiB a second line shows 2 MiB a second:
        // the best seen fades toward it, and no lower.
        sending(&limit, 60, 2 * MIB);
        assert!(about(best_rate(&limit), 2.0 * MIB as f64));
        assert_eq!(limit.current(), 3);
    }

    #[test]
    fn a_burst_inside_a_tick_counts_no_faster_than_the_tick() {
        let (limit, _) = limit();
        // 100 KiB sent within each tenth of a second: at most 1000 KiB a
        // second, whatever the burst's own speed.
        sending(&limit, 2, 1000 * 1024);
        assert!(best_rate(&limit) <= 1000.0 * 1024.0 + 1.0);
    }

    #[test]
    fn the_best_rate_fades_with_the_time_spent_sending() {
        let (limit, _) = limit();
        sending(&limit, 1, 16 * MIB);
        assert!(about(best_rate(&limit), 16.0 * MIB as f64));
        // A minute of sending that delivers nothing: fifteen periods' fading.
        sending(&limit, 60, 0);
        let faded = 16.0 * MIB as f64 * FADE.powi(15);
        assert!(about(best_rate(&limit), faded), "{}", best_rate(&limit));
        assert_eq!(limit.current(), 1);
        // Time not spent sending is not counted, and fades nothing.
        limit.sampled(0, Duration::ZERO);
        assert!(about(best_rate(&limit), faded));
    }

    #[test]
    fn a_full_limit_tries_one_more_for_a_while_after_a_while_of_sending() {
        let (limit, zero) = limit();
        // A far link carries 0.75 MiB a second alone: one place.
        let held = start(&limit, zero, 0.0);
        sending(&limit, 4, 3 * MIB / 4);
        assert_eq!(limit.current(), 1);
        assert!(limit.try_start_at(at(zero, 5.0)).is_none());
        // Eight seconds of holding places: one more, for two seconds, for
        // as many records one after another as it takes.
        let probe = start(&limit, zero, 8.0);
        assert!(limit.try_start_at(at(zero, 8.5)).is_none());
        drop(probe);
        let probe = start(&limit, zero, 9.0);
        // The two together carry twice as much: the place is earned.
        sending(&limit, 2, 3 * MIB / 2);
        assert_eq!(limit.current(), 2);
        // The try is over, and the next is eight seconds of holding away.
        assert!(limit.try_start_at(at(zero, 10.5)).is_none());
        drop((held, probe));
    }

    #[test]
    fn a_try_never_takes_more_than_one_past_the_limit_nor_past_the_ceiling() {
        let (limit, zero) = limit();
        limit.set_best_rate(3.0 * SLOT_RATE);
        let mut slots: Vec<UploadSlot> = (0..4).map(|_| start(&limit, zero, 0.0)).collect();
        // A try opens at the limit, after a while of holding places.
        slots.push(start(&limit, zero, 8.0));
        assert!(limit.try_start_at(at(zero, 8.0)).is_none());
        // The best rate rising to the ceiling mid-try: never past it.
        limit.set_best_rate(100.0 * SLOT_RATE);
        while slots.len() < 16 {
            slots.push(start(&limit, zero, 8.5));
        }
        assert!(limit.try_start_at(at(zero, 8.5)).is_none());
        // Falling below what runs: no try opens past it.
        limit.set_best_rate(2.0 * SLOT_RATE);
        slots.truncate(4);
        assert!(limit.try_start_at(at(zero, 30.0)).is_none());
        drop(slots);
    }

    #[test]
    fn a_dropped_place_is_given_back() {
        let limit = Arc::new(UploadLimit::new(16));
        let slot = limit.try_start();
        assert_eq!(limit.running(), 1);
        drop(slot);
        assert_eq!(limit.running(), 0);
        assert!(limit.try_start().is_some());
    }
}
