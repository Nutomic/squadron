use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub n_lanes: usize,
    pub max_running_slots: usize,
    pub impl_deadline: Duration,
    pub val_deadline: Duration,
    pub judge_deadline: Duration,
    pub retries_per_lane: usize,
    pub data_dir: std::path::PathBuf,
    pub gateway_connector_id: String,
    pub test_mode: bool,
    /// Review F2: test-only escape for the SSRF-safe 200 probe. Production
    /// lane URLs must resolve to public addresses; integration tests point
    /// them at loopback tunnel servers and flip this to true. Never set from
    /// untrusted input.
    pub probe_allow_loopback: bool,
    /// Wave D: interval between worker progress probes on a running lane
    /// sprite (~5 min in production; each probe is a SEPARATE short
    /// `sprite exec`, never the lane's own harness/gateway session).
    pub probe_interval: Duration,
    /// Wave D: exec deadline for one progress probe.
    pub probe_timeout: Duration,
    /// Wave D: TEST-ONLY canned probe exec command (the mock sprite runs plain
    /// `sh -c` commands, so a probe reply can be served deterministically).
    /// Ignored unless `test_mode` is set; never constructed from untrusted
    /// input.
    pub probe_command: Option<String>,
    /// Wave D: base backoff between slot-exhaustion requeue attempts (grows
    /// geometrically, capped at 60s).
    pub slot_requeue_backoff: Duration,
    /// F26: base backoff between sprite-create rate-limit requeue attempts
    /// (the platform window is one minute; each attempt waits out a full
    /// window).
    pub rate_limit_requeue_backoff: Duration,
}

/// Wave D: global default for concurrent lane sprites (org limit is 10
/// concurrent running sprites; headroom matters). Env-overridable.
pub const DEFAULT_MAX_CONCURRENT_LANES: usize = 8;

pub fn max_concurrent_lanes_from_env() -> usize {
    match std::env::var("SQUADRON_MAX_CONCURRENT_LANES") {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if (1..=10).contains(&n) => n,
            _ => DEFAULT_MAX_CONCURRENT_LANES,
        },
        Err(_) => DEFAULT_MAX_CONCURRENT_LANES,
    }
}

impl PipelineConfig {
    pub fn validate_n(n: usize) -> Result<(), String> {
        // 2 is the validation mini-swarm size (spec: Defaults).
        if !(2..=10).contains(&n) {
            return Err("n_lanes must be in range 2..=10".to_owned());
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        Self::validate_n(self.n_lanes)?;
        if self.max_running_slots == 0 || self.max_running_slots > 10 {
            return Err("max_running_slots must be in 1..=10".to_owned());
        }
        Ok(())
    }
}

/// #34: re-export of the canonical connector id; llm-gateway owns it and
/// derives the gateway endpoint from the same const.
pub const DEFAULT_GATEWAY_CONNECTOR_ID: &str = llm_gateway::sprites_gateway::GATEWAY_CONNECTOR_ID;

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            n_lanes: 8,
            max_running_slots: DEFAULT_MAX_CONCURRENT_LANES,
            impl_deadline: Duration::from_secs(60 * 60),
            val_deadline: Duration::from_secs(30 * 60),
            judge_deadline: Duration::from_secs(15 * 60),
            retries_per_lane: 1,
            data_dir: std::path::PathBuf::from("/data"),
            gateway_connector_id: DEFAULT_GATEWAY_CONNECTOR_ID.to_owned(),
            test_mode: false,
            probe_allow_loopback: false,
            probe_interval: Duration::from_secs(5 * 60),
            probe_timeout: Duration::from_secs(2 * 60),
            probe_command: None,
            slot_requeue_backoff: Duration::from_secs(2),
            rate_limit_requeue_backoff: Duration::from_secs(60),
        }
    }
}

pub fn ms_after(base: i64, dur: Duration) -> i64 {
    base + dur.as_millis() as i64
}
