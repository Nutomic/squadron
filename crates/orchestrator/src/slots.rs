use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct SlotSemaphore {
    inner: Arc<Semaphore>,
}

impl SlotSemaphore {
    pub fn new(max_running: usize) -> Self {
        Self {
            inner: Arc::new(Semaphore::new(max_running)),
        }
    }

    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.inner
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed")
    }

    pub fn available(&self) -> usize {
        self.inner.available_permits()
    }
}
