use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in milliseconds, saturating to zero on clock errors.
///
/// The UI only needs a best-effort "now" for relative timestamps; a clock
/// error must never break rendering.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Formats a byte count with binary units, trimming trailing zeros.
///
/// Examples: `0 B`, `512 B`, `1.5 KiB`, `66.93 MiB`, `131.5 MiB`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let decimals = if value >= 100.0 { 1 } else { 2 };
    let mut text = format!("{value:.decimals$}");
    // Trim only the optional fraction digits, never the integer part.
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    format!("{text} {}", UNITS[unit])
}

/// Formats a throughput value. Zero renders as an empty string so idle and
/// finished rows keep a quiet speed cell.
#[must_use]
pub fn format_speed(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        return String::new();
    }
    format!("{}/s", format_bytes(bytes_per_second))
}

/// Formats an ETA duration. `None` (unknown total or speed) renders empty.
#[must_use]
pub fn format_eta(eta_seconds: Option<u64>) -> String {
    let Some(seconds) = eta_seconds else {
        return String::new();
    };
    if seconds < 60 {
        return format!("{seconds} sec left");
    }
    if seconds < 3_600 {
        let minutes = seconds / 60;
        return format!("{minutes} min left");
    }
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if minutes == 0 {
        return format!("{hours} hr left");
    }
    format!("{hours} hr {minutes} min left")
}

/// Formats a timestamp as a short relative time against `now_ms`.
///
/// Future timestamps (clock skew) render as "just now" instead of a negative
/// duration.
#[must_use]
pub fn format_relative_time(now_ms: i64, timestamp_ms: i64) -> String {
    let delta_secs = (now_ms.saturating_sub(timestamp_ms) / 1_000).max(0) as u64;
    if delta_secs < 60 {
        return "just now".to_owned();
    }
    let minutes = delta_secs / 60;
    if minutes < 60 {
        return pluralize(minutes, "min", "min");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return pluralize(hours, "hour", "hours");
    }
    let days = hours / 24;
    if days < 30 {
        return pluralize(days, "day", "days");
    }
    let months = days / 30;
    if months < 12 {
        return pluralize(months, "month", "months");
    }
    pluralize(months / 12, "year", "years")
}

fn pluralize(value: u64, singular: &str, plural: &str) -> String {
    if value == 1 {
        format!("1 {singular} ago")
    } else {
        format!("{value} {plural} ago")
    }
}

/// Computes download progress as a 0–100 percentage.
///
/// Returns `None` when the total is unknown or zero.
#[must_use]
pub fn progress_percent(downloaded_bytes: u64, total_bytes: Option<u64>) -> Option<f32> {
    let total = total_bytes?;
    if total == 0 {
        return None;
    }
    Some(((downloaded_bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32)
}

#[cfg(test)]
mod tests {
    // Explicit imports: `use super::*` would also pull `gpui_kit::*` names
    // (including GPUI's `test` macro) into scope and shadow the built-in
    // `#[test]` attribute.
    use super::{format_bytes, format_eta, format_relative_time, format_speed, progress_percent};

    #[test]
    fn bytes_should_use_binary_units_and_trim_zeros() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_024), "1 KiB");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        // 66.93 MiB reference from the main-page sketch.
        assert_eq!(format_bytes(70_181_192), "66.93 MiB");
        assert_eq!(format_bytes(137_887_744), "131.5 MiB");
        assert_eq!(format_bytes(10 * 1_024 * 1_024 * 1_024), "10 GiB");
    }

    #[test]
    fn speed_should_stay_quiet_when_zero() {
        assert_eq!(format_speed(0), "");
        assert_eq!(format_speed(4_667_392), "4.45 MiB/s");
    }

    #[test]
    fn eta_should_render_short_units_and_hide_unknown() {
        assert_eq!(format_eta(None), "");
        assert_eq!(format_eta(Some(1)), "1 sec left");
        assert_eq!(format_eta(Some(25)), "25 sec left");
        assert_eq!(format_eta(Some(90)), "1 min left");
        assert_eq!(format_eta(Some(3_600)), "1 hr left");
        assert_eq!(format_eta(Some(5_400)), "1 hr 30 min left");
    }

    #[test]
    fn relative_time_should_use_singular_and_plural() {
        let now = 1_700_000_000_000;
        assert_eq!(format_relative_time(now, now), "just now");
        assert_eq!(format_relative_time(now, now - 61_000), "1 min ago");
        assert_eq!(format_relative_time(now, now - 3_600_000), "1 hour ago");
        assert_eq!(
            format_relative_time(now, now - 2 * 3_600_000),
            "2 hours ago"
        );
        assert_eq!(
            format_relative_time(now, now - 30 * 86_400_000),
            "1 month ago"
        );
        assert_eq!(
            format_relative_time(now, now - 60 * 86_400_000),
            "2 months ago"
        );
        // Future timestamps degrade to "just now".
        assert_eq!(format_relative_time(now, now + 60_000), "just now");
    }

    #[test]
    fn progress_should_reject_unknown_or_zero_totals() {
        assert_eq!(progress_percent(100, None), None);
        assert_eq!(progress_percent(100, Some(0)), None);
        assert_eq!(progress_percent(0, Some(1_000)), Some(0.0));
        assert_eq!(progress_percent(1_000, Some(1_000)), Some(100.0));
        // Over-full values clamp instead of overflowing the bar.
        assert_eq!(progress_percent(2_000, Some(1_000)), Some(100.0));
    }
}
