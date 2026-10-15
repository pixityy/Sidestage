//! Ctrl+C / termination handling, so `sidestage run` and the window always stop their session
//! (and with it restore the music app's original output) instead of dying mid-session.
//!
//! A second Ctrl+C ends the process at once, in case stopping hangs; the crash guardian and the
//! next launch then restore the original output.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static CLEANUP_DONE: AtomicBool = AtomicBool::new(false);

/// True once the user pressed Ctrl+C or the process was asked to terminate.
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// Tell a pending console-close handler that the session has been stopped, so Windows may end
/// the process now.
pub fn cleanup_done() {
    CLEANUP_DONE.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
pub fn install() -> anyhow::Result<()> {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    type HandlerRoutine = unsafe extern "system" fn(ctrl_type: u32) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        let again = STOP_REQUESTED.swap(true, Ordering::SeqCst);
        if matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
            // Not handled the second time: Windows' default handler ends the process.
            return i32::from(!again);
        }
        if matches!(ctrl_type, CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
            // Windows ends the process as soon as this handler returns (and after ~5 s at the
            // latest), so give the main thread a moment to stop the session and restore audio.
            for _ in 0..45 {
                if CLEANUP_DONE.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        1
    }

    // SAFETY: registers a handler with the signature Windows expects; it only touches atomics
    // and sleeps, which is allowed on the console control thread.
    let ok = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    anyhow::ensure!(ok != 0, "SetConsoleCtrlHandler failed: {}", std::io::Error::last_os_error());
    Ok(())
}

#[cfg(unix)]
pub fn install() -> anyhow::Result<()> {
    extern "C" fn handler(_signal: libc::c_int) {
        // Only an atomic swap and `_exit`: both async-signal-safe.
        if STOP_REQUESTED.swap(true, Ordering::SeqCst) {
            // SAFETY: `_exit` ends the process without running any user-space code.
            unsafe { libc::_exit(130) };
        }
    }

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `sigaction` with zeroed structs, an empty mask and a handler that only touches
        // an atomic (or calls `_exit`).
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut current) == 0 && current.sa_sigaction == libc::SIG_IGN {
                // Ignored on purpose (`nohup sidestage &`, a background job): keep it that way.
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                anyhow::bail!("sigaction({signal}) failed: {}", std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
pub fn install() -> anyhow::Result<()> {
    Ok(())
}
