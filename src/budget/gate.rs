//! The estimates of admitted requests, held against the budget until they end.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The estimates of requests admitted and not yet settled.
#[derive(Clone, Default)]
pub struct BudgetGate {
    in_flight: Arc<Mutex<HashMap<String, f64>>>,
}

impl BudgetGate {
    pub fn in_flight_usd(&self) -> f64 {
        self.in_flight
            .lock()
            .map(|reservations| total(reservations.values()))
            .unwrap_or(0.0)
    }

    /// Holds `estimate_usd` against every period until the reservation drops.
    pub fn reserve(&self, id: String, estimate_usd: f64) -> Reservation {
        let admitted: Result<Reservation, std::convert::Infallible> =
            self.try_reserve(id, estimate_usd, |_| Ok(()));
        match admitted {
            Ok(reservation) => reservation,
        }
    }

    /// Reserves `estimate_usd` only if `check` passes, given what is already in
    /// flight. The check and the reservation happen under one lock, so two
    /// requests cannot both pass on the same remaining amount.
    pub fn try_reserve<E>(
        &self,
        id: String,
        estimate_usd: f64,
        check: impl FnOnce(f64) -> std::result::Result<(), E>,
    ) -> std::result::Result<Reservation, E> {
        let mut reservations = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        check(total(reservations.values()))?;
        reservations.insert(id.clone(), estimate_usd);
        Ok(Reservation {
            gate: self.clone(),
            id,
        })
    }
}

/// A sum that starts from +0.0: `Iterator::sum` gives -0.0 for no values.
fn total<'a>(values: impl Iterator<Item = &'a f64>) -> f64 {
    values.fold(0.0, |sum, value| sum + value)
}

/// An admitted request's estimate, released when this drops.
pub struct Reservation {
    gate: BudgetGate,
    id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.gate
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}
