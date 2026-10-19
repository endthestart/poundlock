use super::{Store, now};
use anyhow::Result;
use rusqlite::params;

impl Store {
    pub fn record_endpoint_check(&self, id: &str, ok: bool, message: &str) -> Result<bool> {
        record_check(
            self,
            "UPDATE endpoints SET last_checked_at=?1,last_check_ok=?2,last_check_message=?3
             WHERE id=?4",
            id,
            ok,
            message,
        )
    }

    pub fn record_model_check(&self, id: &str, ok: bool, message: &str) -> Result<bool> {
        record_check(
            self,
            "UPDATE model_profiles SET last_checked_at=?1,last_check_ok=?2,last_check_message=?3
             WHERE id=?4",
            id,
            ok,
            message,
        )
    }
}

fn record_check(store: &Store, sql: &str, id: &str, ok: bool, message: &str) -> Result<bool> {
    let message = short_message(message);
    Ok(store
        .connection()?
        .execute(sql, params![now(), if ok { 1 } else { 0 }, message, id])?
        == 1)
}

fn short_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if cleaned.chars().count() > 200 {
        format!("{}…", cleaned.chars().take(200).collect::<String>())
    } else {
        cleaned
    }
}
