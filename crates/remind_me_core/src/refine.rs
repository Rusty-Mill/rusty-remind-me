//! When a refinement run is due, and the record each run leaves (Mill Board
//! #1554, pack D104).
//!
//! The session ladder (ADR-0029) turns captured sessions into facts and
//! scenarios. It used to run on a daily clock, which ran on quiet days and
//! fell behind on busy ones, and Nano paused it on 2026-11-09. This module
//! decides from the material instead: a run is due when enough settled
//! captures wait, the last run is far enough back, and nothing has been
//! captured for a while (new activity resets the idle clock, so it cancels a
//! due run).
//!
//! The server only says a run is due, in the maintenance nudge and in
//! `remind_me_server_status`. A person starts it; nothing here runs a model
//! or a routine (D104, in line with D16's reason: a background run would
//! hold BaileyAi's single model slot).
//!
//! Each run ends with one `refinement_run` memory whose `metadata.run` lists
//! what it decomposed, created, superseded and promoted. The newest one
//! starts the cooldown; #60 reads the same rows.

use crate::db::curation::Curation;
use crate::db::Store;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Category of the record a refinement run leaves.
pub const RUN_CATEGORY: &str = "refinement_run";

/// Settled captures that must wait before a run is due.
pub const MIN_BACKLOG_ENV: &str = "REMIND_ME_REFINE_MIN_BACKLOG";
/// Hours after the last run before another is due.
pub const COOLDOWN_HOURS_ENV: &str = "REMIND_ME_REFINE_COOLDOWN_HOURS";
/// Minutes with no new capture before a run is due.
pub const IDLE_MINUTES_ENV: &str = "REMIND_ME_REFINE_IDLE_MINUTES";

pub const MIN_BACKLOG_DEFAULT: u32 = 50;
pub const COOLDOWN_HOURS_DEFAULT: u32 = 8;
pub const IDLE_MINUTES_DEFAULT: u32 = 60;

/// Largest cooldown accepted: a year. Keeps timestamp arithmetic in range.
pub const COOLDOWN_HOURS_MAX: u32 = 24 * 366;
/// Largest idle window accepted: a year.
pub const IDLE_MINUTES_MAX: u32 = 60 * 24 * 366;

/// The thresholds, from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub min_backlog: u32,
    pub cooldown_hours: u32,
    pub idle_minutes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_backlog: MIN_BACKLOG_DEFAULT,
            cooldown_hours: COOLDOWN_HOURS_DEFAULT,
            idle_minutes: IDLE_MINUTES_DEFAULT,
        }
    }
}

impl Settings {
    /// Read the thresholds. Unset or blank takes the default; a value that is
    /// not a whole number is an error naming the variable, so a typo is
    /// reported rather than quietly replaced.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Settings::from_env`] over any lookup, so it is testable without
    /// touching the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let read = |name: &str, default: u32, max: u32| -> Result<u32, String> {
            let value = match lookup(name).map(|v| v.trim().to_string()) {
                None => return Ok(default),
                Some(v) if v.is_empty() => return Ok(default),
                Some(v) => v
                    .parse::<u32>()
                    .map_err(|_| format!("{name}={v:?} is not a whole number"))?,
            };
            if value > max {
                return Err(format!("{name}={value} is above the maximum, {max}"));
            }
            Ok(value)
        };
        Ok(Self {
            min_backlog: read(MIN_BACKLOG_ENV, MIN_BACKLOG_DEFAULT, u32::MAX)?,
            cooldown_hours: read(
                COOLDOWN_HOURS_ENV,
                COOLDOWN_HOURS_DEFAULT,
                COOLDOWN_HOURS_MAX,
            )?,
            idle_minutes: read(IDLE_MINUTES_ENV, IDLE_MINUTES_DEFAULT, IDLE_MINUTES_MAX)?,
        })
    }
}

/// What the store says, read once per check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// Undecomposed captures unchanged for at least the idle window.
    pub settled_backlog: u64,
    pub last_capture_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
}

/// Whether a run is due, and if not, what it waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Due {
    Due {
        settled_backlog: u64,
    },
    /// Too few settled captures.
    Backlog {
        settled_backlog: u64,
        needed: u32,
    },
    /// The last run was too recent.
    Cooldown {
        settled_backlog: u64,
        until: DateTime<Utc>,
    },
    /// Something was captured too recently.
    Active {
        settled_backlog: u64,
        until: DateTime<Utc>,
    },
    /// A threshold is set to something unreadable.
    Misconfigured {
        reason: String,
    },
}

