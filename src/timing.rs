//! Test-only time compression.
//!
//! Production waits (supervisor ticks, native-send readiness/consumption windows,
//! terminal settle delays) are real-time constants. The check suite spawns real
//! daemons and would otherwise wait them out. When `SESSIONSATION_TEST_TIME_SCALE=N`
//! is set (an integer from 2 to 1000), the waits routed through [`scaled`],
//! [`secs`] and [`budget`] are divided by `N`. Unset, empty, `0`, `1` or
//! unparsable means real time, which is what production always runs. Only waits
//! and poll cadences are compressed; deadlines persisted as wall-clock seconds
//! (expiries, retention, freshness) are not.
use std::{sync::OnceLock, time::Duration};

/// Name of the environment variable; set by `scripts/check.sh` and the nextest
/// profile, never by production deployments.
pub const ENV: &str = "SESSIONSATION_TEST_TIME_SCALE";

fn parse(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| (1..=1000).contains(n))
        .unwrap_or(1)
}

/// The divisor in effect (1 = real time).
pub fn scale() -> u32 {
    static SCALE: OnceLock<u32> = OnceLock::new();
    *SCALE.get_or_init(|| parse(std::env::var(ENV).ok().as_deref()))
}

fn compress(d: Duration, n: u32, floor: Duration) -> Duration {
    if n <= 1 { d } else { (d / n).max(floor.min(d)) }
}

/// A wait or poll cadence compressed by the test time scale (5 ms floor).
pub fn scaled(d: Duration) -> Duration {
    compress(d, scale(), Duration::from_millis(5))
}

/// `scaled(Duration::from_secs(n))`.
pub fn secs(n: u64) -> Duration {
    scaled(Duration::from_secs(n))
}

/// A give-up budget (timeout) compressed by the test time scale, but never below
/// one second so a loaded machine cannot turn a healthy peer into a failure.
pub fn budget(d: Duration) -> Duration {
    compress(d, scale(), Duration::from_secs(1))
}

#[cfg(test)]
mod tests;
