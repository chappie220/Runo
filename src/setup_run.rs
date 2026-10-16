//! A device set-up (`/setup`) as one run over several turns, held to
//! `[limits]` (Runo 1.0, 4.6): `setup_tool_calls` and `setup_usd` over the
//! whole run, and `setup_attempts` failed changes per target. A limit stops
//! only what is left: changes already verified stay.
//!
//! The run is kept in `<state>/system-setup-run.json`. It ends when the
//! agent says it is done, when a limit is reached, when a new one starts,
//! or after hours without a turn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent::Budget;
use crate::i18n::{Tr, tr};
use crate::limits::{LimitsConfig, money};

const FILE: &str = "system-setup-run.json";
/// A run with no turn for this long has ended.
const IDLE_SECS: i64 = 6 * 3600;

const STOP_CALLS: Tr = tr(
    "Device set-up stopped: it made {} tool calls, its limit is {} (limits.setup_tool_calls). The steps already verified stay; /setup starts again from how the device is now.",
    "设备配置已停止：已调用 {} 次工具，上限是 {}（limits.setup_tool_calls）。已验证的步骤保留；/setup 会从设备现在的状态重新开始。",
);
const STOP_COST: Tr = tr(
    "Device set-up stopped: it spent ${}, its limit is ${} (limits.setup_usd). The steps already verified stay; /setup starts again from how the device is now.",
    "设备配置已停止：已花费 ${}，上限是 ${}（limits.setup_usd）。已验证的步骤保留；/setup 会从设备现在的状态重新开始。",
);

#[cfg(test)]
pub const ALL: &[Tr] = &[STOP_CALLS, STOP_COST];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// The session it runs in; turns elsewhere do not count.
    pub session: String,
    pub started: i64,
    /// When a turn of it last ran.
    pub last: i64,
    pub tool_calls: usize,
    pub spent: f64,
    /// Changes put back, per target.
    pub failures: BTreeMap<String, u32>,
    /// Why it ended; `None` while it runs.
    pub ended: Option<String>,
}

pub struct Runs {
    path: PathBuf,
    limits: LimitsConfig,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Runs {
    pub fn new(state: &Path, limits: LimitsConfig) -> Self {
        Self {
            path: state.join(FILE),
            limits,
        }
    }

    fn read(&self) -> Option<Run> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write(&self, run: &Run) {
        let result = serde_json::to_vec_pretty(run)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&self.path, json));
        if let Err(err) = result {
            eprintln!("device set-up: cannot save {}: {err}", self.path.display());
        }
    }

    /// Starts a new run in `session`; one before it ends.
    pub fn start(&self, session: &str) {
        let at = now();
        self.write(&Run {
            session: session.to_owned(),
            started: at,
            last: at,
            ..Run::default()
        });
    }

    /// The run going on in `session`.
    pub fn active(&self, session: &str) -> Option<Run> {
        self.read().filter(|run| {
            run.ended.is_none() && run.session == session && now() - run.last < IDLE_SECS
        })
    }

    fn end(&self, mut run: Run, why: &str) {
        run.ended = Some(why.to_owned());
        self.write(&run);
    }

    /// The agent is done: the turns after this count for nothing.
    pub fn finish(&self, session: &str) -> bool {
        match self.active(session) {
            Some(run) => {
                self.end(run, "done");
                true
            }
            None => false,
        }
    }

    /// Adds a step of `session`'s run; the text that stops it once a limit
    /// is reached, which also ends the run.
    pub fn charge(&self, session: &str, cost: f64, tool_calls: usize) -> Option<String> {
        let mut run = self.active(session)?;
        run.spent += cost;
        run.tool_calls += tool_calls;
        run.last = now();
        let calls = self.limits.setup_tool_calls;
        if calls > 0 && run.tool_calls >= calls {
            let stop = STOP_CALLS.with(&[&run.tool_calls.to_string(), &calls.to_string()]);
            self.end(run, "tool calls");
            return Some(stop);
        }
        if let Some(limit) = self.limits.setup_limit()
            && run.spent >= limit
        {
            let stop = STOP_COST.with(&[&money(run.spent), &money(limit)]);
            self.end(run, "cost");
            return Some(stop);
        }
        self.write(&run);
        None
    }

    /// Whether `target` may be tried again in `session`'s run: `Err` once
    /// it failed `setup_attempts` times.
    pub fn may_try(&self, session: &str, target: &str) -> Result<(), String> {
        let Some(run) = self.active(session) else {
            return Ok(());
        };
        let failed = run.failures.get(target).copied().unwrap_or(0);
        let attempts = self.limits.setup_attempts.max(1);
        if failed >= attempts {
            return Err(format!(
                "error: target {target:?} failed {failed} times in this set-up \
                 (limits.setup_attempts = {attempts}). Leave it as it is now, go on with the \
                 next target, and say in the report why it failed."
            ));
        }
        Ok(())
    }

    /// A change for `target` was put back.
    pub fn failed(&self, session: &str, target: &str) {
        if let Some(mut run) = self.active(session) {
            *run.failures.entry(target.to_owned()).or_default() += 1;
            self.write(&run);
        }
    }
}

