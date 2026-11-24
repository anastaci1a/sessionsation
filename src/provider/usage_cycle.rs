//! Reset cycles of account usage windows (contract 1.65.0).
//!
//! Every meter window (`usage.meter.windows.*`) and every legacy usage window (`usage.short`,
//! `usage.weekly`) names the reset cycle it belongs to: `cycle` (an opaque id) and
//! `cycle_started_at` (Unix seconds), plus `cycle_reason`. The marker changes on ANY reset of
//! that window:
//!
//! - `natural`: a new `resets_at` (more than [`SAME_CYCLE_SECONDS`] later than the window's).
//!   `cycle_started_at` is the window's own start, `resets_at` minus its duration.
//! - `redeem`: this account's own `api account reset-credit` answered `reset` for a credit that
//!   clears the window. `cycle_started_at` is when the claim was answered.
//! - `external_redeem`: a reset made elsewhere (Claude Code's `/limit-reset`, another Codex
//!   client), seen as a reset credit's remaining count falling between two reads.
//!   `cycle_started_at` is the read that saw it.
//! - `provider_lowered`: the provider reported less usage within one `resets_at` with no redeem
//!   seen, which only a reset can do. Codex: its app-server answer. Claude: two OAuth reads at
//!   least [`CONFIRM_SECONDS`] apart (one stale answer never lowers the meter).
//!
//! The rule, per marker: within one cycle `used_percent` never goes down. A lower Claude reading
//! in the same cycle is a stale replay and is kept out (the 1.56.0 rule). A redeem starts a new
//! cycle even when `resets_at` stays the same; an OAuth reading taken after it closes that cycle
//! even when lower, a statusline reading only when it is not lower than the recorded value. A
//! reading taken before the cycle started, or older than the recorded one, is kept out.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Two readings of one window whose `resets_at` differ by at most this much are the same reset
/// cycle: the Claude statusline gets whole seconds from a response header, the OAuth read an ISO
/// 8601 time, and Codex computes it from a remaining duration. A new cycle starts hours later (a
/// 5-hour or 7-day window), never minutes.
pub const SAME_CYCLE_SECONDS: i64 = 300;
/// A lower OAuth reading within one closed cycle is accepted (a reset no read saw) once a second
/// OAuth read at least this much later reports the same or less.
pub const CONFIRM_SECONDS: i64 = 60;
pub const NATURAL: &str = "natural";
pub const REDEEM: &str = "redeem";
pub const EXTERNAL_REDEEM: &str = "external_redeem";
pub const PROVIDER_LOWERED: &str = "provider_lowered";
pub const REASONS: &[&str] = &[NATURAL, REDEEM, EXTERNAL_REDEEM, PROVIDER_LOWERED];
/// The windows every meter has, with their durations in seconds.
pub const WINDOWS: [(&str, i64); 2] = [("five_hour", 300 * 60), ("seven_day", 10_080 * 60)];

/// The reset cycle of one window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cycle {
    pub id: String,
    pub started_at: i64,
    pub reason: String,
    /// The window's `resets_at` as last seen in this cycle.
    pub resets_at: i64,
    /// A redeem started this cycle and no reading has closed it yet (see [`Slot::apply`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open: bool,
    /// A cycle a reset started: the window's `used_percent` before it. An idle Claude session
    /// re-renders the limits of its last API response, so a replayable (statusline) reading
    /// within 0.5 of this value, while the cycle is open or below it, is that replay and is kept
    /// out. The filter ends when an OAuth reading shows usage back at this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_percent: Option<f64>,
    /// A lower OAuth reading of this closed cycle waiting for a second OAuth read at least
    /// [`CONFIRM_SECONDS`] later: `(used_percent, at)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_lower: Option<(f64, i64)>,
}

