use super::*;

const INTERVAL: Duration = Duration::from_secs(1);
const TOTAL: u64 = 1_000_000_000;

fn controller() -> ConnectionController {
    ConnectionController::new(&config(), None, TOTAL, 0)
}

fn config() -> HttpDownloadJobConfig {
    HttpDownloadJobConfig {
        default_connections: 4,
        min_connections: 1,
        max_connections: 8,
        min_part_size: Bytes::new(1024),
        connection_probe_interval: INTERVAL,
        ..HttpDownloadJobConfig::default()
    }
}

fn sample(start: Instant, seconds: u64, downloaded: u64, active: u16) -> ConnectionSample {
    ConnectionSample {
        now: start + Duration::from_secs(seconds),
        downloaded_bytes: downloaded,
        active_part_count: active,
        retry_count: 0,
        remaining_bytes: TOTAL.saturating_sub(downloaded),
        locally_limited: false,
        workers_changed: false,
        workers_finished: false,
    }
}

fn begin_probe(controller: &mut ConnectionController, start: Instant) {
    assert_eq!(controller.observe(sample(start, 0, 0, 4)), 4);
    assert_eq!(controller.observe(sample(start, 1, 1000, 4)), 4);
    assert_eq!(controller.observe(sample(start, 2, 2000, 4)), 5);
    assert!(matches!(
        controller.phase,
        ConnectionPhase::AwaitActivation { .. }
    ));
}

fn begin_observation(controller: &mut ConnectionController, start: Instant) {
    begin_probe(controller, start);
    assert_eq!(controller.observe(sample(start, 3, 3000, 5)), 5);
    assert!(matches!(controller.phase, ConnectionPhase::Settling { .. }));
    assert_eq!(controller.observe(sample(start, 4, 100_000, 5)), 5);
    assert_eq!(controller.observe(sample(start, 5, 200_000, 5)), 5);
    assert!(matches!(controller.phase, ConnectionPhase::Observe { .. }));
}

#[test]
fn throughput_should_normalize_different_elapsed_intervals_and_resume_offsets() {
    let start = Instant::now();
    for millis in [250, 1000, 2500] {
        let mut meter = ThroughputMeter::default();
        assert!(meter.observe(start, 50_000, INTERVAL).is_none());
        let rate = meter
            .observe(
                start + Duration::from_millis(millis),
                50_000 + millis,
                INTERVAL,
            )
            .expect("valid rate");
        assert!((rate.raw_bps - 1000.0).abs() < 0.001);
        assert!((rate.smoothed_bps - 1000.0).abs() < 0.001);
    }
}

#[test]
fn throughput_should_accumulate_short_intervals_and_ignore_reversed_time() {
    let start = Instant::now();
    let mut meter = ThroughputMeter::default();
    assert!(meter.observe(start, 0, INTERVAL).is_none());
    assert!(meter.observe(start, 100, INTERVAL).is_none());
    assert!(meter.observe(start - INTERVAL, 100, INTERVAL).is_none());
    assert!(
        meter
            .observe(start + Duration::from_millis(50), 500, INTERVAL)
            .is_none()
    );
    let rate = meter
        .observe(start + INTERVAL, 1000, INTERVAL)
        .expect("accumulated rate");
    assert!((rate.raw_bps - 1000.0).abs() < 0.001);
}

#[test]
fn throughput_should_smooth_spikes_and_include_zero_progress() {
    let start = Instant::now();
    let mut meter = ThroughputMeter::default();
    meter.reset(start, 0);
    let first = meter
        .observe(start + INTERVAL, 1000, INTERVAL)
        .expect("first rate");
    assert_eq!(first.smoothed_bps, 1000.0);
    let spike = meter
        .observe(start + INTERVAL * 2, 11_000, INTERVAL)
        .expect("spike rate");
    assert_eq!(spike.raw_bps, 10_000.0);
    assert!(spike.smoothed_bps > 1000.0 && spike.smoothed_bps < 5500.0);
    let idle = meter
        .observe(start + INTERVAL * 3, 11_000, INTERVAL)
        .expect("idle rate");
    assert_eq!(idle.raw_bps, 0.0);
    assert!(idle.smoothed_bps < spike.smoothed_bps);
}

