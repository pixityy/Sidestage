//! Console output for the command-line tools.
//!
//! Windows release builds use the GUI subsystem (no console window for the app), so a CLI
//! subcommand started from a terminal has no stdout/stderr until it attaches to the terminal's
//! console. Output that is already redirected (a pipe or a file) keeps working without this.
//!
//! The commands print with [`say!`] and [`tell!`] instead of `println!`/`eprintln!`, which panic
//! when the output is gone (`sidestage list | head -1`, a closed terminal during `run`). A panic
//! there would skip the clean stop of a running session.

/// `println!` that ignores write errors.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// `eprintln!` that ignores write errors.
macro_rules! tell {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub(crate) use {say, tell};

/// Attach to the parent process's console when we do not have usable standard handles.
#[cfg(windows)]
pub fn attach_parent() {
    use std::ffi::c_void;

    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
    }

    // SAFETY: plain Win32 calls without pointer arguments. AttachConsole fails harmlessly when we
    // already have a console (debug builds) or the parent has none (started from Explorer).
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out.is_null() || out == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(windows))]
pub fn attach_parent() {}

/// Show an error in a message box. The window app has no console in Windows release builds, so
/// a failure before the window exists (no usable OpenGL, e.g. in some VMs and remote sessions)
/// would otherwise only reach the log file.
#[cfg(windows)]
pub fn error_box(text: &str) {
    use std::ffi::c_void;

    const MB_ICONERROR: u32 = 0x10;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Sidestage"));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call; no owner.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR);
    }
}

#[cfg(not(windows))]
pub fn error_box(_text: &str) {}
