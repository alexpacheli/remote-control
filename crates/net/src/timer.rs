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
    use std::sync::Once;

    use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };

    impl super::FineTimers {
        pub(crate) fn new() -> Self {
            // Windows 11 ignores the request while the process has no
            // visible window (a minimized host) unless told otherwise.
            static KEEP_RESOLUTION: Once = Once::new();
            KEEP_RESOLUTION.call_once(|| {
                let state = PROCESS_POWER_THROTTLING_STATE {
                    Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                    ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                    StateMask: 0,
                };
                // Fails harmlessly before Windows 11.
                let _ = unsafe {
                    SetProcessInformation(
                        GetCurrentProcess(),
                        ProcessPowerThrottling,
                        (&raw const state).cast(),
                        size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                    )
                };
            });
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
