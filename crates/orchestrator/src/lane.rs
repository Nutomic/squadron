#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    Queued,
    Creating,
    Running,
    Done,
    Validated,
    Failed,
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
                | (Creating, Running)
                | (Creating, Failed)
                | (Running, Done)
                | (Running, Failed)
                | (Done, Validated)
                | (Done, Failed)
                | (Validated, Judged)
        )
    }
}
