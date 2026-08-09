use std::collections::VecDeque;

use crate::download::{Bytes, BytesPerSecond};

/// Computes current and average throughput from timestamped byte samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedMeter {
    samples: VecDeque<SpeedSample>,
    window_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpeedSample {
    at_ms: u64,
    bytes: Bytes,
}

impl SpeedMeter {
    /// Creates a speed meter with no averaging window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_window(None)
    }

    /// Creates a speed meter that averages samples within the provided window.
    #[must_use]
    pub fn with_window(window_ms: Option<u64>) -> Self {
        Self {
            samples: VecDeque::new(),
            window_ms,
        }
    }

    /// Records a sample and returns calculated bytes per second.
    #[must_use]
    pub fn record(&mut self, at_ms: u64, bytes: Bytes) -> BytesPerSecond {
        self.samples.push_back(SpeedSample { at_ms, bytes });
        self.prune(at_ms);
        self.speed()
    }

    /// Returns current speed based on retained samples.
    #[must_use]
    pub fn speed(&self) -> BytesPerSecond {
        let Some(first) = self.samples.front() else {
            return BytesPerSecond::ZERO;
        };
        let Some(last) = self.samples.back() else {
            return BytesPerSecond::ZERO;
        };
        let elapsed_ms = last.at_ms.saturating_sub(first.at_ms);
        if elapsed_ms == 0 || last.bytes < first.bytes {
            return BytesPerSecond::ZERO;
        }
        let delta = last.bytes.get() - first.bytes.get();
        BytesPerSecond::new(delta.saturating_mul(1_000) / elapsed_ms)
    }

    fn prune(&mut self, now_ms: u64) {
        let Some(window_ms) = self.window_ms else {
            while self.samples.len() > 2 {
                let _ = self.samples.pop_front();
            }
            return;
        };

        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|sample| now_ms.saturating_sub(sample.at_ms) > window_ms)
        {
            let _ = self.samples.pop_front();
        }
    }
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_meter_should_calculate_delta_speed() {
        let mut meter = SpeedMeter::new();

        assert_eq!(meter.record(1_000, Bytes::new(100)), BytesPerSecond::ZERO);
        assert_eq!(
            meter.record(2_000, Bytes::new(1_100)),
            BytesPerSecond::new(1_000)
        );
    }

    #[test]
    fn speed_meter_should_average_within_window() {
        let mut meter = SpeedMeter::with_window(Some(2_000));

        let _ = meter.record(0, Bytes::ZERO);
        let _ = meter.record(1_000, Bytes::new(1_000));
        let speed = meter.record(3_000, Bytes::new(2_000));

        assert_eq!(speed, BytesPerSecond::new(500));
    }
}
