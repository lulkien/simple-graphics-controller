//! LVGL-style timer registry: timers with a period fire their callback
//! when the deadline passes, then auto-repeat — all on the calling thread.
//!
//! The registry is a pure SCHEDULER: callbacks take `&mut S` (the app's
//! state) as a parameter instead of capturing it, so timers never fight
//! the event loop for a borrow of the state. Each due timer fires at most
//! once per pass and re-arms from `now` (not from the missed deadline), so
//! a late wake-up never causes a burst of catch-up fires — LVGL semantics.

use std::time::{Duration, Instant};

struct Timer<S> {
    period: Duration,
    next_fire: Instant,
    cb: Box<dyn FnMut(&mut S)>,
}

/// A list of repeating timers, driven by the owning thread's event loop:
/// sleep/`recv_timeout` until [`TimerList::next_deadline`], then
/// [`TimerList::fire_due`].
pub struct TimerList<S> {
    timers: Vec<Timer<S>>,
}

impl<S> TimerList<S> {
    pub fn new() -> Self {
        Self { timers: Vec::new() }
    }

    /// Register a repeating timer: `cb` runs each time `period` elapses,
    /// then the timer re-arms. Pause/resume/delete (LVGL's full timer API)
    /// can layer on top later; for now the list only grows.
    pub fn register(&mut self, period: Duration, cb: impl FnMut(&mut S) + 'static) {
        self.timers.push(Timer {
            period,
            next_fire: Instant::now() + period,
            cb: Box::new(cb),
        });
    }

    /// The earliest deadline across all timers. The loop waits up to this,
    /// then calls [`TimerList::fire_due`].
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|t| t.next_fire).min()
    }

    /// Fire every timer whose deadline has passed, once each, then re-arm
    /// it from `now`. Callbacks run in registration order.
    pub fn fire_due(&mut self, now: Instant, state: &mut S) {
        for i in 0..self.timers.len() {
            if now >= self.timers[i].next_fire {
                (self.timers[i].cb)(state);
                self.timers[i].next_fire = now + self.timers[i].period;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_due_timer_once_and_reschedules_from_now() {
        let mut list: TimerList<usize> = TimerList::new();
        let mut fires = 0;
        list.register(Duration::from_millis(10), |c: &mut usize| *c += 1);

        let t0 = Instant::now();
        // Not due yet (registered at t0-ish, first deadline ~t0 + 10ms).
        list.fire_due(t0 + Duration::from_millis(5), &mut fires);
        assert_eq!(fires, 0, "must not fire before the deadline");

        // Due: fires once, re-arms exactly `period` after the fire time.
        let t_fire = t0 + Duration::from_millis(20);
        list.fire_due(t_fire, &mut fires);
        assert_eq!(fires, 1);
        assert_eq!(
            list.next_deadline(),
            Some(t_fire + Duration::from_millis(10)),
            "re-armed from the fire time, not the missed deadline"
        );

        // Late wake-up: fires once, no catch-up burst.
        list.fire_due(t_fire + Duration::from_millis(50), &mut fires);
        assert_eq!(fires, 2, "late pass must fire exactly once");
    }

    #[test]
    fn multiple_timers_fire_independently() {
        let mut list: TimerList<(usize, usize)> = TimerList::new();
        let mut counts = (0, 0);
        list.register(Duration::from_millis(10), |c: &mut (usize, usize)| c.0 += 1);
        list.register(Duration::from_millis(20), |c: &mut (usize, usize)| c.1 += 1);

        let t0 = Instant::now();
        list.fire_due(t0 + Duration::from_millis(15), &mut counts);
        assert_eq!(counts, (1, 0), "only the 10ms timer is due at 15ms");

        list.fire_due(t0 + Duration::from_millis(25), &mut counts);
        assert_eq!(counts, (2, 1), "both due at 25ms");
    }
}
