//! Process power policy.
//!
//! macOS parks session-less idle processes so hard that even kevent
//! wakeups (timers AND socket readiness) defer for minutes — a link
//! heartbeat dies no matter how the process is launched, and caffeinate
//! and nice do not help. NSProcessInfo's activity assertion is the
//! documented opt-out. It was measured on the transcoder, but nothing in
//! it is transcoder-shaped: any Kahawai process that holds a link and
//! goes quiet is the same shape, a hub at 3am included. So every service
//! takes it, from the one place they all pass through
//! (`kahawai_runtime::startup_checks`) rather than from each `run`.
//!
//! This crate carries no logger, so the caller reports it and the return
//! value says whether this process holds the assertion.

/// Take, once per process, the assertion that keeps this process out of
/// App Nap. `true` when the process holds it. Off macOS there is nothing
/// to hold: no-op, `false`.
pub fn prevent_app_nap() -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
    #[cfg(target_os = "macos")]
    {
        // Idempotent: a second caller must not leave a second retained
        // token behind, whatever order the services start in.
        static HELD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *HELD.get_or_init(|| {
            take_activity_assertion();
            true
        })
    }
}

#[cfg(target_os = "macos")]
fn take_activity_assertion() {
    use std::ffi::c_void;
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    // NSActivityUserInitiated | NSActivityLatencyCritical
    const OPTIONS: u64 = 0x00FF_FFFF | (1 << 20) | 0xFF_0000_0000;
    unsafe {
        type Msg0 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
        type Msg1 =
            unsafe extern "C" fn(*mut c_void, *mut c_void, *const std::ffi::c_char) -> *mut c_void;
        type Msg2 = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *mut c_void) -> *mut c_void;
        let msg0: Msg0 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg1: Msg1 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let msg2: Msg2 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());

        let pi = msg0(
            objc_getClass(c"NSProcessInfo".as_ptr()),
            sel_registerName(c"processInfo".as_ptr()),
        );
        let reason = msg1(
            objc_getClass(c"NSString".as_ptr()),
            sel_registerName(c"stringWithUTF8String:".as_ptr()),
            c"kahawai link liveness".as_ptr(),
        );
        let token = msg2(
            pi,
            sel_registerName(c"beginActivityWithOptions:reason:".as_ptr()),
            OPTIONS,
            reason,
        );
        // Held for the life of the process: retain and never release.
        msg0(token, sel_registerName(c"retain".as_ptr()));
    }
}

#[cfg(test)]
mod tests {
    use super::prevent_app_nap;

    /// A second call must be harmless and must still report the assertion
    /// as held, so no caller has to know whether it was first.
    #[test]
    fn repeated_calls_agree() {
        let first = prevent_app_nap();
        assert_eq!(first, prevent_app_nap());
        assert_eq!(first, cfg!(target_os = "macos"));
    }
}