impl Due {
    pub fn is_due(&self) -> bool {
        matches!(self, Self::Due { .. })
    }
}

/// Decide from what was observed. Pure: the backlog is checked first, so a
/// store with nothing to refine never reports a cooldown or activity.
pub fn evaluate(observed: &Observed, settings: &Settings, now: DateTime<Utc>) -> Due {
    let settled_backlog = observed.settled_backlog;
    if settled_backlog < u64::from(settings.min_backlog) {
        return Due::Backlog {
            settled_backlog,
            needed: settings.min_backlog,
        };
    }
    if let Some(last) = observed.last_run_at {
        let until = last + Duration::hours(i64::from(settings.cooldown_hours));
        if now < until {
            return Due::Cooldown {
                settled_backlog,
                until,
            };
        }
    }
    if let Some(last) = observed.last_capture_at {
        let until = last + Duration::minutes(i64::from(settings.idle_minutes));
        if now < until {
            return Due::Active {
                settled_backlog,
                until,
            };
        }
    }
    Due::Due { settled_backlog }
}

/// The refinement status `remind_me_server_status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefineStatus {
    #[serde(flatten)]
    pub due: Due,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
}

/// Read the store and decide. Never fails: a status helper must not break
/// the call it rides on, so a read error is reported as the reason.
pub fn status(store: &Store<'_>, now: DateTime<Utc>) -> RefineStatus {
    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(reason) => return unreadable(reason),
    };
    match observe(store, &settings, now) {
        Ok((observed, last_run_id)) => RefineStatus {
            due: evaluate(&observed, &settings, now),
            last_run_at: observed.last_run_at,
            last_run_id,
        },
        Err(e) => unreadable(format!("could not read the store: {e}")),
    }
}

fn unreadable(reason: String) -> RefineStatus {
    RefineStatus {
        due: Due::Misconfigured { reason },
        last_run_at: None,
        last_run_id: None,
    }
}

