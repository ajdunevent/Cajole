//! One-shot wall-clock timer on a `timerfd`, used where `systemd --user`
//! timers are unreachable (inside a Flatpak sandbox).
//!
//! The target is absolute and runs on `CLOCK_REALTIME`, the same clock an
//! `OnCalendar=` timer uses. A target that passes during suspend fires on
//! resume, and a discontinuous clock change (NTP step, manual set, timezone
//! shift) also wakes the timer. A glib timeout runs on the monotonic clock,
//! which stops during suspend, so it is not a substitute.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use chrono::{DateTime, Local};

/// A `timerfd` armed with an absolute `CLOCK_REALTIME` target.
pub struct TimerFd {
    fd: OwnedFd,
}

impl TimerFd {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the returned descriptor is owned below.
        let raw = unsafe {
            libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a freshly created descriptor nothing else owns.
        Ok(TimerFd {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Replaces any earlier target. A target in the past fires at once.
    pub fn arm(&self, target: DateTime<Local>) -> io::Result<()> {
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec {
                tv_sec: target.timestamp() as libc::time_t,
                tv_nsec: target.timestamp_subsec_nanos() as libc::c_long,
            },
        };
        self.settime(libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET, &spec)
    }

    pub fn disarm(&self) -> io::Result<()> {
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        };
        self.settime(0, &spec)
    }

    fn settime(&self, flags: libc::c_int, spec: &libc::itimerspec) -> io::Result<()> {
        // SAFETY: `spec` is a valid itimerspec and the old-value pointer may be null.
        let result = unsafe {
            libc::timerfd_settime(self.fd.as_raw_fd(), flags, spec, std::ptr::null_mut())
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Consumes a pending expiration. True when the target passed or the
    /// clock was set (`ECANCELED`); false when nothing was pending.
    pub fn take_expiration(&self) -> bool {
        let mut expirations: u64 = 0;
        // SAFETY: reads at most 8 bytes into a u64 on the stack.
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                &mut expirations as *mut u64 as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        if read == std::mem::size_of::<u64>() as isize {
            return true;
        }
        read < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECANCELED)
    }
}

/// A [`TimerFd`] attached to the glib main loop. Arming again replaces the
/// previous target, so at most one wake is pending.
pub struct WallClockTimer {
    timer: std::rc::Rc<TimerFd>,
    source: Option<glib::SourceId>,
}

impl WallClockTimer {
    /// `on_fire` runs on the main loop once the target passes or the clock
    /// is set.
    pub fn new(on_fire: impl Fn() + 'static) -> io::Result<Self> {
        let timer = std::rc::Rc::new(TimerFd::new()?);
        let source = glib::unix_fd_add_local(timer.as_raw_fd(), glib::IOCondition::IN, {
            let timer = timer.clone();
            move |_, _| {
                if timer.take_expiration() {
                    on_fire();
                }
                glib::ControlFlow::Continue
            }
        });
        Ok(WallClockTimer {
            timer,
            source: Some(source),
        })
    }

    pub fn arm(&self, target: DateTime<Local>) -> io::Result<()> {
        self.timer.arm(target)
    }

    pub fn disarm(&self) -> io::Result<()> {
        self.timer.disarm()
    }
}

impl Drop for WallClockTimer {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            source.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn readable_within(timer: &TimerFd, timeout_ms: i32) -> bool {
        let mut pollfd = libc::pollfd {
            fd: timer.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        ready == 1 && pollfd.revents & libc::POLLIN != 0
    }

    #[test]
    fn unarmed_timer_is_not_readable() {
        let timer = TimerFd::new().unwrap();
        assert!(!readable_within(&timer, 50));
        assert!(!timer.take_expiration());
    }

    #[test]
    fn target_in_the_past_fires_at_once() {
        let timer = TimerFd::new().unwrap();
        timer.arm(Local::now() - Duration::seconds(5)).unwrap();
        assert!(readable_within(&timer, 1000));
        assert!(timer.take_expiration());
        assert!(!timer.take_expiration());
    }

    #[test]
    fn future_target_fires_after_it_passes_not_before() {
        let timer = TimerFd::new().unwrap();
        timer.arm(Local::now() + Duration::milliseconds(400)).unwrap();
        assert!(!readable_within(&timer, 100));
        assert!(readable_within(&timer, 2000));
        assert!(timer.take_expiration());
    }

    #[test]
    fn arming_again_replaces_the_earlier_target() {
        let timer = TimerFd::new().unwrap();
        timer.arm(Local::now() + Duration::milliseconds(200)).unwrap();
        timer.arm(Local::now() + Duration::hours(1)).unwrap();
        assert!(!readable_within(&timer, 500));
    }

    #[test]
    fn disarm_cancels_a_pending_target() {
        let timer = TimerFd::new().unwrap();
        timer.arm(Local::now() + Duration::milliseconds(200)).unwrap();
        timer.disarm().unwrap();
        assert!(!readable_within(&timer, 500));
    }
}
