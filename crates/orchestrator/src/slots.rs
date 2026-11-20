use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Default)]
pub struct SlotStats {
    pub in_flight: Arc<AtomicUsize>,
    pub peak_in_flight: Arc<AtomicUsize>,
    pub total_acquired: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
pub struct SlotSemaphore {
    inner: Arc<Semaphore>,
    stats: SlotStats,
}

/// A held slot permit; releasing it (drop) frees the slot and updates the
/// in-flight counters.
#[derive(Debug)]
pub struct SlotPermit {
    _inner: OwnedSemaphorePermit,
    stats: SlotStats,
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SlotSemaphore {
    pub fn new(max_running: usize) -> Self {
        Self {
            inner: Arc::new(Semaphore::new(max_running)),
            stats: SlotStats::default(),
        }
    }

    pub async fn acquire(&self) -> SlotPermit {
        let permit = self
            .inner
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let in_flight = self.stats.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.stats
            .peak_in_flight
            .fetch_max(in_flight, Ordering::Relaxed);
        self.stats.total_acquired.fetch_add(1, Ordering::Relaxed);
        SlotPermit {
            _inner: permit,
            stats: self.stats.clone(),
        }
    }

    pub fn available(&self) -> usize {
        self.inner.available_permits()
    }

    pub fn in_flight(&self) -> usize {
        self.stats.in_flight.load(Ordering::Relaxed)
    }

    pub fn peak_in_flight(&self) -> usize {
        self.stats.peak_in_flight.load(Ordering::Relaxed)
    }

    pub fn total_acquired(&self) -> u64 {
        self.stats.total_acquired.load(Ordering::Relaxed)
    }
}