fn observe(
    store: &Store<'_>,
    settings: &Settings,
    now: DateTime<Utc>,
) -> crate::db::Result<(Observed, Option<String>)> {
    let settled_before = (now - Duration::minutes(i64::from(settings.idle_minutes))).to_rfc3339();
    // One adapter call under one lock. One capture writes a dialog and a
    // summary under one `capture_id` (and a continued transcript may add
    // more), so the backlog counts distinct captures, not rows: the threshold
    // is in sessions. SQLite reads ids and times only; the engine has no
    // narrow read and clones each record once (see its implementation).
    let seen = Curation::new(store).refine_observation(Some(&settled_before), RUN_CATEGORY)?;
    let (last_run_id, last_run_at) = match seen.last_run {
        Some((id, at)) => (Some(id), parse_time(&at)),
        None => (None, None),
    };
    Ok((
        Observed {
            settled_backlog: u64::try_from(seen.settled_captures).unwrap_or(0),
            last_capture_at: seen.last_capture_at.as_deref().and_then(parse_time),
            last_run_at,
        },
        last_run_id,
    ))
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// One line for the maintenance nudge when a run is due.
pub fn notice(status: &RefineStatus) -> Option<String> {
    match status.due {
        Due::Due { settled_backlog } => Some(format!(
            "- refinement due: {settled_backlog} settled captures → start the session-ladder \
             routine (docs/routines/session-ladder.md)"
        )),
        _ => None,
    }
}

/// Check a run record's `metadata.run`: an object whose `decomposed`,
/// `created`, `superseded` and `promoted` are arrays of ids (each optional)
/// and whose `started_at` and `finished_at` are RFC 3339 (both required).
pub fn validate_run(metadata: &Value) -> Result<(), crate::kinds::KindError> {
    use crate::kinds::{check_rfc3339, KindError};
    const K: &str = "refinement_run";
    let bad = |field: &'static str, problem: &str| KindError::Field {
        kind: K,
        field,
        problem: problem.to_string(),
    };
    let run = match metadata.get("run") {
        Some(run @ Value::Object(_)) => run,
        _ => return Err(bad("run", "is required: an object")),
    };
    for (key, field) in [
        ("decomposed", "run.decomposed"),
        ("created", "run.created"),
        ("superseded", "run.superseded"),
        ("promoted", "run.promoted"),
    ] {
        match run.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Array(ids)) if ids.iter().all(Value::is_string) => {}
            Some(_) => return Err(bad(field, "must be an array of memory ids")),
        }
    }
    for (key, field) in [
        ("started_at", "run.started_at"),
        ("finished_at", "run.finished_at"),
    ] {
        match run.get(key).and_then(Value::as_str) {
            Some(at) => check_rfc3339(K, field, at)?,
            None => return Err(bad(field, "is required: an RFC 3339 timestamp")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(raw: &str) -> DateTime<Utc> {
        parse_time(raw).unwrap()
    }

    fn observed(backlog: u64, last_capture: Option<&str>, last_run: Option<&str>) -> Observed {
        Observed {
            settled_backlog: backlog,
            last_capture_at: last_capture.map(at),
            last_run_at: last_run.map(at),
        }
    }

    const NOW: &str = "2026-11-27T12:00:00Z";

    #[test]
    fn below_the_backlog_threshold_is_not_due() {
        let due = evaluate(&observed(49, None, None), &Settings::default(), at(NOW));
        assert_eq!(
            due,
            Due::Backlog {
                settled_backlog: 49,
                needed: 50
            }
        );
    }

    #[test]
    fn enough_settled_material_after_a_quiet_hour_is_due() {
        let o = observed(
            50,
            Some("2026-11-27T10:59:00Z"),
            Some("2026-11-27T03:00:00Z"),
        );
        assert_eq!(
            evaluate(&o, &Settings::default(), at(NOW)),
            Due::Due {
                settled_backlog: 50
            }
        );
        assert!(evaluate(&observed(50, None, None), &Settings::default(), at(NOW)).is_due());
    }

    #[test]
    fn a_recent_run_holds_the_next_one_until_the_cooldown_ends() {
        let o = observed(80, None, Some("2026-11-27T05:00:00Z"));
        assert_eq!(
            evaluate(&o, &Settings::default(), at(NOW)),
            Due::Cooldown {
                settled_backlog: 80,
                until: at("2026-11-27T13:00:00Z")
            }
        );
    }

    #[test]
    fn new_capture_activity_cancels_a_due_run() {
        let quiet = observed(80, Some("2026-11-27T10:00:00Z"), None);
        assert!(evaluate(&quiet, &Settings::default(), at(NOW)).is_due());

        let active = observed(80, Some("2026-11-27T11:30:00Z"), None);
        assert_eq!(
            evaluate(&active, &Settings::default(), at(NOW)),
            Due::Active {
                settled_backlog: 80,
                until: at("2026-11-27T12:30:00Z")
            }
        );
    }

    #[test]
    fn thresholds_come_from_settings_and_a_typo_is_named() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(Settings::from_lookup(env(&[])), Ok(Settings::default()));
        assert_eq!(
            Settings::from_lookup(env(&[(MIN_BACKLOG_ENV, "10"), (IDLE_MINUTES_ENV, " ")])),
            Ok(Settings {
                min_backlog: 10,
                ..Settings::default()
            })
        );
        let err = Settings::from_lookup(env(&[(COOLDOWN_HOURS_ENV, "8h")])).unwrap_err();
        assert!(err.contains(COOLDOWN_HOURS_ENV), "{err}");
        let huge = Settings::from_lookup(env(&[(COOLDOWN_HOURS_ENV, "4294967295")])).unwrap_err();
        assert!(huge.contains("maximum"), "{huge}");
        let idle = Settings::from_lookup(env(&[(IDLE_MINUTES_ENV, "4294967295")])).unwrap_err();
        assert!(idle.contains(IDLE_MINUTES_ENV), "{idle}");
    }

    #[test]
    fn only_a_due_run_produces_a_notice() {
        let due = RefineStatus {
            due: Due::Due {
                settled_backlog: 61,
            },
            last_run_at: None,
            last_run_id: None,
        };
        assert!(notice(&due).unwrap().contains("61 settled captures"));
        let waiting = RefineStatus {
            due: Due::Backlog {
                settled_backlog: 3,
                needed: 50,
            },
            ..due
        };
        assert_eq!(notice(&waiting), None);
    }

    #[test]
    fn a_run_record_lists_ids_and_times() {
        let ok = json!({"run": {
            "decomposed": ["mem_c"], "created": ["mem_f1", "mem_f2"],
            "superseded": [], "promoted": ["mem_s"],
            "started_at": "2026-11-27T12:00:00Z", "finished_at": "2026-11-27T12:20:00Z"
        }});
        assert!(validate_run(&ok).is_ok());
        assert!(validate_run(&json!({})).is_err());
        let ids = json!({"run": {"created": [1], "started_at": NOW, "finished_at": NOW}});
        assert!(validate_run(&ids)
            .unwrap_err()
            .to_string()
            .contains("run.created"));
        let times = json!({"run": {"started_at": NOW}});
        assert!(validate_run(&times)
            .unwrap_err()
            .to_string()
            .contains("run.finished_at"));
    }
}
