//! Additive persistence for full validator reports (#18).
//!
//! Kept in a dedicated module so the existing `store` queries stay untouched;
//! runtime (non-macro) SQL only, plus the additive migration in
//! `migrations/20261120000000_lane_validator_reports.sql`.

use std::collections::HashMap;

use crate::{Store, StoreError};

impl Store {
    /// Persist (or replace) the full validator report for one lane.
    pub async fn set_val_report(
        &self,
        run_id: &str,
        idx: i64,
        report_json: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO lane_validator_reports (run_id, idx, report_json, created_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(run_id, idx) DO UPDATE \
             SET report_json = excluded.report_json, created_at = excluded.created_at",
        )
        .bind(run_id)
        .bind(idx)
        .bind(report_json)
        .bind(created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Fetch the persisted report for one lane, if any.
    pub async fn val_report(&self, run_id: &str, idx: i64) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT report_json FROM lane_validator_reports WHERE run_id = ? AND idx = ?",
        )
        .bind(run_id)
        .bind(idx)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Fetch all persisted reports for a run, keyed by lane index.
    pub async fn val_reports_for_run(
        &self,
        run_id: &str,
    ) -> Result<HashMap<i64, String>, StoreError> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT idx, report_json FROM lane_validator_reports WHERE run_id = ? ORDER BY idx ASC",
        )
        .bind(run_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().collect())
    }
}