impl Cycle {
    /// The cycle a window's own `resets_at` implies.
    pub fn natural(resets_at: i64, duration_seconds: i64) -> Self {
        let started_at = resets_at - duration_seconds;
        Self {
            id: format!("{resets_at}:{started_at}:{NATURAL}"),
            started_at,
            reason: NATURAL.into(),
            resets_at,
            open: false,
            prior_percent: None,
            pending_lower: None,
        }
    }
    /// The natural cycle of a window that starts on first use, seen not started at `at`. Its
    /// `resets_at` anchor is `at`: the window's first real reset time is a later cycle.
    pub fn first_use(at: i64) -> Self {
        Self {
            id: format!("{at}:{at}:first_use"),
            started_at: at,
            reason: NATURAL.into(),
            resets_at: at,
            open: false,
            prior_percent: None,
            pending_lower: None,
        }
    }
    /// A cycle a reset started at `at` (`reason` [`REDEEM`], [`EXTERNAL_REDEEM`] or
    /// [`PROVIDER_LOWERED`]); `prior` is the usage it replaced.
    pub fn reset(resets_at: i64, at: i64, reason: &str, prior: Option<f64>, open: bool) -> Self {
        Self {
            id: format!("{resets_at}:{at}:{reason}"),
            started_at: at,
            reason: reason.into(),
            resets_at,
            open,
            prior_percent: prior,
            pending_lower: None,
        }
    }
    /// `cycle`, `cycle_started_at` and `cycle_reason` of a public window.
    pub fn stamp(&self, window: &mut Value) {
        if let Some(w) = window.as_object_mut() {
            w.insert("cycle".into(), json!(self.id));
            w.insert("cycle_started_at".into(), json!(self.started_at));
            w.insert("cycle_reason".into(), json!(self.reason));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Older,
    Same,
    Later,
}
/// How a reading's `resets_at` relates to the recorded one.
pub fn relation(reading: i64, recorded: i64) -> Relation {
    if reading < recorded - SAME_CYCLE_SECONDS {
        Relation::Older
    } else if reading > recorded + SAME_CYCLE_SECONDS {
        Relation::Later
    } else {
        Relation::Same
    }
}

/// Fill the cycle of a public window that has none (a record written before 1.65.0): the
/// natural cycle of its `resets_at`. `duration_seconds` is the window's length.
pub fn fill(window: &mut Value, duration_seconds: i64) {
    if !window.is_object() || window["cycle"].is_string() {
        return;
    }
    if let Some(resets) = window["resets_at"].as_i64() {
        Cycle::natural(resets, duration_seconds).stamp(window);
    }
}

// ------------------------------------------------------------------ Claude

/// One Claude window as the recorder keeps it: the window (`{used_percentage, resets_at,
/// observed_at, source}` or null) and its cycle, which outlives a cleared (null) window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub window: Value,
    #[serde(default)]
    pub cycle: Option<Cycle>,
}

/// One reading of a window offered to a [`Slot`].
pub struct Reading<'a> {
    /// `{used_percentage, resets_at}` or null (nothing current reported).
    pub window: &'a Value,
    /// When the reading was taken (an OAuth read: when its request was sent).
    pub at: i64,
    pub source: &'a str,
    /// The reading is replayable: Claude Code's statusline re-renders its last API response.
    /// Anything else is a current answer of the provider (the OAuth read).
    pub replayable: bool,
    /// The OAuth answer names the window with no reset time and no usage: it has not started
    /// and starts on first use (after its reset on an idle account, or after a redeem).
    pub first_use: bool,
}

/// What the rule does with one reading of the recorded cycle.
enum Verdict {
    Keep,
    Accept,
    /// Accept, starting a `provider_lowered` cycle (a confirmed lower OAuth reading).
    Lowered,
}

