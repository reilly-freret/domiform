//! Rules: `trigger + condition + commands`.
//!
//! A rule fires when an incoming event matches its `trigger` AND its
//! `condition` (a small predicate algebra over the current state store) holds.
//! It then emits its `commands`. Rules are pure with respect to the state store
//! and the triggering event, which makes them independently testable — see the
//! unit tests at the bottom of this file, which exercise the condition algebra
//! against a hand-built `StateStore` with no engine at all.

use crate::ids::{ActionId, DeviceId, RuleId, ScheduleId};
use crate::model::{CapabilityKind, CapabilityState, Command, Event, Millis, TimerKey};
use crate::state::StateStore;

/// Direction of a numeric threshold crossing for a `Crosses` trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossDir {
    /// Fire when the value moves from below the bound to at/above it.
    Above,
    /// Fire when the value moves from above the bound to at/below it.
    Below,
}

/// What kind of event wakes this rule. Note `Timer` and `Time` make scheduler
/// events first-class triggers — identical in shape to physical events.
///
/// The on-change family (`Changed`, `Crosses`, `Reports`) all match against the
/// single `Event::StateReported` — occupancy, contact, temperature and every
/// other stateful capability ride it, with no per-capability event type. Edge
/// triggers (`Changed`, `Crosses`) need the *prior* stored value to detect a
/// transition, so `matches` takes it as context (captured before the fold in the
/// drain loop). A first-ever report (`prior == None`) that satisfies the
/// predicate **counts as an edge** — "already hot at boot → turn on the fan".
#[derive(Clone, Debug)]
pub enum Trigger {
    Action {
        device: DeviceId,
        action: ActionId,
    },
    /// Edge trigger on a bool-shaped capability changing *to* `to` (occupancy,
    /// contact, water-leak, …). Fires only on the transition into `to`, not on
    /// repeated reports of the same value.
    Changed {
        device: DeviceId,
        kind: CapabilityKind,
        to: bool,
    },
    /// Edge trigger on a numeric-shaped capability *crossing* `bound` in the
    /// given direction. Fires once when the value moves from not-satisfying to
    /// satisfying the threshold — the fan/climate case.
    Crosses {
        device: DeviceId,
        kind: CapabilityKind,
        bound: i64,
        dir: CrossDir,
    },
    /// Level trigger: fires on *every* report of the capability, regardless of
    /// whether the value changed. Opt-in (metering/logging-style rules).
    Reports {
        device: DeviceId,
        kind: CapabilityKind,
    },
    Timer {
        key: TimerKey,
    },
    Time {
        schedule: ScheduleId,
    },
    /// React to a command the engine gave up on. `device: None` matches any
    /// failure; `Some(d)` matches failures of commands targeting device `d` —
    /// the basis for "device offline → notify / fall back" rules.
    CommandFailed {
        device: Option<DeviceId>,
    },
}

impl Trigger {
    /// Whether `ev` fires this trigger. `prior` is the state store's value for the
    /// reported `(device, capability)` *before* the event was folded — required by
    /// the edge triggers (`Changed`/`Crosses`) to see the transition. It is
    /// ignored by every other trigger.
    pub fn matches(&self, ev: &Event, prior: Option<&CapabilityState>) -> bool {
        match (self, ev) {
            (
                Trigger::Action { device, action },
                Event::Action {
                    device: d,
                    action: a,
                },
            ) => device == d && action == a,
            (Trigger::Changed { device, kind, to }, Event::StateReported { device: d, state }) => {
                device == d
                    && state.kind() == *kind
                    && state.as_bool() == Some(*to)
                    // Edge: the prior value was not already `to` (a first-ever
                    // report that satisfies the predicate counts as an edge).
                    && prior.and_then(CapabilityState::as_bool) != Some(*to)
            }
            (
                Trigger::Crosses {
                    device,
                    kind,
                    bound,
                    dir,
                },
                Event::StateReported { device: d, state },
            ) => {
                if device != d || state.kind() != *kind {
                    return false;
                }
                let Some(new) = state.as_i64() else {
                    return false;
                };
                let satisfied = |v: i64| match dir {
                    CrossDir::Above => v >= *bound,
                    CrossDir::Below => v <= *bound,
                };
                // Edge: now satisfies, but the prior value did not (a first-ever
                // report that already satisfies counts as a crossing).
                let prior_satisfied = prior
                    .and_then(CapabilityState::as_i64)
                    .map(satisfied)
                    .unwrap_or(false);
                satisfied(new) && !prior_satisfied
            }
            (Trigger::Reports { device, kind }, Event::StateReported { device: d, state }) => {
                device == d && state.kind() == *kind
            }
            (Trigger::Timer { key }, Event::TimerElapsed { key: k }) => key == k,
            (Trigger::Time { schedule }, Event::TimeReached { schedule: s }) => schedule == s,
            (Trigger::CommandFailed { device }, Event::CommandFailed { command, .. }) => {
                match device {
                    None => true,
                    Some(d) => command.target_device() == Some(*d),
                }
            }
            _ => false,
        }
    }

