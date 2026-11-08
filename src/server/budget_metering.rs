//! Admits and charges every request to an external model, whatever sent it.

use super::{ApiError, ApiResult, AppState};
use crate::budget::{self, Reservation, UsageMeter};
use crate::domain::{ModelProfile, Pricing};
use crate::store::{SpendRecord, Store};
use chrono::Utc;
use serde_json::Value;

/// Where a metered request came from.
pub(super) struct Origin<'a> {
    pub request_id: &'a str,
    pub api_token_id: &'a str,
    /// `relay`, `job`, or `check` for an operator's test or probe.
    pub kind: &'static str,
    pub job_id: Option<&'a str>,
}

/// Admits a request to `profile` against the budget. `None` for a profile that
/// is not metered; otherwise the metering that charges it when it ends.
pub(super) fn admit(
    state: &AppState,
    profile: &ModelProfile,
    body: &Value,
    origin: Origin<'_>,
) -> ApiResult<Option<Metering>> {
    if !budget::metered(profile) {
        return Ok(None);
    }
    let settings = state.store.frontier_budget()?;
    let estimate_usd = budget::estimate_usd(profile, body);
    if settings.configured() && estimate_usd.is_none() {
        return Err(ApiError::forbidden(
            "frontier_budget_unpriced",
            "this external model has no published price, so the install's frontier budget cannot meter it; the operator must set one",
        ));
    }
    let reservation = state.budget.try_reserve(
        origin.request_id.to_owned(),
        estimate_usd.unwrap_or(0.0),
        |in_flight| {
            let standing = budget::standing(&settings, Utc::now(), in_flight, |since| {
                state.store.frontier_spend_since(since)
            })
            .map_err(ApiError::from)?;
            match standing
                .iter()
                .find(|status| status.committed_usd() >= status.period.limit_usd)
            {
                Some(exhausted) => Err(ApiError::forbidden(
                    "frontier_budget_exhausted",
                    format!(
                        "the install's {} frontier budget of ${:.2} is used up; it resets at {}",
                        exhausted.period.name,
                        exhausted.period.limit_usd,
                        exhausted.period.resets_at.to_rfc3339()
                    ),
                )),
                None => Ok(()),
            }
        },
    )?;
    Ok(Some(Metering {
        store: state.store.clone(),
        request_id: origin.request_id.to_owned(),
        api_token_id: origin.api_token_id.to_owned(),
        profile_id: profile.id.clone(),
        origin: origin.kind,
        job_id: origin.job_id.map(str::to_owned),
        pricing: profile.catalog.pricing.clone(),
        estimate_usd,
        meter: None,
        gateway_cost_usd: None,
        dispatched: false,
        settled: false,
        _reservation: reservation,
    }))
}

/// Charges one admitted request when it ends. Dropping it unsettled after the
/// request was sent (a client that went away, a stream cut short) charges what
/// was seen, or the estimate. Dropping it before then charges nothing.
pub(super) struct Metering {
    store: Store,
    request_id: String,
    api_token_id: String,
    profile_id: String,
    origin: &'static str,
    job_id: Option<String>,
    pricing: Option<Pricing>,
    estimate_usd: Option<f64>,
    meter: Option<UsageMeter>,
    gateway_cost_usd: Option<f64>,
    /// The request has been sent upstream, so it may cost something.
    dispatched: bool,
    settled: bool,
    _reservation: Reservation,
}

impl Metering {
    /// The request is about to go upstream; from here on it is charged.
    pub(super) fn dispatched(&mut self) {
        self.dispatched = true;
    }

    /// The response has begun: whether it streams, and the gateway's own cost
    /// for it, if it gave one.
    pub(super) fn begin(&mut self, streaming: bool, gateway_cost_usd: Option<f64>) {
        self.meter = Some(UsageMeter::new(streaming));
        self.gateway_cost_usd = gateway_cost_usd;
    }

    pub(super) fn observe(&mut self, chunk: &[u8]) {
        if let Some(meter) = &mut self.meter {
            meter.observe(chunk);
        }
    }

    /// The upstream refused or could not be reached: nothing to charge.
    pub(super) fn cancel(mut self) {
        self.settled = true;
    }

    pub(super) fn settle(mut self) {
        self.record();
    }

    fn record(&mut self) {
        if self.settled || !self.dispatched {
            return;
        }
        self.settled = true;
        let usage = self.meter.take().and_then(UsageMeter::finish);
        let charge = budget::charge(
            self.gateway_cost_usd,
            usage.as_ref(),
            self.pricing.as_ref(),
            self.estimate_usd,
        );
        let recorded = self.store.record_frontier_spend(&SpendRecord {
            request_id: &self.request_id,
            api_token_id: &self.api_token_id,
            profile_id: &self.profile_id,
            origin: self.origin,
            job_id: self.job_id.as_deref(),
            charge: &charge,
            at: Utc::now(),
        });
        match recorded {
            Ok(()) => tracing::info!(
                request_id = %self.request_id,
                profile_id = %self.profile_id,
                origin = self.origin,
                cost_usd = charge.cost_usd,
                cost_source = charge.source,
                "frontier spend recorded"
            ),
            Err(error) => tracing::error!(
                request_id = %self.request_id,
                profile_id = %self.profile_id,
                cost_usd = charge.cost_usd,
                %error,
                "could not record frontier spend"
            ),
        }
    }
}

impl Drop for Metering {
    fn drop(&mut self) {
        self.record();
    }
}

/// The gateway's cost header, when it is a plain non-negative decimal.
pub(super) fn gateway_cost(headers: &axum::http::HeaderMap) -> Option<f64> {
    headers
        .get("x-litellm-response-cost")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 32)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Whether a response is a server-sent-event stream.
pub(super) fn is_event_stream(headers: &axum::http::HeaderMap, requested_stream: bool) -> bool {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/event-stream"))
        .unwrap_or(requested_stream)
}

#[cfg(test)]
#[path = "budget_metering_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "budget_probe_tests.rs"]
mod probe_tests;
