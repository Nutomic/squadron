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
            max_running_slots: 10,
            impl_deadline: Duration::from_secs(60 * 60),
            val_deadline: Duration::from_secs(30 * 60),
            judge_deadline: Duration::from_secs(15 * 60),
            retries_per_lane: 1,
            data_dir: std::path::PathBuf::from("/data"),
            gateway_connector_id: DEFAULT_GATEWAY_CONNECTOR_ID.to_owned(),
            test_mode: false,
        }
    }
}

pub fn ms_after(base: i64, dur: Duration) -> i64 {
    base + dur.as_millis() as i64
}
