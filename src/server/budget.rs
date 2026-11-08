//! The install's frontier budget at the API: the operator's settings and what
//! clients are told about it. See `crate::budget` for the rules.

use super::{AdminAuth, ApiError, ApiResult, AppState};
use crate::budget::{self, FrontierBudget, PeriodStatus};
use crate::domain::ModelProfile;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/frontier-budget",
        get(read_budget).put(write_budget),
    )
}

async fn read_budget(State(state): State<AppState>, _admin: AdminAuth) -> ApiResult<Json<Value>> {
    Ok(Json(operator_view(&state)?))
}

async fn write_budget(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(input): Json<FrontierBudget>,
) -> ApiResult<Json<Value>> {
    input
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    state.store.save_frontier_budget(&input)?;
    tracing::info!(
        weekly_usd = ?input.weekly_usd,
        monthly_usd = ?input.monthly_usd,
        "frontier budget saved"
    );
    Ok(Json(operator_view(&state)?))
}

/// Settings, standing and this period's spend by application and profile.
pub(super) fn operator_view(state: &AppState) -> ApiResult<Value> {
    let view = BudgetView::load(state)?;
    // Spend since the longest configured period began, or this calendar month
    // when none is: the window an operator is deciding about.
    let since = view
        .standing
        .iter()
        .map(|status| status.period.starts_at)
        .min()
        .unwrap_or_else(|| {
            let month = FrontierBudget {
                monthly_usd: Some(1.0),
                ..view.budget.clone()
            };
            budget::periods(&month, Utc::now())[0].starts_at
        });
    Ok(json!({
        "schema_version": 1,
        "settings": view.budget,
        "status": view.to_json(),
        "spend": {
            "since": since.to_rfc3339(),
            "total_usd": state.store.frontier_spend_since(since)?,
            "lines": state.store.frontier_spend_lines(since)?,
        },
    }))
}

/// The budget's standing at one moment, read once per API request.
pub(super) struct BudgetView {
    budget: FrontierBudget,
    standing: Vec<PeriodStatus>,
}

impl BudgetView {
    pub(super) fn load(state: &AppState) -> ApiResult<Self> {
        let budget = state.store.frontier_budget()?;
        let standing =
            budget::standing(&budget, Utc::now(), state.budget.in_flight_usd(), |since| {
                state.store.frontier_spend_since(since)
            })?;
        Ok(Self { budget, standing })
    }

    /// `unconfigured`, `ok`, `low` or `exhausted`: the worst period.
    fn state(&self) -> &'static str {
        if !self.budget.configured() {
            return "unconfigured";
        }
        let states: Vec<&str> = self
            .standing
            .iter()
            .map(|status| status.state(self.budget.warn_at))
            .collect();
        ["exhausted", "low"]
            .into_iter()
            .find(|state| states.contains(state))
            .unwrap_or("ok")
    }

    fn remaining_usd(&self) -> Option<f64> {
        self.standing
            .iter()
            .map(PeriodStatus::remaining_usd)
            .min_by(f64::total_cmp)
    }

    /// What every application may see: the install's standing, not who spent.
    pub(super) fn to_json(&self) -> Value {
        json!({
            "configured": self.budget.configured(),
            "state": self.state(),
            "remaining_usd": self.remaining_usd().map(|value| (value * 1e6).round() / 1e6),
            "warn_at": self.budget.warn_at,
            "utc_offset_minutes": self.budget.utc_offset_minutes,
            "periods": self
                .standing
                .iter()
                .map(|status| status.to_json(self.budget.warn_at))
                .collect::<Vec<_>>(),
        })
    }

    /// Adds the budget to an external profile's access state: blocked when it
    /// is used up or the profile has no price to meter by, warned when low.
    /// Other profiles are untouched.
    pub(super) fn apply_access(&self, profile: &ModelProfile, access: &mut Value) {
        if !budget::metered(profile) || !self.budget.configured() {
            return;
        }
        if access["availability"] != "blocked" {
            let blocked = if self.state() == "exhausted" {
                Some("frontier_budget_exhausted")
            } else if profile.catalog.pricing.is_none() {
                Some("frontier_budget_unpriced")
            } else {
                None
            };
            if let Some(reason) = blocked {
                access["availability"] = json!("blocked");
                access["availability_reason"] = json!(reason);
                access["blocked_reason"] = json!(reason);
            }
        }
        if self.state() == "low" && access["warning_reason"].is_null() {
            access["warning_reason"] = json!("frontier_budget_low");
        }
        access["frontier_budget"] = self.summary();
    }

    /// The short form shown beside an external profile.
    pub(super) fn summary(&self) -> Value {
        json!({
            "state": self.state(),
            "remaining_usd": self.remaining_usd().map(|value| (value * 1e6).round() / 1e6),
            "resets_at": self
                .standing
                .iter()
                .filter(|status| status.state(self.budget.warn_at) != "ok")
                .map(|status| status.period.resets_at)
                .max()
                .map(|at| at.to_rfc3339()),
        })
    }

    pub(super) fn configured(&self) -> bool {
        self.budget.configured()
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
pub(super) mod tests;
