#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    Queued,
    Creating,
    Running,
    Done,
    Validated,
    Failed,
    Skipped,
    Judged,
}

impl LaneState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Done => "done",
            Self::Validated => "validated",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Judged => "judged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "creating" => Self::Creating,
            "running" => Self::Running,
            "done" => Self::Done,
            "validated" => Self::Validated,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            "judged" => Self::Judged,
            _ => return None,
        })
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use LaneState::*;
        matches!(
            (self, next),
            (Queued, Creating)
                | (Queued, Failed)
                | (Queued, Skipped)
                | (Creating, Running)
                | (Creating, Failed)
                | (Creating, Skipped)
                | (Running, Done)
                | (Running, Failed)
                | (Running, Skipped)
                | (Done, Validated)
                | (Done, Failed)
                | (Done, Skipped)
                | (Validated, Judged)
                | (Failed, Skipped)
        )
    }
}
