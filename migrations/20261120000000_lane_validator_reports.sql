-- Additive (#18): persist the FULL validator report (features/bugs/security)
-- alongside the coarse per-lane columns. Existing tables/queries untouched.
CREATE TABLE IF NOT EXISTS lane_validator_reports (
  run_id      TEXT NOT NULL REFERENCES runs(id),
  idx         INTEGER NOT NULL,
  report_json TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (run_id, idx)
);
