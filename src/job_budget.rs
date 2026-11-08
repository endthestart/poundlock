//! A durable job's own budget: its wall clock and its external-model spend.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// What one job may use. Each limit is optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobBudget {
    /// Wall-clock seconds for the whole job, every attempt and repair
    /// together. A runner whose own limit is shorter still stops sooner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_seconds: Option<u64>,
    /// External-model spend the job may record, in US cents. Once it is
    /// reached, the job's next external request is refused with
    /// `job_budget_exhausted`; a request already running finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontier_cents: Option<u64>,
}

pub const MIN_JOB_WALL_SECONDS: u64 = 60;
pub const MAX_JOB_WALL_SECONDS: u64 = 24 * 60 * 60;
pub const MAX_JOB_FRONTIER_CENTS: u64 = 100_000;

impl JobBudget {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The wall budget a runner with `runner_seconds` of its own gives this job.
    pub fn wall_seconds_with(&self, runner_seconds: u64) -> u64 {
        self.wall_seconds
            .map_or(runner_seconds, |seconds| seconds.min(runner_seconds))
    }

    pub fn frontier_usd(&self) -> Option<f64> {
        self.frontier_cents.map(|cents| cents as f64 / 100.0)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(seconds) = self.wall_seconds {
            ensure!(
                (MIN_JOB_WALL_SECONDS..=MAX_JOB_WALL_SECONDS).contains(&seconds),
                "budget.wall_seconds must be between {MIN_JOB_WALL_SECONDS} and {MAX_JOB_WALL_SECONDS}"
            );
        }
        if let Some(cents) = self.frontier_cents {
            ensure!(
                (1..=MAX_JOB_FRONTIER_CENTS).contains(&cents),
                "budget.frontier_cents must be between 1 and {MAX_JOB_FRONTIER_CENTS}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_policy::JobPolicy;

    #[test]
    fn a_job_budget_is_bounded_and_never_outlasts_the_runner() {
        let policy: JobPolicy = serde_json::from_value(serde_json::json!({
            "budget": {"wall_seconds": 900, "frontier_cents": 250}
        }))
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.budget.wall_seconds_with(3600), 900);
        assert_eq!(policy.budget.wall_seconds_with(600), 600);
        assert_eq!(policy.budget.frontier_usd(), Some(2.5));
        assert_eq!(JobBudget::default().wall_seconds_with(3600), 3600);
        // An empty budget leaves the policy, and its wire form, as before.
        assert!(JobPolicy::default().is_default());
        assert!(
            !serde_json::to_string(&JobPolicy::default())
                .unwrap()
                .contains("budget")
        );
        for budget in [
            serde_json::json!({"wall_seconds": 59}),
            serde_json::json!({"wall_seconds": 86_401}),
            serde_json::json!({"frontier_cents": 0}),
            serde_json::json!({"frontier_cents": 100_001}),
        ] {
            let policy: JobPolicy =
                serde_json::from_value(serde_json::json!({ "budget": budget })).unwrap();
            assert!(policy.validate().is_err(), "{budget}");
        }
        assert!(
            serde_json::from_value::<JobPolicy>(serde_json::json!({"budget": {"usd": 1}})).is_err()
        );
    }
}
