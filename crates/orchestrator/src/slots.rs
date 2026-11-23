use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct SlotSemaphore {
    inner: Arc<Semaphore>,
    /// Pool size at construction: the total number of lane slots, for
    /// reservation decisions that must never ask for more units than exist.
    total: usize,
}

impl SlotSemaphore {
    pub fn new(max_running: usize) -> Self {
        Self {
            inner: Arc::new(Semaphore::new(max_running)),
            total: max_running,
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

    /// Total pool size (Wave D: the global lane-slot capacity).
    pub fn capacity(&self) -> usize {
        self.total
    }

    /// Wave D queue promotion: reserve `n` single-unit lane permits in one
    /// atomic-enough sweep. On partial failure the already-reserved permits
    /// are dropped (returning their units) and None is returned, so a failed
    /// reservation leaves the pool exactly as it was. A promoted run holds its
    /// reservation until each lane's implementer phase consumes its permit.
    pub fn try_reserve_n(&self, n: usize) -> Option<Vec<OwnedSemaphorePermit>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match self.inner.clone().try_acquire_owned() {
                Ok(permit) => out.push(permit),
                Err(_) => return None, // dropping `out` returns every unit
            }
        }
        Some(out)
    }
}