#[test]
fn throughput_smoothing_should_use_elapsed_time_not_tick_count() {
    let start = Instant::now();
    let mut frequent = ThroughputMeter::default();
    frequent.reset(start, 0);
    frequent.observe(start + INTERVAL, 1000, INTERVAL);
    let mut delayed = frequent.clone();
    frequent.observe(start + INTERVAL * 2, 3000, INTERVAL);
    let a = frequent
        .observe(start + INTERVAL * 3, 5000, INTERVAL)
        .expect("frequent rate");
    let b = delayed
        .observe(start + INTERVAL * 3, 5000, INTERVAL)
        .expect("delayed rate");
    assert!((a.smoothed_bps - b.smoothed_bps).abs() < 0.001);
}

#[test]
fn throughput_should_reset_on_counter_regression() {
    let start = Instant::now();
    let mut meter = ThroughputMeter::default();
    meter.reset(start, 1000);
    assert!(meter.observe(start + INTERVAL, 500, INTERVAL).is_none());
    let rate = meter
        .observe(start + INTERVAL * 2, 1500, INTERVAL)
        .expect("new baseline rate");
    assert_eq!(rate.raw_bps, 1000.0);
    assert_eq!(meter.samples, 1);
}

#[test]
fn adaptive_probe_should_wait_for_activation_and_exclude_settling_bytes() {
    let start = Instant::now();
    let mut controller = controller();
    begin_probe(&mut controller, start);
    assert_eq!(controller.observe(sample(start, 3, 100_000, 4)), 5);
    assert!(matches!(
        controller.phase,
        ConnectionPhase::AwaitActivation { .. }
    ));
    assert_eq!(controller.observe(sample(start, 4, 200_000, 5)), 5);
    assert_eq!(controller.observe(sample(start, 5, 300_000, 5)), 5);
    assert!(matches!(controller.phase, ConnectionPhase::Settling { .. }));
    controller.observe(sample(start, 6, 400_000, 5));
    controller.observe(sample(start, 7, 401_000, 5));
    controller.observe(sample(start, 8, 402_000, 5));
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
    assert_eq!(controller.target(), 5);
}

#[test]
fn adaptive_probe_should_retain_sustained_gain_then_allow_only_one_new_worker() {
    let start = Instant::now();
    let mut controller = controller();
    begin_observation(&mut controller, start);
    assert_eq!(controller.observe(sample(start, 6, 201_250, 5)), 5);
    assert_eq!(controller.observe(sample(start, 7, 202_500, 5)), 5);
    assert!(matches!(
        controller.phase,
        ConnectionPhase::Baseline { active: 5 }
    ));
    assert_eq!(controller.observe(sample(start, 8, 203_750, 5)), 5);
    assert_eq!(controller.observe(sample(start, 9, 205_000, 5)), 6);
    for second in 10..13 {
        assert_eq!(
            controller.observe(sample(start, second, 205_000 + second * 1250, 5)),
            6
        );
    }
}

#[test]
fn adaptive_probe_should_not_accept_one_noisy_sample_or_oscillate_after_low_gain() {
    let start = Instant::now();
    let mut controller = controller();
    begin_observation(&mut controller, start);
    controller.observe(sample(start, 6, 210_000, 5));
    assert!(matches!(controller.phase, ConnectionPhase::Observe { .. }));
    controller.observe(sample(start, 7, 211_000, 5));
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
    for second in 8..=12 {
        assert_eq!(
            controller.observe(sample(start, second, 211_000 + (second - 7) * 1000, 5)),
            5
        );
    }
    assert_eq!(controller.observe(sample(start, 13, 217_000, 5)), 6);
}

#[test]
fn adaptive_probe_should_timeout_without_activation_or_terminating_workers() {
    let start = Instant::now();
    let mut controller = controller();
    begin_probe(&mut controller, start);
    controller.observe(sample(start, 6, 6000, 4));
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
    assert_eq!(controller.target(), 4);
    for second in 7..=11 {
        assert_eq!(
            controller.observe(sample(start, second, second * 1000, 4)),
            4
        );
    }
    assert_eq!(controller.observe(sample(start, 12, 12_000, 4)), 5);
}

#[test]
fn adaptive_probe_should_invalidate_worker_replacement_and_worker_loss() {
    for changed in [true, false] {
        let start = Instant::now();
        let mut controller = controller();
        begin_observation(&mut controller, start);
        let mut observation = sample(start, 6, 205_000, if changed { 5 } else { 4 });
        observation.workers_changed = changed;
        controller.observe(observation);
        assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
        assert_eq!(controller.target(), 5);
    }
}

