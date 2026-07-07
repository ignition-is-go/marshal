//! A shared 1 Hz wall clock for the dashboard.
//!
//! marshal's timestamps (`connected_at`, `last_activity_at`, `last_tool_at`,
//! `sent_at`, …) are all epoch-**millis** `i64`. Freshness ("how long since this
//! session did anything") is the roster's whole point, so rather than each panel
//! minting its own timer, `provide_now()` installs one reactive clock at the app
//! root (ticking every second) and panels read it via `use_now()`. Age math
//! leans on the browser's own `Date.now()` — wasm-native, zero deps.

use leptos::prelude::*;
use std::time::Duration;

/// Handle to the shared clock. Cheap to copy; `ms()` is a reactive read, so
/// anything that calls it re-runs each second as the clock ticks.
#[derive(Clone, Copy)]
pub struct Now(ReadSignal<f64>);

impl Now {
    /// Current wall-clock time in epoch milliseconds (for age math).
    pub fn ms(&self) -> f64 {
        self.0.get()
    }
}

/// Install the shared clock at the app root. Call once, inside `App`.
pub fn provide_now() {
    let (now, set_now) = signal(js_sys::Date::now());
    // Leptos' `set_interval` cleans the handle up on owner drop; at the root
    // owner that's the app's lifetime, which is exactly what we want.
    set_interval(
        move || set_now.set(js_sys::Date::now()),
        Duration::from_secs(1),
    );
    provide_context(Now(now));
}

/// Read the shared clock from context. Panics only if `provide_now()` wasn't
/// called at the root — a wiring bug, not a runtime condition.
pub fn use_now() -> Now {
    use_context::<Now>().expect("provide_now() must run at the app root")
}

/// Age in whole seconds of an epoch-millis timestamp relative to `now_ms`.
/// Negative ages (client/daemon clock skew) clamp to 0 rather than printing a
/// confusing negative duration.
pub fn age_secs(ts_ms: i64, now_ms: f64) -> i64 {
    (((now_ms - ts_ms as f64) / 1000.0).round() as i64).max(0)
}

/// Liveness bucket for a session's last activity. Thresholds match the
/// pre-overhaul dashboard: fresh under a minute, aging under five, stale beyond.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freshness {
    Fresh,
    Aging,
    Stale,
}

impl Freshness {
    pub fn from_age_secs(age: i64) -> Freshness {
        if age < 60 {
            Freshness::Fresh
        } else if age < 300 {
            Freshness::Aging
        } else {
            Freshness::Stale
        }
    }

    /// CSS class suffix used by the shared `.seen-*` colouring.
    pub fn class(self) -> &'static str {
        match self {
            Freshness::Fresh => "fresh",
            Freshness::Aging => "aging",
            Freshness::Stale => "stale",
        }
    }
}

/// Compact human age: `"12s"`, `"3m"`, `"2h"`, `"4d"`. Sub-second reads as `"0s"`.
pub fn humanize_age(age_secs: i64) -> String {
    if age_secs < 60 {
        format!("{age_secs}s")
    } else if age_secs < 3600 {
        format!("{}m", age_secs / 60)
    } else if age_secs < 86_400 {
        format!("{}h", age_secs / 3600)
    } else {
        format!("{}d", age_secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_clamps_negative_to_zero() {
        // now is BEHIND the timestamp (clock skew) → clamp to 0, not negative.
        assert_eq!(age_secs(1_000_000, 500_000.0), 0);
    }

    #[test]
    fn age_rounds_to_whole_seconds() {
        assert_eq!(age_secs(1_000_000, 1_005_000.0), 5);
        assert_eq!(age_secs(1_000_000, 1_064_000.0), 64);
    }

    #[test]
    fn freshness_buckets() {
        assert_eq!(Freshness::from_age_secs(0), Freshness::Fresh);
        assert_eq!(Freshness::from_age_secs(59), Freshness::Fresh);
        assert_eq!(Freshness::from_age_secs(60), Freshness::Aging);
        assert_eq!(Freshness::from_age_secs(299), Freshness::Aging);
        assert_eq!(Freshness::from_age_secs(300), Freshness::Stale);
    }

    #[test]
    fn humanize_scales() {
        assert_eq!(humanize_age(5), "5s");
        assert_eq!(humanize_age(120), "2m");
        assert_eq!(humanize_age(7_200), "2h");
        assert_eq!(humanize_age(172_800), "2d");
    }
}