impl Slot {
    fn used(&self) -> Option<f64> {
        self.window["used_percentage"].as_f64()
    }
    fn resets(&self) -> Option<i64> {
        self.window["resets_at"].as_i64()
    }
    /// The window has not started (0%, no reset time) and starts on first use.
    pub fn first_use(&self) -> bool {
        self.window["starts_on_first_use"] == true
    }
    /// Past its own reset (a window that starts on first use never is).
    fn expired(&self, now: i64) -> bool {
        !self.first_use() && self.resets().is_none_or(|t| t <= now)
    }
    /// The cycle of a window recorded before 1.65.0 is its natural one.
    pub fn ensure_cycle(&mut self, duration_seconds: i64) {
        if self.cycle.is_none()
            && let Some(r) = self.resets()
        {
            self.cycle = Some(Cycle::natural(r, duration_seconds));
        }
    }
    /// Apply one reading under the cycle rule. Returns whether the slot changed.
    pub fn apply(&mut self, r: &Reading, now: i64, duration_seconds: i64) -> bool {
        let before = self.clone();
        self.ensure_cycle(duration_seconds);
        let used = r.window["used_percentage"].as_f64();
        let resets = r.window["resets_at"].as_i64().filter(|t| *t > now);
        // A reading taken before the reset that started this cycle, or before the recorded one,
        // says nothing current.
        let too_old = self
            .cycle
            .as_ref()
            .is_some_and(|c| c.reason != NATURAL && r.at < c.started_at)
            || self.window["observed_at"]
                .as_i64()
                .is_some_and(|t| r.at < t);
        let (Some(used), Some(f)) = (used, resets) else {
            // An OAuth answer says the window has not started. After its reset, after a redeem
            // or before any reading, that is the window now: 0% until first use. While a
            // recorded window still runs, one such answer can be stale and is kept out.
            let open = self.cycle.as_ref().is_some_and(|c| c.open);
            if r.first_use
                && !too_old
                && (open || self.first_use() || self.window.is_null() || self.expired(now))
            {
                let was_first = self.first_use();
                match &mut self.cycle {
                    // A redeem's cycle stays open: the window's first real reading is its own.
                    Some(c) if open => c.pending_lower = None,
                    Some(_) if was_first => {}
                    _ => self.cycle = Some(Cycle::first_use(r.at)),
                }
                self.window = json!({"used_percentage":0.0,"resets_at":null,"observed_at":r.at,
                    "source":r.source,"starts_on_first_use":true});
                return *self != before;
            }
            // Nothing current: carry the recorded window until its own reset.
            if self.expired(now) {
                self.window = Value::Null;
            }
            return *self != before;
        };
        let anchor = self
            .resets()
            .filter(|t| *t > now)
            .or(self.cycle.as_ref().map(|c| c.resets_at));
        let open = self.cycle.as_ref().is_some_and(|c| c.open);
        let verdict = match anchor.map(|a| relation(f, a)) {
            // A replay of an earlier cycle never replaces the current one.
            Some(Relation::Older) => Verdict::Keep,
            _ if too_old => Verdict::Keep,
            // The first window a redeem started (a cleared 5-hour window starts anew).
            Some(Relation::Later) if open => Verdict::Accept,
            None | Some(Relation::Later) => {
                self.cycle = Some(Cycle::natural(f, duration_seconds));
                Verdict::Accept
            }
            Some(Relation::Same) => self.same_cycle(r, used),
        };
        match verdict {
            Verdict::Keep => {
                if self.expired(now) && self.resets().is_some() {
                    self.window = Value::Null;
                }
                return *self != before;
            }
            Verdict::Lowered => {
                let prior = self.used();
                self.cycle = Some(Cycle::reset(f, r.at, PROVIDER_LOWERED, prior, false));
            }
            Verdict::Accept => {}
        }
        if let Some(c) = &mut self.cycle {
            c.open = false;
            c.resets_at = f;
            if !r.replayable {
                // A current answer: no lower reading is waiting any more, and once usage is
                // back at the pre-reset value the replay filter has nothing left to hide.
                c.pending_lower = None;
                if c.prior_percent.is_some_and(|p| used >= p - 0.5) {
                    c.prior_percent = None;
                }
            }
        }
        self.window =
            json!({"used_percentage":used,"resets_at":f,"observed_at":r.at,"source":r.source});
        *self != before
    }
    /// The verdict for a reading of the recorded cycle (`resets_at` within
    /// [`SAME_CYCLE_SECONDS`]).
    fn same_cycle(&mut self, r: &Reading, used: f64) -> Verdict {
        let old = self.used();
        let old_at = self.window["observed_at"].as_i64();
        let Some(cycle) = self.cycle.as_mut() else {
            return Verdict::Accept;
        };
        let lower = old.is_some_and(|o| used < o);
        if r.replayable {
            let replay = cycle.prior_percent.is_some_and(|p| {
                (used - p).abs() < 0.5 && (cycle.open || old.is_none_or(|o| o < p - 0.5))
            });
            // A statusline reading never lowers a cycle, open or not (another session's last
            // response can be older than the redeem).
            if replay || lower {
                return Verdict::Keep;
            }
        } else if cycle.open {
            // The first OAuth answer after a redeem is the post-reset value.
            return Verdict::Accept;
        } else if lower {
            // One lower OAuth answer can be stale; a second one at least CONFIRM_SECONDS later
            // with the same or less confirms a reset no read saw.
            return match cycle.pending_lower {
                Some((u, at)) if r.at - at >= CONFIRM_SECONDS && used <= u => Verdict::Lowered,
                Some((_, at)) if r.at - at < CONFIRM_SECONDS => Verdict::Keep,
                _ => {
                    cycle.pending_lower = Some((used, r.at));
                    Verdict::Keep
                }
            };
        }
        match old {
            // An equal value is current unless it is older than what is recorded.
            Some(o) if used == o && old_at.is_some_and(|t| r.at < t) => Verdict::Keep,
            _ => Verdict::Accept,
        }
    }
    /// Start a new cycle at `at` because the window was reset (`reason` [`REDEEM`] or
    /// [`EXTERNAL_REDEEM`]). The recorded value stays until a reading closes the cycle.
    /// Returns false when nothing is known about the window.
    pub fn start(&mut self, at: i64, reason: &str, duration_seconds: i64) -> bool {
        self.ensure_cycle(duration_seconds);
        let resets = self
            .resets()
            .filter(|t| *t > at)
            .or(self.cycle.as_ref().map(|c| c.resets_at));
        let Some(resets) = resets else {
            return false;
        };
        self.cycle = Some(Cycle::reset(resets, at, reason, self.used(), true));
        true
    }
    /// The window as `usage.json` and the projection carry it (with its cycle), or null.
    pub fn public(&self) -> Value {
        let mut w = self.window.clone();
        if let Some(c) = &self.cycle {
            c.stamp(&mut w);
        }
        w
    }
}

