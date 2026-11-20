use tokio::sync::broadcast;

use crate::snapshot::Snapshot;

#[derive(Debug, Clone)]
pub struct SseHub {
    tx: broadcast::Sender<Snapshot>,
}

impl SseHub {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Snapshot> {
        self.tx.subscribe()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        let _ = self.tx.send(snapshot);
    }
}

impl Default for SseHub {
    fn default() -> Self {
        Self::new(256)
    }
}
