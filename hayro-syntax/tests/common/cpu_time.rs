//! The processor time the calling thread has used, for tests that bound the time
//! some work takes: other threads and processes that take the processor add to the
//! wall-clock time of the work, not to this. What shares the core's caches and
//! memory still slows it.

use std::time::Duration;

/// The processor time, user and kernel, this thread has used so far.
#[cfg(unix)]
pub(crate) fn thread_cpu_time() -> Duration {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `t` is a `timespec` the call may write.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    assert_eq!(r, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID)");
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

/// The processor time, user and kernel, this thread has used so far.
#[cfg(windows)]
pub(crate) fn thread_cpu_time() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle `GetCurrentThread` returns names the calling thread,
    // and each pointer is to a `FILETIME` the call may write.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "GetThreadTimes");
    // Each counts units of 100 ns.
    let units = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos((units(kernel) + units(user)) * 100)
}