// ------------------------------------------------------------------- Codex

/// The cycle of one Codex window after an accepted reading. `prev` is the window it replaces,
/// `redeem` an own (`(at, REDEEM)`) or external (`(at, EXTERNAL_REDEEM)`) reset seen with it.
pub fn codex(
    prev_cycle: Option<&Cycle>,
    prev: Option<(f64, i64)>,
    new: Option<(f64, i64)>,
    duration_seconds: i64,
    at: i64,
    redeem: Option<&str>,
) -> Option<Cycle> {
    let Some((used, resets)) = new else {
        return prev_cycle.cloned();
    };
    let prior = prev.map(|(u, _)| u);
    if let Some(reason) = redeem {
        return Some(Cycle::reset(resets, at, reason, prior, false));
    }
    let same = prev_cycle
        .filter(|c| relation(resets, c.resets_at) == Relation::Same)
        .cloned();
    match same {
        None => Some(Cycle::natural(resets, duration_seconds)),
        Some(_) if prev.is_some_and(|(u, r)| relation(resets, r) == Relation::Same && used < u) => {
            Some(Cycle::reset(resets, at, PROVIDER_LOWERED, prior, false))
        }
        Some(mut c) => {
            c.resets_at = resets;
            Some(c)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(w: &Value, at: i64, replayable: bool) -> Reading<'_> {
        Reading {
            window: w,
            at,
            source: if replayable { "statusline" } else { "oauth" },
            replayable,
            first_use: false,
        }
    }
    fn w(used: f64, resets: i64) -> Value {
        json!({"used_percentage":used,"resets_at":resets})
    }

    #[test]
    fn a_cycle_never_goes_down_and_a_redeem_starts_one_that_accepts_the_lower_reading() {
        let (now, r) = (1_000_000, 1_000_000 + 86_400);
        let week = 10_080 * 60;
        let mut s = Slot::default();
        assert!(s.apply(&reading(&w(20.0, r), now, true), now, week));
        let natural = s.cycle.clone().unwrap();
        assert_eq!(natural.reason, NATURAL);
        assert_eq!(natural.started_at, r - week);
        // A lower reading of the same cycle is stale.
        assert!(!s.apply(&reading(&w(10.0, r), now + 60, true), now + 60, week));
        assert_eq!(s.window["used_percentage"], 20.0);
        // A redeem starts a new cycle with the same resets_at.
        assert!(s.start(now + 100, REDEEM, week));
        let redeemed = s.cycle.clone().unwrap();
        assert_ne!(redeemed.id, natural.id);
        assert_eq!(redeemed.started_at, now + 100);
        assert_eq!(redeemed.resets_at, r);
        // While it is open: a replay of the pre-redeem response, a lower statusline reading of
        // another session and an OAuth answer sent before the redeem are kept out.
        assert!(!s.apply(&reading(&w(20.0, r), now + 110, true), now + 110, week));
        assert!(!s.apply(&reading(&w(5.0, r), now + 112, true), now + 112, week));
        assert!(!s.apply(&reading(&w(10.0, r), now + 99, false), now + 115, week));
        assert!(s.cycle.as_ref().unwrap().open);
        // The first OAuth answer after it closes the cycle, lower.
        assert!(s.apply(&reading(&w(10.0, r), now + 120, false), now + 120, week));
        assert_eq!(s.window["used_percentage"], 10.0);
        assert_eq!(s.cycle.as_ref().unwrap().id, redeemed.id);
        assert!(!s.cycle.as_ref().unwrap().open);
        // Then the rule holds again, and the replay stays out (an older recorder rewriting the
        // pre-redeem value is one such reading).
        assert!(!s.apply(&reading(&w(9.0, r), now + 130, true), now + 130, week));
        assert!(!s.apply(&reading(&w(20.0, r), now + 140, true), now + 140, week));
        assert_eq!(s.window["used_percentage"], 10.0);
        // A reading older than the recorded one is kept out.
        assert!(!s.apply(&reading(&w(15.0, r), now + 115, true), now + 145, week));
        // Real growth past the old value is accepted.
        assert!(s.apply(&reading(&w(21.0, r), now + 150, true), now + 150, week));
        // A natural reset is a new cycle.
        let next = w(1.0, r + week);
        assert!(s.apply(&reading(&next, now + 160, true), now + 160, week));
        let c = s.cycle.unwrap();
        assert_eq!((c.reason.as_str(), c.started_at), (NATURAL, r));
        assert_eq!(s.window["used_percentage"], 1.0);
    }

    #[test]
    fn a_statusline_reading_closes_an_open_cycle_only_when_not_lower() {
        let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
        let mut s = Slot::default();
        s.apply(&reading(&w(20.0, r), now, true), now, week);
        s.start(now + 10, REDEEM, week);
        assert!(s.apply(&reading(&w(22.0, r), now + 20, true), now + 20, week));
        assert!(!s.cycle.as_ref().unwrap().open);
        assert_eq!(s.window["used_percentage"], 22.0);
    }

    #[test]
    fn the_replay_filter_ends_once_an_oauth_reading_is_back_at_the_prior_value() {
        let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
        let mut s = Slot::default();
        s.apply(&reading(&w(20.0, r), now, true), now, week);
        s.start(now + 10, REDEEM, week);
        s.apply(&reading(&w(10.0, r), now + 20, false), now + 20, week);
        assert_eq!(s.cycle.as_ref().unwrap().prior_percent, Some(20.0));
        // Usage really grows back: the OAuth read sees 20, the filter ends, and the statusline
        // value of exactly 20 counts again.
        assert!(s.apply(&reading(&w(20.0, r), now + 30, false), now + 30, week));
        assert_eq!(s.cycle.as_ref().unwrap().prior_percent, None);
        assert!(s.apply(&reading(&w(20.0, r), now + 40, true), now + 40, week));
        assert_eq!(s.window["source"], "statusline");
    }

    #[test]
    fn a_lower_oauth_reading_is_accepted_only_when_a_second_read_confirms_it() {
        let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
        let mut s = Slot::default();
        s.apply(&reading(&w(20.0, r), now, true), now, week);
        let natural = s.cycle.clone().unwrap().id;
        // One lower OAuth answer: kept out, waiting.
        s.apply(&reading(&w(10.0, r), now + 10, false), now + 10, week);
        assert_eq!(s.window["used_percentage"], 20.0);
        assert_eq!(
            s.cycle.as_ref().unwrap().pending_lower,
            Some((10.0, now + 10))
        );
        // A second one too soon is no confirmation.
        s.apply(&reading(&w(10.0, r), now + 30, false), now + 30, week);
        assert_eq!(s.window["used_percentage"], 20.0);
        // A not-lower OAuth answer clears the wait.
        s.apply(&reading(&w(20.0, r), now + 40, false), now + 40, week);
        assert_eq!(s.cycle.as_ref().unwrap().pending_lower, None);
        s.apply(&reading(&w(10.0, r), now + 50, false), now + 50, week);
        // Confirmed at least CONFIRM_SECONDS later with the same or less: a new cycle.
        assert!(s.apply(
            &reading(&w(9.0, r), now + 50 + CONFIRM_SECONDS, false),
            now + 50 + CONFIRM_SECONDS,
            week
        ));
        assert_eq!(s.window["used_percentage"], 9.0);
        let c = s.cycle.clone().unwrap();
        assert_eq!(c.reason, PROVIDER_LOWERED);
        assert_ne!(c.id, natural);
        assert_eq!(c.prior_percent, Some(20.0));
        // A statusline replay of the stale value stays out after the heal.
        assert!(!s.apply(&reading(&w(20.0, r), now + 200, true), now + 200, week));
        // A confirmation with more usage than the waiting reading does not lower.
        let mut t = Slot::default();
        t.apply(&reading(&w(20.0, r), now, true), now, week);
        t.apply(&reading(&w(10.0, r), now + 10, false), now + 10, week);
        t.apply(&reading(&w(12.0, r), now + 100, false), now + 100, week);
        assert_eq!(t.window["used_percentage"], 20.0);
    }

    #[test]
    fn a_cleared_window_with_no_current_one_starts_on_first_use() {
        let (now, r5) = (1_000_000, 1_000_000 + 3_600);
        let five = 300 * 60;
        let mut s = Slot::default();
        s.apply(&reading(&w(80.0, r5), now, true), now, five);
        assert!(s.start(now + 10, REDEEM, five));
        let id = s.cycle.clone().unwrap().id;
        let none = Value::Null;
        let cleared = Reading {
            window: &none,
            at: now + 20,
            source: "oauth",
            replayable: false,
            first_use: true,
        };
        // After a redeem: 0% until first use, still the redeem's cycle.
        assert!(s.apply(&cleared, now + 20, five));
        assert!(s.first_use());
        assert_eq!(s.window["used_percentage"], 0.0);
        assert!(s.window["resets_at"].is_null());
        assert_eq!(s.cycle.as_ref().unwrap().id, id);
        // The replay of the old window stays out; the next window keeps the redeem's cycle.
        assert!(!s.apply(&reading(&w(80.0, r5), now + 30, true), now + 30, five));
        let fresh = w(2.0, now + 40 + five);
        assert!(s.apply(&reading(&fresh, now + 40, true), now + 40, five));
        assert_eq!(s.cycle.as_ref().unwrap().id, id);
        // After the window's own reset on an idle account: a new natural cycle at 0%.
        let after = now + 41 + five;
        let idle = Reading {
            window: &none,
            at: after,
            source: "oauth",
            replayable: false,
            first_use: true,
        };
        assert!(s.apply(&idle, after, five));
        let c = s.cycle.clone().unwrap();
        assert_eq!(c.reason, NATURAL);
        assert_ne!(c.id, id);
        assert!(s.first_use());
        // A not-started answer while a recorded window still runs is kept out.
        let mut t = Slot::default();
        t.apply(&reading(&w(30.0, r5), now, true), now, five);
        assert!(!t.apply(&cleared, now + 20, five));
        assert_eq!(t.window["used_percentage"], 30.0);
    }

    #[test]
    fn codex_cycles_change_on_new_resets_redeems_and_lowered_readings() {
        let (at, r, five) = (1_000, 20_000, 300 * 60);
        let first = codex(None, None, Some((23.0, r)), five, at, None).unwrap();
        assert_eq!(first.reason, NATURAL);
        // Clock drift within a cycle keeps it.
        let same = codex(
            Some(&first),
            Some((23.0, r)),
            Some((24.0, r + 3)),
            five,
            at + 1,
            None,
        );
        assert_eq!(same.as_ref().unwrap().id, first.id);
        let lowered = codex(
            same.as_ref(),
            Some((24.0, r + 3)),
            Some((5.0, r + 4)),
            five,
            at + 2,
            None,
        )
        .unwrap();
        assert_eq!(lowered.reason, PROVIDER_LOWERED);
        let redeemed = codex(
            Some(&lowered),
            Some((5.0, r)),
            Some((0.0, r)),
            five,
            at + 3,
            Some(REDEEM),
        )
        .unwrap();
        assert_eq!(
            (redeemed.reason.as_str(), redeemed.started_at),
            (REDEEM, at + 3)
        );
        let next = codex(
            Some(&redeemed),
            Some((0.0, r)),
            Some((1.0, r + five)),
            five,
            at + 4,
            None,
        )
        .unwrap();
        assert_eq!((next.reason.as_str(), next.started_at), (NATURAL, r));
        // An absent window keeps its cycle.
        assert_eq!(
            codex(Some(&next), None, None, five, at + 5, None),
            Some(next)
        );
    }
}
