use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

use crate::ids::new_run_id;
use crate::{Lane, LaneEvent, Run, StoreError};

macro_rules! lane_from_query_row {
    ($row:expr) => {
        Lane {
            run_id: $row.run_id,
            idx: $row.idx,
            variant_json: $row.variant_json,
            sprite_name: $row.sprite_name,
            impl_url: $row.impl_url,
            state: $row.state,
            impl_model: $row.impl_model,
            impl_summary: $row.impl_summary,
            impl_files: $row.impl_files,
            impl_started_at: $row.impl_started_at,
            impl_deadline: $row.impl_deadline,
            val_model: $row.val_model,
            val_verdict: $row.val_verdict,
            val_summary: $row.val_summary,
            val_quality: $row.val_quality,
            val_started_at: $row.val_started_at,
            val_deadline: $row.val_deadline,
            val_report_json: $row.val_report_json,
            tarball_path: $row.tarball_path,
            tokei_json: $row.tokei_json,
            screenshot_path: $row.screenshot_path,
            screenshot_state: $row.screenshot_state,
            console_tail: $row.console_tail,
            cost_usd: $row.cost_usd,
            error: $row.error,
        }
    };
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Ok(Self {
            pool: SqlitePool::connect(url).await?,
        })
    }

    pub async fn connect_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Ok(Self { pool })
    }

    pub async fn connect_with_migrations(url: &str) -> Result<Self, StoreError> {
        let store = Self::connect(url).await?;
        store.migrate().await?;
        Ok(store)
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn create_run(
        &self,
        brief: &str,
        n_lanes: i64,
        created_at: i64,
    ) -> Result<Run, StoreError> {
        if n_lanes <= 0 {
            return Err(StoreError::Invalid("n_lanes must be positive".to_owned()));
        }
        let id = new_run_id();
        sqlx::query!(
            "INSERT INTO runs (id, brief, status, n_lanes, created_at) VALUES (?, ?, ?, ?, ?)",
            id,
            brief,
            "queued",
            n_lanes,
            created_at
        )
        .execute(&self.pool)
        .await?;
        self.get_run(&id)
            .await?
            .ok_or_else(|| StoreError::NotFound(id))
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<Run>, StoreError> {
        let row = sqlx::query!(
            "SELECT id, brief, status, n_lanes, created_at, judge_verdict, winner_lane, total_cost_usd, finished_at FROM runs WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| Run {
            id: row.id.expect("id is NOT NULL"),
            brief: row.brief,
            status: row.status,
            n_lanes: row.n_lanes,
            created_at: row.created_at,
            judge_verdict: row.judge_verdict,
            winner_lane: row.winner_lane,
            total_cost_usd: row.total_cost_usd,
            finished_at: row.finished_at,
        }))
    }

    pub async fn list_runs(&self) -> Result<Vec<Run>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, brief, status, n_lanes, created_at, judge_verdict, winner_lane, total_cost_usd, finished_at FROM runs ORDER BY created_at DESC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Run {
                id: row.id.expect("id is NOT NULL"),
                brief: row.brief,
                status: row.status,
                n_lanes: row.n_lanes,
                created_at: row.created_at,
                judge_verdict: row.judge_verdict,
                winner_lane: row.winner_lane,
                total_cost_usd: row.total_cost_usd,
                finished_at: row.finished_at,
            })
            .collect())
    }

    pub async fn update_run_status(&self, id: &str, status: &str) -> Result<(), StoreError> {
        let result = sqlx::query!("UPDATE runs SET status = ? WHERE id = ?", status, id)
            .execute(&self.pool)
            .await?;
        ensure(result.rows_affected(), id)
    }

    pub async fn finish_run(
        &self,
        id: &str,
        status: &str,
        judge_verdict: &str,
        winner_lane: i64,
        total_cost_usd: f64,
        finished_at: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE runs SET status = ?, judge_verdict = ?, winner_lane = ?, total_cost_usd = ?, finished_at = ? WHERE id = ?",
            status,
            judge_verdict,
            winner_lane,
            total_cost_usd,
            finished_at,
            id
        )
        .execute(&self.pool)
        .await?;
        ensure(result.rows_affected(), id)
    }

    pub async fn insert_lane(
        &self,
        run_id: &str,
        idx: i64,
        variant_json: &str,
    ) -> Result<Lane, StoreError> {
        sqlx::query!(
            "INSERT INTO lanes (run_id, idx, variant_json, state) VALUES (?, ?, ?, ?)",
            run_id,
            idx,
            variant_json,
            "queued"
        )
        .execute(&self.pool)
        .await?;
        self.get_lane(run_id, idx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("{run_id}/{idx}")))
    }

    pub async fn get_lane(&self, run_id: &str, idx: i64) -> Result<Option<Lane>, StoreError> {
        let row = sqlx::query!(
            "SELECT run_id, idx, variant_json, sprite_name, impl_url, state, impl_model, impl_summary, impl_files, impl_started_at, impl_deadline, val_model, val_verdict, val_summary, val_quality, val_started_at, val_deadline, val_report_json, tarball_path, tokei_json, screenshot_path, screenshot_state, console_tail, cost_usd, error FROM lanes WHERE run_id = ? AND idx = ?",
            run_id,
            idx
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| lane_from_query_row!(row)))
    }

    pub async fn list_lanes(&self, run_id: &str) -> Result<Vec<Lane>, StoreError> {
        let rows = sqlx::query!(
            "SELECT run_id, idx, variant_json, sprite_name, impl_url, state, impl_model, impl_summary, impl_files, impl_started_at, impl_deadline, val_model, val_verdict, val_summary, val_quality, val_started_at, val_deadline, val_report_json, tarball_path, tokei_json, screenshot_path, screenshot_state, console_tail, cost_usd, error FROM lanes WHERE run_id = ? ORDER BY idx ASC",
            run_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| lane_from_query_row!(row))
            .collect())
    }

    pub async fn update_lane_state(
        &self,
        run_id: &str,
        idx: i64,
        state: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET state = ? WHERE run_id = ? AND idx = ?",
            state,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn start_impl(
        &self,
        run_id: &str,
        idx: i64,
        sprite_name: &str,
        impl_url: &str,
        model: &str,
        started_at: i64,
        deadline: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET sprite_name = ?, impl_url = ?, impl_model = ?, impl_started_at = ?, impl_deadline = ? WHERE run_id = ? AND idx = ?",
            sprite_name,
            impl_url,
            model,
            started_at,
            deadline,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn finish_impl(
        &self,
        run_id: &str,
        idx: i64,
        summary: &str,
        files: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET impl_summary = ?, impl_files = ? WHERE run_id = ? AND idx = ?",
            summary,
            files,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn start_validation(
        &self,
        run_id: &str,
        idx: i64,
        model: &str,
        started_at: i64,
        deadline: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET val_model = ?, val_started_at = ?, val_deadline = ? WHERE run_id = ? AND idx = ?",
            model,
            started_at,
            deadline,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn finish_validation(
        &self,
        run_id: &str,
        idx: i64,
        verdict: &str,
        summary: &str,
        quality: f64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET val_verdict = ?, val_summary = ?, val_quality = ? WHERE run_id = ? AND idx = ?",
            verdict,
            summary,
            quality,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn set_artifacts(
        &self,
        run_id: &str,
        idx: i64,
        tarball_path: &str,
        tokei_json: &str,
        screenshot_path: Option<&str>,
        screenshot_state: &str,
        console_tail: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET tarball_path = ?, tokei_json = ?, screenshot_path = ?, screenshot_state = ?, console_tail = ? WHERE run_id = ? AND idx = ?",
            tarball_path,
            tokei_json,
            screenshot_path,
            screenshot_state,
            console_tail,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn set_screenshot_state(
        &self,
        run_id: &str,
        idx: i64,
        screenshot_state: &str,
        screenshot_path: Option<&str>,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET screenshot_state = ?, screenshot_path = COALESCE(?, screenshot_path) WHERE run_id = ? AND idx = ?",
            screenshot_state,
            screenshot_path,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn set_lane_console_tail(
        &self,
        run_id: &str,
        idx: i64,
        console_tail: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET console_tail = ? WHERE run_id = ? AND idx = ?",
            console_tail,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    pub async fn set_lane_error(
        &self,
        run_id: &str,
        idx: i64,
        error: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET error = ? WHERE run_id = ? AND idx = ?",
            error,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    /// Add a cost delta and append the matching `cost` lane event in one
    /// transaction, so the sparkline series can never miss an update.
    pub async fn add_lane_cost(
        &self,
        run_id: &str,
        idx: i64,
        delta: f64,
    ) -> Result<(), StoreError> {
        // BEGIN IMMEDIATE takes the write lock up front: a deferred transaction
        // upgrades from a shared read lock and can deadlock, surfacing as
        // SQLITE_BUSY (code 5) under concurrent writers. Same for both txn paths.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query!(
            "UPDATE lanes SET cost_usd = COALESCE(cost_usd, 0) + ? WHERE run_id = ? AND idx = ?",
            delta,
            run_id,
            idx
        )
        .execute(&mut *tx)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)?;
        append_event_tx(
            &mut tx,
            run_id,
            idx,
            now_wall_ms(),
            "cost",
            &delta.to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_validator_report(
        &self,
        run_id: &str,
        idx: i64,
        report_json: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE lanes SET val_report_json = ? WHERE run_id = ? AND idx = ?",
            report_json,
            run_id,
            idx
        )
        .execute(&self.pool)
        .await?;
        ensure_lane(result.rows_affected(), run_id, idx)
    }

    /// Append a lane event with a monotonic per-lane timestamp. The read of the
    /// current max and the insert happen inside one immediate transaction so
    /// concurrent publishers on the same lane cannot collide on the PK.
    pub async fn append_lane_event(
        &self,
        run_id: &str,
        lane_idx: i64,
        ts: i64,
        kind: &str,
        payload: &str,
    ) -> Result<i64, StoreError> {
        // BEGIN IMMEDIATE takes the write lock up front: a deferred transaction
        // upgrades from a shared read lock and can deadlock, surfacing as
        // SQLITE_BUSY (code 5) under concurrent writers. Same for both txn paths.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let assigned = append_event_tx(&mut tx, run_id, lane_idx, ts, kind, payload).await?;
        tx.commit().await?;
        Ok(assigned)
    }

    pub async fn list_lane_events(
        &self,
        run_id: &str,
        lane_idx: i64,
    ) -> Result<Vec<LaneEvent>, StoreError> {
        let rows = sqlx::query!(
            "SELECT run_id, lane_idx, ts, kind, payload FROM lane_events WHERE run_id = ? AND lane_idx = ? ORDER BY ts ASC",
            run_id,
            lane_idx
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| LaneEvent {
                run_id: row.run_id,
                lane_idx: row.lane_idx,
                ts: row.ts,
                kind: row.kind,
                payload: row.payload,
            })
            .collect())
    }
}

/// Insert a lane event inside an existing transaction, assigning the next
/// monotonic per-lane timestamp. Concurrent publishers serialize on the
/// immediate transaction, so the (run_id, lane_idx, ts) primary key can never
/// collide.
async fn append_event_tx(
    tx: &mut sqlx::SqliteConnection,
    run_id: &str,
    lane_idx: i64,
    ts: i64,
    kind: &str,
    payload: &str,
) -> Result<i64, StoreError> {
    let max = sqlx::query!(
        "SELECT COALESCE(MAX(ts), 0) AS max_ts FROM lane_events WHERE run_id = ? AND lane_idx = ?",
        run_id,
        lane_idx
    )
    .fetch_one(&mut *tx)
    .await?
    .max_ts;
    let assigned = ts.max(max + 1);
    sqlx::query!(
        "INSERT INTO lane_events (run_id, lane_idx, ts, kind, payload) VALUES (?, ?, ?, ?, ?)",
        run_id,
        lane_idx,
        assigned,
        kind,
        payload
    )
    .execute(&mut *tx)
    .await?;
    Ok(assigned)
}

fn now_wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ensure(rows_affected: u64, id: &str) -> Result<(), StoreError> {
    if rows_affected == 0 {
        Err(StoreError::NotFound(id.to_owned()))
    } else {
        Ok(())
    }
}

fn ensure_lane(rows_affected: u64, run_id: &str, idx: i64) -> Result<(), StoreError> {
    ensure(rows_affected, &format!("{run_id}/{idx}"))
}
