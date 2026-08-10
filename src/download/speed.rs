use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::download::BytesPerSecond;

/// Shared async token scheduler for shaping aggregate download throughput.
#[derive(Debug)]
pub struct SpeedLimiter {
    bytes_per_second: NonZeroU64,
    next_available: Mutex<tokio::time::Instant>,
}

impl SpeedLimiter {
    /// Creates a limiter when `limit` is non-zero.
    #[must_use]
    pub fn new(limit: BytesPerSecond) -> Option<Arc<Self>> {
        NonZeroU64::new(limit.get()).map(|bytes_per_second| {
            Arc::new(Self {
                bytes_per_second,
                next_available: Mutex::new(tokio::time::Instant::now()),
            })
        })
    }

    /// Waits until `bytes` fit in the configured throughput budget.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }

        let seconds = bytes as f64 / self.bytes_per_second.get() as f64;
        let cost = Duration::from_secs_f64(seconds);
        let sleep_for = {
            let mut next_available = self.next_available.lock().await;
            let now = tokio::time::Instant::now();
            let start = (*next_available).max(now);
            *next_available = start + cost;
            start.saturating_duration_since(now)
        };

        if !sleep_for.is_zero() {
            tokio::time::sleep(sleep_for).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_should_use_none_fast_path_for_zero_limit() {
        assert!(SpeedLimiter::new(BytesPerSecond::ZERO).is_none());
    }

    #[tokio::test]
    async fn limiter_should_delay_when_enabled() {
        let limiter = SpeedLimiter::new(BytesPerSecond::new(1_000))
            .ok_or("limiter should be enabled")
            .expect("test limiter must be enabled");
        let start = tokio::time::Instant::now();

        limiter.acquire(250).await;
        limiter.acquire(250).await;

        assert!(start.elapsed() >= Duration::from_millis(200));
    }
}