    /// The `(device, capability)` an edge trigger watches, if any — the state a
    /// `for`-qualified rule must keep an eye on to auto-cancel its pending timer.
    /// Only the edge triggers (`Changed`, `Crosses`) support a `for` qualifier;
    /// everything else returns `None`.
    pub fn watched(&self) -> Option<(DeviceId, CapabilityKind)> {
        match self {
            Trigger::Changed { device, kind, .. } | Trigger::Crosses { device, kind, .. } => {
                Some((*device, *kind))
            }
            _ => None,
        }
    }

    /// Whether the edge trigger's *predicate* currently holds against the store —
    /// used by a `for`-qualified rule both to auto-cancel (predicate no longer
    /// holds) and to re-verify on elapse (predicate still holds). Returns `false`
    /// for non-edge triggers and for state that has never been reported (Unknown
    /// does not sustain).
    pub fn predicate_holds(&self, state: &StateStore) -> bool {
        match self {
            Trigger::Changed { device, kind, to } => state.bool_value(*device, *kind) == Some(*to),
            Trigger::Crosses {
                device,
                kind,
                bound,
                dir,
            } => match state.num_value(*device, *kind) {
                Some(v) => match dir {
                    CrossDir::Above => v >= *bound,
                    CrossDir::Below => v <= *bound,
                },
                None => false,
            },
            _ => false,
        }
    }
}

/// Comparison operators for numeric state conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

impl CmpOp {
    fn test(self, actual: i64, bound: i64) -> bool {
        match self {
            CmpOp::Lt => actual < bound,
            CmpOp::Le => actual <= bound,
            CmpOp::Eq => actual == bound,
            CmpOp::Ne => actual != bound,
            CmpOp::Ge => actual >= bound,
            CmpOp::Gt => actual > bound,
        }
    }
}

/// A three-valued (Kleene) truth. `Unknown` is a first-class outcome: it means
/// "the state this leaf depends on has never been reported." It propagates
/// through the boolean operators instead of silently collapsing to `false`, so
/// `Not(Unknown)` is `Unknown` rather than `True`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    pub fn from_bool(b: bool) -> Self {
        if b {
            Truth::True
        } else {
            Truth::False
        }
    }

    /// A rule fires only on a definite `True`. `Unknown` and `False` both hold.
    pub fn is_true(self) -> bool {
        self == Truth::True
    }

    fn not(self) -> Self {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    fn and(self, other: Truth) -> Self {
        match (self, other) {
            // Definitely false if either side is false, even when the other is unknown.
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::True, Truth::True) => Truth::True,
            _ => Truth::Unknown,
        }
    }

    fn or(self, other: Truth) -> Self {
        match (self, other) {
            // Definitely true if either side is true, even when the other is unknown.
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::False, Truth::False) => Truth::False,
            _ => Truth::Unknown,
        }
    }
}

