//! Deadlines use elapsed boot time, never the adjustable real-time clock.
//! Linux CLOCK_BOOTTIME includes suspend. The non-Linux fallback measures
//! process elapsed time; it is used for development, not RTC persistence.
use std::{
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Clock {
    started: Instant,
    baseline: Duration,
    #[cfg(target_os = "linux")]
    kernel: bool,
}

#[cfg(target_os = "linux")]
fn boot_time() -> Option<Duration> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value points to a writable timespec; this call only reads time.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0
        || value.tv_sec < 0
        || !(0..1_000_000_000).contains(&value.tv_nsec)
    {
        return None;
    }
    Some(Duration::new(value.tv_sec as u64, value.tv_nsec as u32))
}

fn real_elapsed() -> Duration {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    let clock = CLOCK.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let baseline = boot_time();
        Clock {
            started: Instant::now(),
            #[cfg(target_os = "linux")]
            baseline: baseline.unwrap_or_default(),
            #[cfg(not(target_os = "linux"))]
            baseline: Duration::ZERO,
            #[cfg(target_os = "linux")]
            kernel: baseline.is_some(),
        }
    });
    #[cfg(target_os = "linux")]
    if clock.kernel
        && let Some(value) = boot_time()
    {
        return value;
    }
    clock.baseline.saturating_add(clock.started.elapsed())
}

pub(crate) fn now() -> Duration {
    #[cfg(test)]
    if let Some(clock) = TEST_CLOCK.with(|clock| clock.borrow().clone()) {
        return clock.elapsed;
    }
    real_elapsed()
}

/// Epoch values are display metadata only. They must not decide a deadline.
pub(crate) fn wall_epoch() -> i64 {
    #[cfg(test)]
    if let Some(clock) = TEST_CLOCK.with(|clock| clock.borrow().clone()) {
        return clock.wall;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

/// A boot-relative deadline is reusable across daemon restarts only within
/// the same kernel boot. A reboot/RTC change never supplies its elapsed age.
pub(crate) fn boot_id() -> Option<String> {
    #[cfg(test)]
    if let Some(clock) = TEST_CLOCK.with(|clock| clock.borrow().clone()) {
        return Some(clock.boot);
    }
    #[cfg(target_os = "linux")]
    {
        let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let value = value.trim();
        (value.len() == 36 && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
            .then(|| value.to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    None
}

#[cfg(test)]
#[derive(Clone)]
struct TestClock {
    elapsed: Duration,
    wall: i64,
    boot: String,
}

#[cfg(test)]
thread_local! { static TEST_CLOCK: std::cell::RefCell<Option<TestClock>> = const { std::cell::RefCell::new(None) }; }

#[cfg(test)]
pub(crate) fn with_clock<T>(elapsed: Duration, wall: i64, test: impl FnOnce() -> T) -> T {
    struct Restore(Option<TestClock>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_CLOCK.with(|clock| *clock.borrow_mut() = self.0.take());
        }
    }
    let old = TEST_CLOCK.with(|clock| {
        clock.replace(Some(TestClock {
            elapsed,
            wall,
            boot: "test-boot".into(),
        }))
    });
    let _restore = Restore(old);
    test()
}

#[cfg(test)]
pub(crate) fn advance(elapsed: Duration, wall_delta: i64) {
    TEST_CLOCK.with(|clock| {
        let mut clock = clock.borrow_mut();
        let clock = clock.as_mut().expect("with_clock required");
        clock.elapsed = clock.elapsed.saturating_add(elapsed);
        clock.wall = clock.wall.saturating_add(wall_delta);
    });
}

#[cfg(test)]
pub(crate) fn set_boot_id(value: &str) {
    TEST_CLOCK.with(|clock| {
        clock
            .borrow_mut()
            .as_mut()
            .expect("with_clock required")
            .boot = value.to_owned()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_age_includes_suspend_and_ignores_rtc_steps() {
        with_clock(Duration::from_secs(123), 1_780_000_000, || {
            advance(Duration::ZERO, 8 * 3600);
            assert_eq!(now(), Duration::from_secs(123));
            advance(Duration::from_secs(3600), -8 * 3600);
            assert_eq!(now(), Duration::from_secs(3723));
            assert_eq!(wall_epoch(), 1_780_000_000);
        });
    }

    #[test]
    fn kernel_boot_time_is_read_only_and_monotonic() {
        let first = real_elapsed();
        let second = real_elapsed();
        assert!(second >= first);
        #[cfg(target_os = "linux")]
        {
            assert!(boot_time().is_some());
            assert!(boot_id().is_some());
        }
    }
}
