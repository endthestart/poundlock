//! One frontier budget for the whole install: a weekly and a monthly ceiling on
//! what requests to external profiles cost, across every application token and
//! every durable job.
//!
//! Poundlock meters each external request itself, so the ceiling holds without
//! LiteLLM and across gateways. The cost of a request is, in order, the
//! gateway's own figure (`x-litellm-response-cost`), the provider's
//! (`usage.cost`, as OpenRouter reports it), or the tokens the response reports
//! times the profile's published price. A request that reports none of these is
//! charged its admission estimate, which assumes the whole output budget was
//! used: a ceiling may count high, never low.
//!
//! Admission refuses new external work once a period's recorded spend plus the
//! estimates of requests still running reaches its ceiling. The overshoot is
//! bounded by the requests already in flight when the ceiling is reached.
//!
//! Nothing here records a prompt or a response: only token counts, a cost and
//! where the cost came from.

use anyhow::{Result, ensure};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod gate;
mod meter;

pub use gate::{BudgetGate, Reservation};
pub use meter::{Charge, Usage, UsageMeter, charge, estimate_usd, metered, request_stream_usage};

/// A ceiling above this is a typo, not a budget.
const MAX_CEILING_USD: f64 = 1_000_000.0;

/// The operator's ceilings. Neither set means external spend is metered and
/// shown but never refused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FrontierBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_usd: Option<f64>,
    /// The share of a ceiling at which clients are warned, above 0 and below 1.
    #[serde(default = "default_warn_at")]
    pub warn_at: f64,
    /// Where weeks (Monday 00:00) and months (the 1st, 00:00) begin, in minutes
    /// east of UTC. A fixed offset: it does not follow daylight saving.
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

fn default_warn_at() -> f64 {
    0.8
}

impl Default for FrontierBudget {
    fn default() -> Self {
        Self {
            weekly_usd: None,
            monthly_usd: None,
            warn_at: default_warn_at(),
            utc_offset_minutes: 0,
        }
    }
}

impl FrontierBudget {
    pub fn configured(&self) -> bool {
        self.weekly_usd.is_some() || self.monthly_usd.is_some()
    }

    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("weekly_usd", self.weekly_usd),
            ("monthly_usd", self.monthly_usd),
        ] {
            if let Some(value) = value {
                ensure!(
                    value.is_finite() && value > 0.0 && value <= MAX_CEILING_USD,
                    "{name} must be a positive amount of at most {MAX_CEILING_USD}"
                );
            }
        }
        ensure!(
            self.warn_at.is_finite() && self.warn_at > 0.0 && self.warn_at < 1.0,
            "warn_at must be above 0 and below 1"
        );
        ensure!(
            (-14 * 60..=14 * 60).contains(&self.utc_offset_minutes),
            "utc_offset_minutes must be between -840 and 840"
        );
        Ok(())
    }

    fn offset(&self) -> FixedOffset {
        FixedOffset::east_opt(self.utc_offset_minutes * 60)
            .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC is a valid offset"))
    }
}

/// One budget period as it stands at a moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub name: &'static str,
    pub limit_usd: f64,
    pub starts_at: DateTime<Utc>,
    pub resets_at: DateTime<Utc>,
}

/// The configured periods containing `now`, weekly first.
pub fn periods(budget: &FrontierBudget, now: DateTime<Utc>) -> Vec<Period> {
    let offset = budget.offset();
    let local = now.with_timezone(&offset).date_naive();
    let mut periods = Vec::new();
    if let Some(limit_usd) = budget.weekly_usd {
        let monday = local - chrono::Days::new(u64::from(local.weekday().num_days_from_monday()));
        periods.push(Period {
            name: "weekly",
            limit_usd,
            starts_at: midnight(offset, monday),
            resets_at: midnight(offset, monday + chrono::Days::new(7)),
        });
    }
    if let Some(limit_usd) = budget.monthly_usd {
        let first = local.with_day(1).expect("every month has a first day");
        let next = if first.month() == 12 {
            NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
        }
        .expect("the first of the next month exists");
        periods.push(Period {
            name: "monthly",
            limit_usd,
            starts_at: midnight(offset, first),
            resets_at: midnight(offset, next),
        });
    }
    periods
}

fn midnight(offset: FixedOffset, date: NaiveDate) -> DateTime<Utc> {
    offset
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        .single()
        .expect("a fixed offset has no ambiguous times")
        .with_timezone(&Utc)
}

/// Where a period stands: `ok`, `low` (past the warning share) or `exhausted`.
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodStatus {
    pub period: Period,
    pub spent_usd: f64,
    pub in_flight_usd: f64,
}

impl PeriodStatus {
    pub fn committed_usd(&self) -> f64 {
        self.spent_usd + self.in_flight_usd
    }

    pub fn remaining_usd(&self) -> f64 {
        (self.period.limit_usd - self.committed_usd()).max(0.0)
    }

    pub fn state(&self, warn_at: f64) -> &'static str {
        if self.committed_usd() >= self.period.limit_usd {
            "exhausted"
        } else if self.committed_usd() >= self.period.limit_usd * warn_at {
            "low"
        } else {
            "ok"
        }
    }

    pub fn to_json(&self, warn_at: f64) -> Value {
        json!({
            "period": self.period.name,
            "limit_usd": round_cents(self.period.limit_usd),
            "spent_usd": round_micros(self.spent_usd),
            "in_flight_usd": round_micros(self.in_flight_usd),
            "remaining_usd": round_micros(self.remaining_usd()),
            "starts_at": self.period.starts_at.to_rfc3339(),
            "resets_at": self.period.resets_at.to_rfc3339(),
            "state": self.state(warn_at),
        })
    }
}

fn round_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round_micros(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// Every period's standing, for a budget and the spend recorded since each
/// period began.
pub fn standing(
    budget: &FrontierBudget,
    now: DateTime<Utc>,
    in_flight_usd: f64,
    spent_since: impl Fn(DateTime<Utc>) -> Result<f64>,
) -> Result<Vec<PeriodStatus>> {
    periods(budget, now)
        .into_iter()
        .map(|period| {
            Ok(PeriodStatus {
                spent_usd: spent_since(period.starts_at)?,
                in_flight_usd,
                period,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