/// A boolean expression over current state. Kept intentionally small; this is
/// the one place the engine needs an evaluator. Every leaf is a read of the
/// state store, so the synthetic clock/sun device is reachable here with no
/// special syntax — `BoolEquals { device: sun, kind: SunUp, value: false }` is
/// literally "after sunset".
///
/// **Unknown-state semantics:** if a referenced capability has never been
/// reported, the leaf evaluates to `Truth::Unknown` (not `False`), and the rule
/// does not fire. The compiler should still warn when a condition references a
/// capability that no adapter ever reports, since such a leaf is permanently
/// `Unknown` and the rule is effectively dead.
#[derive(Clone, Debug)]
pub enum Condition {
    Always,
    Not(Box<Condition>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    /// Boolean capability equals an expected value (switch / occupancy / sun-up).
    BoolEquals {
        device: DeviceId,
        kind: CapabilityKind,
        value: bool,
    },
    /// Numeric capability compared against a constant
    /// (brightness / battery / time-of-day).
    Compare {
        device: DeviceId,
        kind: CapabilityKind,
        op: CmpOp,
        value: i64,
    },
    /// Chromatic color equals an exact sRGB triple. Color is neither bool- nor
    /// numeric-shaped, so it can't ride `BoolEquals`/`Compare`; this leaf reads
    /// the stored `CapabilityState::Color` directly. Exact equality only —
    /// devices may round-trip color through HSV/XY and report a near-miss, so a
    /// `tolerance` field is a possible future addition, deliberately omitted here.
    ColorEquals {
        device: DeviceId,
        r: u8,
        g: u8,
        b: u8,
    },
}

impl Condition {
    pub fn eval(&self, state: &StateStore) -> Truth {
        match self {
            Condition::Always => Truth::True,
            Condition::Not(inner) => inner.eval(state).not(),
            Condition::And(cs) => cs.iter().fold(Truth::True, |acc, c| acc.and(c.eval(state))),
            Condition::Or(cs) => cs.iter().fold(Truth::False, |acc, c| acc.or(c.eval(state))),
            Condition::BoolEquals {
                device,
                kind,
                value,
            } => match state.bool_value(*device, *kind) {
                Some(actual) => Truth::from_bool(actual == *value),
                None => Truth::Unknown,
            },
            Condition::Compare {
                device,
                kind,
                op,
                value,
            } => match state.num_value(*device, *kind) {
                Some(actual) => Truth::from_bool(op.test(actual, *value)),
                None => Truth::Unknown,
            },
            Condition::ColorEquals { device, r, g, b } => {
                match state.get(*device, CapabilityKind::Color) {
                    Some(CapabilityState::Color {
                        r: cr,
                        g: cg,
                        b: cb,
                    }) => Truth::from_bool(cr == r && cg == g && cb == b),
                    _ => Truth::Unknown,
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: RuleId,
    /// The config name of this rule. A debug label only — the runtime matches on
    /// `id`, never the name — but carrying it on the node lets the observer trace
    /// rules by name, the same way devices and scenes are named in the tree.
    pub name: String,
    pub trigger: Trigger,
    pub condition: Condition,
    pub commands: Vec<Command>,
    /// Optional `for:` qualifier (feature E). When set, the rule's commands fire
    /// only if the edge trigger's predicate has held *continuously* for this many
    /// milliseconds: on the edge the engine schedules a timer; if the state
    /// reverts before it elapses, the timer is auto-cancelled; on elapse the
    /// predicate is re-verified before firing. All timing rides the scheduler
    /// adapter (virtual time), so it stays deterministic and replayable.
    pub for_duration: Option<Millis>,
}

impl Rule {
    pub fn new(id: RuleId, trigger: Trigger, condition: Condition, commands: Vec<Command>) -> Self {
        Rule {
            id,
            name: String::new(),
            trigger,
            condition,
            commands,
            for_duration: None,
        }
    }

    /// Attach the config name (used by the compiler; see `resolve`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Attach a `for:` sustained-duration qualifier (feature E).
    pub fn with_for_duration(mut self, for_duration: Option<Millis>) -> Self {
        self.for_duration = for_duration;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CapabilityState;

    const SUN: DeviceId = DeviceId(99);

    fn store_at(minute: u16) -> StateStore {
        let mut s = StateStore::default();
        s.set(SUN, CapabilityState::TimeOfDay(minute));
        s
    }

    // "Quiet hours" 22:00–06:00 — the across-midnight case that needs Or.
    fn quiet_hours() -> Condition {
        Condition::Or(vec![
            Condition::Compare {
                device: SUN,
                kind: CapabilityKind::TimeOfDay,
                op: CmpOp::Ge,
                value: 22 * 60,
            },
            Condition::Compare {
                device: SUN,
                kind: CapabilityKind::TimeOfDay,
                op: CmpOp::Lt,
                value: 6 * 60,
            },
        ])
    }

    #[test]
    fn quiet_hours_span_midnight() {
        assert_eq!(quiet_hours().eval(&store_at(23 * 60)), Truth::True); // 23:00
        assert_eq!(quiet_hours().eval(&store_at(2 * 60)), Truth::True); // 02:00
        assert_eq!(quiet_hours().eval(&store_at(12 * 60)), Truth::False); // noon
    }

    #[test]
    fn unknown_propagates_and_never_fires() {
        // Empty store: every leaf reads `None`, so the result is Unknown.
        let empty = StateStore::default();
        let cond = Condition::BoolEquals {
            device: SUN,
            kind: CapabilityKind::SunUp,
            value: false,
        };
        assert_eq!(cond.eval(&empty), Truth::Unknown);
        assert!(!cond.eval(&empty).is_true()); // does not fire

        // The sharp edge is gone: Not(Unknown) is Unknown, not True.
        assert_eq!(Condition::Not(Box::new(cond)).eval(&empty), Truth::Unknown);
    }

    #[test]
    fn unknown_short_circuits_like_classical_logic_when_it_can() {
        // And: a known-false beats an unknown sibling.
        let known_false = Condition::Compare {
            device: SUN,
            kind: CapabilityKind::TimeOfDay,
            op: CmpOp::Eq,
            value: 99,
        };
        let unknown = Condition::BoolEquals {
            device: SUN,
            kind: CapabilityKind::SunUp, // never set in store_at()
            value: true,
        };
        let store = store_at(12 * 60);
        assert_eq!(
            Condition::And(vec![known_false.clone(), unknown.clone()]).eval(&store),
            Truth::False
        );
        // Or: an unknown sibling alongside a non-true stays Unknown.
        assert_eq!(
            Condition::Or(vec![known_false, unknown]).eval(&store),
            Truth::Unknown
        );
    }
}
