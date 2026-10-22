//! Thread sleeps that wake on time.
//!
//! `std::thread::sleep` (nanosleep) lets the kernel defer the wake-up: on a
//! busy, unattended Mac a 25 ms sleep was measured returning after 50-175 ms,
//! at any thread priority. A `kevent` wait with a timeout and no registered
//! events wakes within about 2 ms of its deadline on the same host, as do
//! tokio's timers, which wait the same way. The driver's waits sit on the
//! input path (glide settles, AppKit settles, window watches), so every sleep
//! in this crate goes through [`sleep`].

use std::time::{Duration, Instant};

/// A kqueue with nothing registered, used only for its timed wait.
struct Timer(libc::c_int);

impl Drop for Timer {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

thread_local! {
    static TIMER: Timer = Timer(unsafe { libc::kqueue() });
}

/// Block the current thread for at least `d`, waking close to the deadline.
pub fn sleep(d: Duration) {
    if d.is_zero() {
        return;
    }
    let deadline = Instant::now() + d;
    let timed = TIMER
        .try_with(|timer| timer.0 >= 0 && wait_until(timer.0, deadline))
        .unwrap_or(false);
    if !timed {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Wait on the empty kqueue until `deadline`; `false` if the wait failed.
fn wait_until(kq: libc::c_int, deadline: Instant) -> bool {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        let ts = libc::timespec {
            tv_sec: left.as_secs() as libc::time_t,
            tv_nsec: left.subsec_nanos() as libc::c_long,
        };
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, &ts) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleeps_at_least_the_duration() {
        for ms in [0u64, 1, 5, 25] {
            let started = Instant::now();
            sleep(Duration::from_millis(ms));
            assert!(started.elapsed() >= Duration::from_millis(ms));
        }
    }

    #[test]
    fn wakes_close_to_the_deadline() {
        // Generous bound: a coalesced nanosleep overshoots by up to ~150 ms on
        // a loaded host; the kevent wait stays within a few ms.
        let mut worst = Duration::ZERO;
        for _ in 0..10 {
            let started = Instant::now();
            sleep(Duration::from_millis(25));
            worst = worst.max(started.elapsed());
        }
        assert!(worst < Duration::from_millis(60), "worst wake {worst:?}");
    }

    #[test]
    fn each_thread_gets_its_own_timer() {
        let handles: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(|| sleep(Duration::from_millis(5))))
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
