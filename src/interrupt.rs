//! Ctrl+C and SIGTERM handling.
//!
//! A handler only records the signal number. Code on the command thread polls [`check`] at
//! every wait point; a pending signal unwinds the stack with an [`Interrupted`] payload, so
//! every owned child, temporary directory and state lock is released by `Drop` on the way
//! out, the same way Python's `KeyboardInterrupt` unwound context managers.

use std::cell::Cell;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

thread_local! {
    static INTERRUPTIBLE: Cell<bool> = const { Cell::new(false) };
    static LOCAL_SIGNAL: Cell<i32> = const { Cell::new(0) };
    static LOCAL_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Unwind payload for a received Ctrl+C or SIGTERM.
#[derive(Debug, Clone, Copy)]
pub struct Interrupted;

/// Unwind payload for a consumer that closed stdout.
#[derive(Debug, Clone, Copy)]
pub struct BrokenPipe;

pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

/// The pending signal number, or 0.
pub fn pending() -> i32 {
    let local = LOCAL_SIGNAL.with(Cell::get);
    let due = LOCAL_AT.with(Cell::get).is_none_or(|at| Instant::now() >= at);
    if local != 0 && due { local } else { SIGNAL.load(Ordering::SeqCst) }
}

/// Mark the current thread as the command thread whose waits honour interruption.
pub fn set_interruptible(value: bool) {
    INTERRUPTIBLE.with(|flag| flag.set(value));
}

pub fn is_interruptible() -> bool {
    INTERRUPTIBLE.with(Cell::get)
}

/// True when this thread should stop now.
pub fn requested() -> bool {
    is_interruptible() && pending() != 0
}

/// Unwind with [`Interrupted`] when a signal is pending on an interruptible thread.
pub fn check() {
    if requested() {
        std::panic::resume_unwind(Box::new(Interrupted));
    }
}

/// Raise a simulated signal for this thread only (tests).
pub fn simulate(signal: i32) {
    LOCAL_AT.with(|at| at.set(None));
    LOCAL_SIGNAL.with(|flag| flag.set(signal));
}

/// Raise a simulated signal for this thread once `delay` has passed (tests).
pub fn simulate_after(signal: i32, delay: Duration) {
    LOCAL_AT.with(|at| at.set(Some(Instant::now() + delay)));
    LOCAL_SIGNAL.with(|flag| flag.set(signal));
}

/// Clear a simulated signal (tests).
pub fn clear_simulated() {
    LOCAL_AT.with(|at| at.set(None));
    LOCAL_SIGNAL.with(|flag| flag.set(0));
}

/// Sleep in short slices so a signal is honoured within about 50 ms.
pub fn sleep(seconds: f64) {
    if !(seconds.is_finite() && seconds > 0.0) {
        check();
        return;
    }
    let end = Instant::now() + Duration::from_secs_f64(seconds.min(1e9));
    loop {
        check();
        let now = Instant::now();
        if now >= end {
            return;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

/// True if a panic payload is one of the control-flow payloads above.
pub fn is_control_payload(payload: &(dyn std::any::Any + Send)) -> bool {
    payload.is::<Interrupted>() || payload.is::<BrokenPipe>()
}

#[cfg(unix)]
extern "C" fn record(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::SeqCst);
}

/// Install the Ctrl+C and SIGTERM handlers for the CLI process.
pub fn install_handlers() {
    #[cfg(unix)]
    // SAFETY: the handler only performs an atomic store, which is async-signal-safe.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe extern "system" fn handler(kind: u32) -> windows_sys::core::BOOL {
            // CTRL_C_EVENT 0 and CTRL_BREAK_EVENT 1 behave like Ctrl+C; close, logoff and
            // shutdown behave like a service stop.
            let signal = if kind <= 1 { SIGINT } else { SIGTERM };
            SIGNAL.store(signal, Ordering::SeqCst);
            if kind > 1 {
                // Windows ends the process when this handler returns for a close event;
                // give the command thread time to stop its children and release state.
                std::thread::sleep(Duration::from_secs(4));
            }
            1
        }
        // SAFETY: registering a static handler function with the console subsystem.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}
