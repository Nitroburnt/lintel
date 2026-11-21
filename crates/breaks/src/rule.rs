// The break rule: when a break is due, held off, on screen and over. No clock and no Windows in
// here: time comes in as whole seconds, so the probe runs it over a made-up timeline.
//
// Each kind counts the seconds the person was at the PC. Away (idle, locked, asleep) nothing
// counts, and an absence as long as the break was the break. A break that is due waits while the
// person is busy (a call, something full screen) and is asked about again every `RECHECK` seconds.

use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Eye,
    Move,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Eye => "eye",
            Kind::Move => "move",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "eye" => Some(Kind::Eye),
            "move" => Some(Kind::Move),
            _ => None,
        }
    }
}

/// One kind of break: after `every` seconds at the PC, `secs` seconds of break.
#[derive(Clone, Copy, Default, Debug)]
pub struct Plan {
    pub on: bool,
    pub every: u64,
    pub secs: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Config {
    pub eye: Plan,
    pub mov: Plan,
    /// A snoozed break is due again after this many seconds at the PC.
    pub snooze: u64,
    /// Says `Soon` this many seconds before a break is due; 0 = never (only the fade warns).
    pub warn: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ev {
    /// The break is `Config::warn` seconds away.
    Soon(Kind),
    /// Due, but the person is busy: it waits.
    Held(Kind),
    /// On screen for this many seconds.
    Start(Kind, u64),
    /// Over: "done", "skip", "snooze", "away" (locked or asleep meanwhile) or "off".
    End(Kind, &'static str),
}

/// A held break asks again this often, so it starts within this long of the call ending.
pub const RECHECK: u64 = 10;

#[derive(Default)]
pub struct Rule {
    pub cfg: Config,
    /// Seconds at the PC since the last break, by kind.
    active: [u64; 2],
    /// Seconds of the absence that is going on.
    away: u64,
    /// The break on screen and the seconds it has left.
    now: Option<(Kind, u64)>,
    held: bool,
    wait: u64,
}

impl Rule {
    fn plan(&self, kind: Kind) -> Plan {
        match kind {
            Kind::Eye => self.cfg.eye,
            Kind::Move => self.cfg.mov,
        }
    }

    pub fn on_break(&self) -> Option<Kind> {
        self.now.map(|n| n.0)
    }

    /// `dt` seconds went by. `away`: idle, locked or asleep for all of them. `busy` is asked only
    /// when a break is due: is the person on a call, or is something full screen?
    pub fn tick(&mut self, dt: u64, away: bool, busy: impl FnOnce() -> bool) -> Vec<Ev> {
        let mut out = Vec::new();
        if let Some((kind, left)) = self.now {
            // A break runs on the clock, whatever the person does meanwhile.
            if dt >= left {
                out.push(self.end(kind, "done"));
            } else {
                self.now = Some((kind, left - dt));
            }
            return out;
        }
        if away {
            self.away += dt;
            for kind in [Kind::Eye, Kind::Move] {
                if self.away >= self.plan(kind).secs {
                    self.active[kind as usize] = 0;
                }
            }
            (self.held, self.wait) = (false, 0);
            return out;
        }
        self.away = 0;
        for kind in [Kind::Eye, Kind::Move] {
            let plan = self.plan(kind);
            if !plan.on {
                self.active[kind as usize] = 0;
                continue;
            }
            let before = self.active[kind as usize];
            let after = before + dt;
            self.active[kind as usize] = after;
            let warn_at = plan.every.saturating_sub(self.cfg.warn);
            if self.cfg.warn > 0 && before < warn_at && after >= warn_at && after < plan.every {
                out.push(Ev::Soon(kind));
            }
        }
        // Movement first: it covers an eye break due at the same time.
        let due = |kind: Kind| self.plan(kind).on && self.active[kind as usize] >= self.plan(kind).every;
        let Some(kind) = [Kind::Move, Kind::Eye].into_iter().find(|k| due(*k)) else {
            (self.held, self.wait) = (false, 0);
            return out;
        };
        if self.wait > dt {
            self.wait -= dt;
            return out;
        }
        if busy() {
            if !self.held {
                self.held = true;
                out.push(Ev::Held(kind));
            }
            self.wait = RECHECK;
            return out;
        }
        out.push(self.start(kind));
        out
    }

    fn start(&mut self, kind: Kind) -> Ev {
        let secs = self.plan(kind).secs.max(1);
        self.now = Some((kind, secs));
        (self.held, self.wait) = (false, 0);
        Ev::Start(kind, secs)
    }

    fn end(&mut self, kind: Kind, why: &'static str) -> Ev {
        self.now = None;
        let plan = self.plan(kind);
        if why == "snooze" {
            self.active[kind as usize] = plan.every.saturating_sub(self.cfg.snooze.max(1));
        } else {
            self.active[kind as usize] = 0;
            // Standing up rests the eyes as well.
            if kind == Kind::Move {
                self.active[Kind::Eye as usize] = 0;
            }
        }
        Ev::End(kind, why)
    }

    /// Ends the break on screen, if there is one.
    pub fn stop(&mut self, why: &'static str) -> Option<Ev> {
        self.now.map(|(kind, _)| self.end(kind, why))
    }

    /// Skip: ends the break on screen; with none on screen, calls off the one that was announced.
    pub fn skip(&mut self) -> Option<Ev> {
        if self.now.is_some() {
            return self.stop("skip");
        }
        for kind in [Kind::Eye, Kind::Move] {
            let plan = self.plan(kind);
            if plan.on && self.active[kind as usize] + self.cfg.warn >= plan.every {
                self.active[kind as usize] = 0;
            }
        }
        None
    }

    /// A break now, asked for by hand (it does not wait for a call to end).
    pub fn take(&mut self, kind: Kind) -> Option<Ev> {
        self.now.is_none().then(|| self.start(kind))
    }

    /// For the page: the break on screen, and the seconds at the PC until the next one of each kind.
    pub fn status(&self) -> Value {
        let next = |kind: Kind| {
            let plan = self.plan(kind);
            plan.on.then(|| plan.every.saturating_sub(self.active[kind as usize]))
        };
        json!({
            "break": self.now.map(|(kind, left)| json!({ "kind": kind.name(), "left": left, "total": self.plan(kind).secs.max(1) })),
            "eye": next(Kind::Eye),
            "move": next(Kind::Move),
            "held": self.held,
        })
    }
}

// ── Quiet hours ───────────────────────────────────────────────────────────────

/// "HH:MM" on the 24-hour clock as minutes of the day; anything else is `None`.
pub fn minutes_of(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Whether `at` is inside the quiet window `from`–`to`, both minutes of the day. An end before
/// the start crosses midnight (22:00–07:00); one equal to the start is no window at all.
pub fn within(at: u32, from: u32, to: u32) -> bool {
    if from == to {
        return false;
    }
    if from < to {
        at >= from && at < to
    } else {
        at >= from || at < to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_time_is_read_strictly() {
        assert_eq!(minutes_of("00:00"), Some(0));
        assert_eq!(minutes_of("09:30"), Some(570));
        assert_eq!(minutes_of("23:59"), Some(1439));
        assert_eq!(minutes_of(" 7:05 "), Some(425));
        for bad in ["24:00", "12:60", "9", "09:5", "09:300", "nine:30", "-1:00", ""] {
            assert_eq!(minutes_of(bad), None, "{bad} was taken");
        }
    }

    #[test]
    fn a_window_of_the_day_holds_its_start_and_lets_its_end_go() {
        assert!(within(9 * 60, 9 * 60, 10 * 60));
        assert!(within(9 * 60 + 59, 9 * 60, 10 * 60));
        assert!(!within(10 * 60, 9 * 60, 10 * 60));
        assert!(!within(8 * 60 + 59, 9 * 60, 10 * 60));
    }

    #[test]
    fn a_window_over_midnight_holds_both_ends() {
        assert!(within(22 * 60, 22 * 60, 7 * 60));
        assert!(within(23 * 60 + 30, 22 * 60, 7 * 60));
        assert!(within(0, 22 * 60, 7 * 60));
        assert!(within(6 * 60 + 59, 22 * 60, 7 * 60));
        assert!(!within(7 * 60, 22 * 60, 7 * 60));
        assert!(!within(12 * 60, 22 * 60, 7 * 60));
    }

    #[test]
    fn an_empty_window_never_holds() {
        assert!(!within(0, 0, 0));
        assert!(!within(12 * 60, 9 * 60, 9 * 60));
    }

    #[test]
    fn a_break_waits_while_the_reason_says_busy_and_starts_when_it_ends() {
        let mut rule = Rule {
            cfg: Config {
                eye: Plan { on: true, every: 10, secs: 5 },
                mov: Plan::default(),
                snooze: 60,
                warn: 0,
            },
            ..Default::default()
        };
        assert!(rule.tick(5, false, || false).is_empty());
        assert_eq!(rule.tick(5, false, || true), vec![Ev::Held(Kind::Eye)]);
        // It asks again after RECHECK; the ask still waits until the reason is gone.
        assert!(rule.tick(RECHECK, false, || true).is_empty());
        assert_eq!(rule.tick(RECHECK, false, || false), vec![Ev::Start(Kind::Eye, 5)]);
    }
}
