//! Taking a profile out of service when its upstream model is gone — and only
//! then.
//!
//! The rule, agreed with the owner on 2026-09-23: three consecutive
//! *authoritative* failures, the first and last at least thirty minutes apart,
//! disable a profile. An authoritative failure is the upstream or gateway
//! saying the model or route does not exist. Nothing transient counts — a
//! timeout, a rate limit, a server error, a busy slot or a sleeping machine is
//! neither counted nor allowed to reset the count. Any success resets it.
//! Only an operator re-enables a disabled profile; a later success does not.
//!
//! The profile, its history and its evidence are kept either way.

use super::{Store, now};
use crate::domain::{DisabledState, HardFailure, Retirement};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration};
use rusqlite::{OptionalExtension, params};

/// Authoritative failures in a row before a profile is disabled.
pub const RETIRE_AFTER_FAILURES: usize = 3;
/// How far apart the first and last of them must be.
pub const RETIRE_SPAN_MINUTES: i64 = 30;
/// Evidence kept per profile; older entries fall off.
const EVIDENCE_LIMIT: usize = 10;
/// A success is written at most this often while nothing is pending, so a
/// busy profile does not turn every answer into a database write.
const SUCCESS_REFRESH_MINUTES: i64 = 10;

/// What one upstream answer says about whether the model still exists.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteOutcome {
    Served,
    /// The upstream or gateway reported the model or route as not existing.
    Gone {
        status: u16,
        detail: String,
    },
}

/// Whether an upstream error answer is authoritative evidence that the model
/// or route no longer exists. `None` for anything that might be transient.
///
/// Recognises the answers seen from the gateways Poundlock relays to:
/// LiteLLM's "Invalid model name", OpenRouter's "No endpoints found" and "is
/// not a valid model ID", and the generic not-found shapes of OpenAI-style and
/// Anthropic-style APIs. A 404 with none of these phrases is not assumed to be
/// about the model — a wrong base URL returns 404 too, and that is an
/// operator's mistake, not a retirement.
pub fn model_gone(status: u16, body: &str) -> Option<String> {
    let text = body.to_ascii_lowercase();
    let phrases: &[&str] = match status {
        404 => &[
            "no endpoints found",
            "model_not_found",
            "model not found",
            "does not exist",
            "not_found_error",
            "invalid model name",
        ],
        400 => &[
            "invalid model name",
            "not a valid model id",
            "model_not_found",
            "no endpoints found",
            "does not exist",
        ],
        _ => return None,
    };
    let phrase = phrases.iter().find(|phrase| text.contains(**phrase))?;
    Some(format!("HTTP {status}: upstream reported \"{phrase}\""))
}

impl Store {
    pub fn profile_retirement(&self, id: &str) -> Result<Option<Retirement>> {
        let raw: Option<String> = self
            .connection()?
            .query_row(
                "SELECT retirement_json FROM model_profiles WHERE id=?1 AND removed_at IS NULL",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|text| {
            if text.trim().is_empty() {
                Ok(Retirement::default())
            } else {
                serde_json::from_str(&text).context("read profile retirement state")
            }
        })
        .transpose()
    }

    fn save_retirement(&self, id: &str, retirement: &Retirement) -> Result<()> {
        self.connection()?.execute(
            "UPDATE model_profiles SET retirement_json=?1 WHERE id=?2 AND removed_at IS NULL",
            params![serde_json::to_string(retirement)?, id],
        )?;
        Ok(())
    }

    /// Record what one upstream answer showed. Returns true when this answer
    /// is the one that disabled the profile.
    pub fn record_route_outcome(
        &self,
        id: &str,
        source: &str,
        outcome: &RouteOutcome,
    ) -> Result<bool> {
        self.record_route_outcome_at(id, source, outcome, now())
    }

    /// [`Store::record_route_outcome`] at a given time, so the thirty-minute
    /// span can be tested without waiting for it.
    #[doc(hidden)]
    pub fn record_route_outcome_at(
        &self,
        id: &str,
        source: &str,
        outcome: &RouteOutcome,
        at: String,
    ) -> Result<bool> {
        let Some(mut retirement) = self.profile_retirement(id)? else {
            return Ok(false);
        };
        match outcome {
            RouteOutcome::Served => {
                let stale = retirement.last_success_at.as_deref().is_none_or(|last| {
                    minutes_between(last, &at)
                        .is_none_or(|minutes| minutes >= SUCCESS_REFRESH_MINUTES)
                });
                if retirement.hard_failures.is_empty() && !stale {
                    return Ok(false);
                }
                retirement.hard_failures.clear();
                retirement.last_success_at = Some(at);
                self.save_retirement(id, &retirement)?;
                Ok(false)
            }
            RouteOutcome::Gone { status, detail } => {
                retirement.hard_failures.push(HardFailure {
                    at: at.clone(),
                    status: *status,
                    source: source.into(),
                    detail: detail.chars().take(200).collect(),
                });
                let overflow = retirement
                    .hard_failures
                    .len()
                    .saturating_sub(EVIDENCE_LIMIT);
                retirement.hard_failures.drain(..overflow);
                let disables = retirement.disabled.is_none()
                    && meets_retirement_rule(&retirement.hard_failures);
                if disables {
                    retirement.disabled = Some(DisabledState {
                        reason: retirement
                            .hard_failures
                            .last()
                            .map(|failure| failure.detail.clone())
                            .unwrap_or_default(),
                        source: source.into(),
                        since: at,
                        evidence: retirement.hard_failures.clone(),
                    });
                }
                self.save_retirement(id, &retirement)?;
                Ok(disables)
            }
        }
    }

    /// An operator's decision to put a profile back in service. Clears the
    /// evidence too: the count starts again from the next answer.
    pub fn enable_profile(&self, id: &str) -> Result<bool> {
        let Some(mut retirement) = self.profile_retirement(id)? else {
            return Ok(false);
        };
        retirement.disabled = None;
        retirement.hard_failures.clear();
        self.save_retirement(id, &retirement)?;
        Ok(true)
    }
}

/// The agreed rule, applied to the consecutive failures on record.
pub fn meets_retirement_rule(failures: &[HardFailure]) -> bool {
    if failures.len() < RETIRE_AFTER_FAILURES {
        return false;
    }
    let window = &failures[failures.len() - RETIRE_AFTER_FAILURES..];
    minutes_between(&window[0].at, &window[window.len() - 1].at)
        .is_some_and(|minutes| minutes >= RETIRE_SPAN_MINUTES)
}

fn minutes_between(earlier: &str, later: &str) -> Option<i64> {
    let earlier = DateTime::parse_from_rfc3339(earlier).ok()?;
    let later = DateTime::parse_from_rfc3339(later).ok()?;
    Some(
        (later - earlier)
            .num_minutes()
            .max(Duration::zero().num_minutes()),
    )
}