#[test]
fn adaptive_probe_should_reconcile_unfilled_slots_after_worker_loss() {
    let start = Instant::now();
    let mut controller = controller();
    begin_observation(&mut controller, start);
    controller.observe(sample(start, 6, 201_000, 4));
    for second in 7..=11 {
        assert_eq!(
            controller.observe(sample(start, second, 201_000 + (second - 6) * 1000, 4)),
            5
        );
    }
    assert_eq!(controller.observe(sample(start, 12, 207_000, 4)), 4);
}

#[test]
fn adaptive_probe_should_reject_completion_hidden_by_activation_count() {
    let start = Instant::now();
    let mut controller = controller();
    begin_probe(&mut controller, start);
    let mut activation = sample(start, 3, 3000, 5);
    activation.workers_changed = true;
    activation.workers_finished = true;
    assert_eq!(controller.observe(activation), 4);
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
}

#[test]
fn adaptive_probe_should_reset_baseline_and_settling_on_worker_changes() {
    let start = Instant::now();
    let mut controller = controller();
    controller.observe(sample(start, 0, 0, 4));
    controller.observe(sample(start, 1, 1000, 4));
    let mut replacement = sample(start, 2, 2000, 4);
    replacement.workers_changed = true;
    assert_eq!(controller.observe(replacement), 4);
    assert_eq!(controller.observe(sample(start, 3, 3000, 4)), 4);
    assert_eq!(controller.observe(sample(start, 4, 4000, 4)), 5);
    controller.observe(sample(start, 5, 5000, 5));
    let mut replacement = sample(start, 6, 6000, 5);
    replacement.workers_changed = true;
    controller.observe(replacement);
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
}

#[test]
fn adaptive_connections_should_decrease_after_retries_and_wait_for_drain() {
    let start = Instant::now();
    let mut controller = controller();
    begin_observation(&mut controller, start);
    let mut retry = sample(start, 6, 201_000, 5);
    retry.retry_count = 1;
    assert_eq!(controller.observe(retry), 4);
    for second in 7..=20 {
        assert_eq!(
            controller.observe(sample(start, second, second * 50_000, 5)),
            4
        );
    }
    assert!(matches!(controller.phase, ConnectionPhase::Baseline { .. }));
}

#[test]
fn adaptive_connections_should_suspend_expansion_under_local_or_global_limiter() {
    let start = Instant::now();
    let mut controller = controller();
    begin_probe(&mut controller, start);
    for second in 3..=20 {
        let mut limited = sample(start, second, second * 2000, 5);
        limited.locally_limited = true;
        assert_eq!(controller.observe(limited), 5);
        assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
    }
    controller.observe(sample(start, 21, 42_000, 5));
    assert!(matches!(controller.phase, ConnectionPhase::Cooldown { .. }));
}

#[test]
fn connection_overrides_and_disabled_adaptation_should_remain_fixed() {
    let start = Instant::now();
    let mut disabled = config();
    disabled.adaptive_connections_enabled = false;
    for (config, preferred, expected) in [
        (config(), Some(NonZeroU16::new(64).expect("nonzero")), 8),
        (config(), Some(NonZeroU16::new(1).expect("nonzero")), 1),
        (disabled, None, 4),
    ] {
        let mut controller = ConnectionController::new(&config, preferred, TOTAL, 0);
        assert!(!controller.is_adaptive());
        let mut observation = sample(start, 1, 10_000, 8);
        observation.retry_count = 2;
        observation.locally_limited = true;
        assert_eq!(controller.observe(observation), expected);
    }
}

#[test]
fn adaptive_connections_should_respect_bounds_and_remaining_work() {
    let start = Instant::now();
    let mut config = config();
    config.max_connections = 4;
    let mut capped = ConnectionController::new(&config, None, TOTAL, 0);
    for second in 0..10 {
        assert_eq!(capped.observe(sample(start, second, second * 1000, 4)), 4);
    }
    let mut tail = sample(start, 10, 10_000, 4);
    tail.remaining_bytes = 1024;
    assert_eq!(capped.observe(tail), 1);
    tail.remaining_bytes = 0;
    tail.retry_count = 1;
    assert_eq!(capped.observe(tail), 1);

    config.min_connections = 4;
    config.max_connections = 2;
    let mut invalid_bounds = ConnectionController::new(&config, None, TOTAL, 0);
    assert_eq!(invalid_bounds.target(), 2);
    assert_eq!(invalid_bounds.observe(tail), 2);
}

#[test]
fn adaptive_connections_should_not_probe_without_a_positive_baseline() {
    let start = Instant::now();
    let mut controller = controller();
    for second in 0..10 {
        assert_eq!(controller.observe(sample(start, second, 0, 4)), 4);
    }
}
