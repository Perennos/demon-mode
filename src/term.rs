//! Terminal details: whether a stream is a terminal, and a Ctrl+C flag for the preview and the GUI.
//!
//! The flag lets `demon preview` restore the cursor and exit 130 on Ctrl+C, and lets `demon gui`
//! stop cleanly, as the Python release did when it caught `KeyboardInterrupt`.

use std::io::IsTerminal;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INSTALL: Once = Once::new();

pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

pub fn stdout_is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

/// `NO_COLOR` set to anything, even empty, disables colour (<https://no-color.org>).
pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Sleep in short slices; `false` if Ctrl+C arrived first.
pub fn sleep(total: Duration) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if interrupted() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(25)));
    }
}

/// Route Ctrl+C to the flag instead of terminating the process. Safe to call more than once.
pub fn catch_interrupt() {
    INSTALL.call_once(platform::install);
}

#[cfg(unix)]
mod platform {
    extern "C" fn on_interrupt(_signal: libc::c_int) {
        super::INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn install() {
        let handler: extern "C" fn(libc::c_int) = on_interrupt;
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        }
    }
}

#[cfg(windows)]
mod platform {
    type Handler = Option<unsafe extern "system" fn(u32) -> i32>;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Handler, add: i32) -> i32;
    }

    unsafe extern "system" fn on_control(kind: u32) -> i32 {
        const CTRL_C_EVENT: u32 = 0;
        const CTRL_BREAK_EVENT: u32 = 1;
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            super::INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    pub fn install() {
        // SAFETY: registers a handler that only stores to an atomic.
        unsafe {
            SetConsoleCtrlHandler(Some(on_control), 1);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub fn install() {}
}
