//! Fine timers while video is being paced.
//!
//! Windows wakes sleeping tasks on a 15.6 ms tick by default, which would
//! turn pacing into bursts; asking for 1 ms (for this process, since
//! Windows 10 2004) keeps packets spread. Elsewhere timers are already fine.

/// Holds the finer timer resolution while it lives.
pub(crate) struct FineTimers {
    #[cfg(windows)]
    active: bool,
}

#[cfg(windows)]
#[allow(unsafe_code)] // plain Win32 calls
mod imp {
    use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};

    impl super::FineTimers {
        pub(crate) fn new() -> Self {
            Self {
                active: unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR,
            }
        }
    }

    impl Drop for super::FineTimers {
        fn drop(&mut self) {
            if self.active {
                unsafe { timeEndPeriod(1) };
            }
        }
    }
}

#[cfg(not(windows))]
impl FineTimers {
    pub(crate) fn new() -> Self {
        Self {}
    }
}
