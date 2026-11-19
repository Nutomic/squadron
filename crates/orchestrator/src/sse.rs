use tokio::sync::broadcast;

use crate::snapshot::RunSnapshot;

#[derive(Debug, Clone)]
pub struct SseHub {
    tx: broadcast::Sender<RunSnapshot>,
}

impl SseHub {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RunSnapshot> {
        self.tx.subscribe()
    }

    pub fn publish(&self, snapshot: RunSnapshot) {
        let _ = self.tx.send(snapshot);
    }
}

impl Default for SseHub {
    fn default() -> Self {
        Self::new(256)
    }
}
