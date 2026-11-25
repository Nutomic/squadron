CREATE TABLE runs (
  id              TEXT PRIMARY KEY,
  brief           TEXT NOT NULL,
  status          TEXT NOT NULL,
  n_lanes         INTEGER NOT NULL,
  created_at      INTEGER NOT NULL,
  judge_verdict   TEXT,
  winner_lane     INTEGER,
  total_cost_usd  REAL,
  finished_at     INTEGER
);

CREATE TABLE lanes (
  run_id            TEXT NOT NULL REFERENCES runs(id),
  idx               INTEGER NOT NULL,
  variant_json      TEXT NOT NULL,
  sprite_name       TEXT,
  impl_url          TEXT,
  state             TEXT NOT NULL,
  revisions         INTEGER NOT NULL DEFAULT 0,
  impl_model        TEXT,
  impl_summary      TEXT,
  impl_files        INTEGER,
  impl_started_at   INTEGER,
  impl_deadline     INTEGER,
  val_model         TEXT,
  val_verdict       TEXT,
  val_summary       TEXT,
  val_quality       REAL,
  val_started_at    INTEGER,
  val_deadline      INTEGER,
  tarball_path      TEXT,
  tokei_json        TEXT,
  screenshot_path   TEXT,
  screenshot_state  TEXT DEFAULT 'pending',
  console_tail      TEXT,
  cost_usd          REAL,
  error             TEXT,
  PRIMARY KEY (run_id, idx)
);

CREATE TABLE lane_events (
  run_id   TEXT NOT NULL,
  lane_idx INTEGER NOT NULL,
  ts       INTEGER NOT NULL,
  kind     TEXT NOT NULL,
  payload  TEXT NOT NULL,
  PRIMARY KEY (run_id, lane_idx, ts)
);