/// `runs` as the budget of `session`'s turns.
pub fn budget(runs: Arc<Runs>, session: String) -> Arc<dyn Budget> {
    Arc::new(SessionBudget { runs, session })
}

struct SessionBudget {
    runs: Arc<Runs>,
    session: String,
}

impl Budget for SessionBudget {
    fn charge(&self, cost: f64, tool_calls: usize) -> Option<String> {
        self.runs.charge(&self.session, cost, tool_calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(dir: &Path, calls: usize, usd: Option<f64>) -> Runs {
        Runs::new(
            dir,
            LimitsConfig {
                setup_tool_calls: calls,
                setup_usd: usd,
                ..LimitsConfig::default()
            },
        )
    }

    #[test]
    fn a_run_counts_tool_calls_until_its_limit_and_then_ends() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs(dir.path(), 5, None);
        // No run: nothing is counted and nothing stops.
        assert_eq!(runs.charge("main", 9.0, 9), None);
        runs.start("main");
        assert!(runs.active("other").is_none());
        assert_eq!(runs.charge("other", 0.0, 9), None);
        assert_eq!(runs.charge("main", 0.01, 3), None);
        let stop = runs.charge("main", 0.01, 2).unwrap();
        assert!(
            stop.contains("5") && stop.contains("limits.setup_tool_calls"),
            "{stop}"
        );
        assert!(runs.active("main").is_none());
        assert_eq!(runs.charge("main", 0.0, 1), None);
        assert_eq!(runs.read().unwrap().ended.as_deref(), Some("tool calls"));
    }

    #[test]
    fn a_run_stops_at_its_cost_and_ends_when_done() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs(dir.path(), 0, Some(1.0));
        runs.start("main");
        assert_eq!(runs.charge("main", 0.6, 100), None);
        let stop = runs.charge("main", 0.5, 0).unwrap();
        assert!(stop.contains("limits.setup_usd"), "{stop}");

        runs.start("main");
        assert!(runs.finish("main"));
        assert!(!runs.finish("main"));
        assert_eq!(runs.charge("main", 5.0, 0), None);
    }

    #[test]
    fn a_target_is_given_up_after_its_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs(dir.path(), 50, None);
        // Outside a run nothing is counted.
        runs.failed("main", "network");
        assert!(runs.may_try("main", "network").is_ok());
        runs.start("main");
        for _ in 0..3 {
            assert!(runs.may_try("main", "network").is_ok());
            runs.failed("main", "network");
        }
        let err = runs.may_try("main", "network").unwrap_err();
        assert!(err.contains("failed 3 times"), "{err}");
        assert!(runs.may_try("main", "time").is_ok());
        // A new run starts over.
        runs.start("main");
        assert!(runs.may_try("main", "network").is_ok());
    }

    #[test]
    fn an_idle_run_has_ended() {
        let dir = tempfile::tempdir().unwrap();
        let runs = runs(dir.path(), 50, None);
        runs.start("main");
        let mut run = runs.read().unwrap();
        run.last -= IDLE_SECS + 1;
        runs.write(&run);
        assert!(runs.active("main").is_none());
    }

    #[test]
    fn every_text_is_translated() {
        crate::i18n::assert_complete(ALL);
    }
}
