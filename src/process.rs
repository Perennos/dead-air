//! Bounded child processes with capped pipes and finite cleanup.
//!
//! Every child is started in its own process group (POSIX) or console process group
//! (Windows), its output is drained on two threads up to a byte cap, and it is stopped and
//! reaped on timeout, overflow, a file-size limit, interruption or any unwind. On POSIX the
//! whole group is sent SIGKILL afterwards, so a helper that outlived its parent cannot keep a
//! pipe open. Windows descendants are not Job Object contained.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::interrupt;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChildResult {
    pub stdout: String,
    pub stderr: String,
    pub returncode: Option<i32>,
    pub timed_out: bool,
    pub output_limited: bool,
    pub file_limited: bool,
    pub cleanup_complete: bool,
    pub launch_error: Option<String>,
}

impl ChildResult {
    /// A completed child, as test runners return.
    pub fn exited(stdout: &str, stderr: &str, returncode: i32) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            returncode: Some(returncode),
            cleanup_complete: true,
            ..Self::default()
        }
    }

    /// Timed out, truncated, file-limited or not fully reaped.
    pub fn bounded_failure(&self) -> bool {
        self.timed_out || self.output_limited || self.file_limited || !self.cleanup_complete
    }

    pub fn succeeded(&self) -> bool {
        self.launch_error.is_none() && self.returncode == Some(0)
    }
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub max_output_bytes: usize,
    pub file_limits: Vec<(PathBuf, u64)>,
}

impl RunOptions {
    pub fn output(max_output_bytes: usize) -> Self {
        Self { max_output_bytes, file_limits: Vec::new() }
    }
}

/// The runner seam: `(argv, timeout_s, options) -> result`.
pub type Runner<'a> = &'a dyn Fn(&[String], f64, &RunOptions) -> ChildResult;

/// Runtime essentials only; proxy, credential and user configuration variables are omitted.
pub fn child_environment() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"]
        .iter()
        .filter_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()).map(|v| (key.to_string(), v)))
        .collect();
    env.push(("LC_ALL".into(), "C".into()));
    env.push(("LANG".into(), "C".into()));
    env
}

/// Validate an argument vector: a non-empty executable and no NUL anywhere.
pub fn build_argv(executable: &str, args: &[&str]) -> Result<Vec<String>, String> {
    let mut values = vec![executable.to_string()];
    values.extend(args.iter().map(|a| a.to_string()));
    if values[0].trim().is_empty() || values.iter().any(|v| v.contains('\0')) {
        return Err("argv contains an empty executable or NUL".into());
    }
    Ok(values)
}

pub fn launch_error_name(error: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound => "FileNotFoundError",
        PermissionDenied => "PermissionError",
        InvalidInput => "ValueError",
        _ => "OSError",
    }
    .to_string()
}

#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Put a command in its own process group so terminal signals reach only this process.
pub fn isolate(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

pub fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(-1)
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) -> std::io::Result<()> {
    // SAFETY: killpg only sends a signal; the group id is the pid of a child we started in
    // its own group.
    let rc = unsafe { libc::killpg(pid as libc::pid_t, signal) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn wait_for(child: &mut Child, seconds: f64) -> Option<ExitStatus> {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Terminate gently, then forcefully, within about 1.2 s. Returns whether the child exited.
/// `grace_s` is the time allowed between the polite and the forceful stop.
pub fn terminate_and_reap(child: &mut Child, grace_s: f64) -> Option<ExitStatus> {
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    #[cfg(unix)]
    {
        if signal_group(child.id(), libc::SIGTERM).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
    if let Some(status) = wait_for(child, grace_s) {
        return Some(status);
    }
    #[cfg(unix)]
    {
        if signal_group(child.id(), libc::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
    wait_for(child, 1.0)
}

/// Kill whatever is left in a finished child's process group (POSIX).
fn sweep_group(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match signal_group(pid, libc::SIGKILL) {
            Ok(()) => true,
            Err(error) => error.raw_os_error() == Some(libc::ESRCH) || error.raw_os_error() == Some(libc::EPERM),
        }
    }
    #[cfg(windows)]
    {
        let _ = pid;
        true
    }
}

/// Owns a started child until it is reaped; `Drop` stops it during an unwind.
struct OwnedChild {
    child: Option<Child>,
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            terminate_and_reap(&mut child, 0.2);
            sweep_group(pid);
        }
    }
}

struct Pipe {
    buffer: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone, Copy, Default)]
struct DrainEnd {
    eof: bool,
    truncated: bool,
}

fn start_drain(
    mut pipe: impl Read + Send + 'static,
    max: usize,
    overflow: Arc<AtomicBool>,
    done: mpsc::Sender<DrainEnd>,
) -> Pipe {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&buffer);
    let failed = Arc::clone(&overflow);
    let spawned = std::thread::Builder::new().name("dead-air-drain".into()).spawn(move || {
        let mut chunk = [0u8; 4096];
        let mut end = DrainEnd::default();
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    end.eof = true;
                    break;
                }
                Ok(n) => {
                    let mut buf = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let room = (max + 1).saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                    if buf.len() > max {
                        end.truncated = true;
                        overflow.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = done.send(end);
    });
    if spawned.is_err() {
        failed.store(true, Ordering::SeqCst);
    }
    Pipe { buffer }
}

fn file_limit_exceeded(limits: &[(PathBuf, u64)]) -> bool {
    limits.iter().any(|(path, limit)| match std::fs::metadata(path) {
        Ok(meta) => meta.len() > *limit,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    })
}

/// Run one child with capped pipes, a deadline and finite cleanup.
pub fn run_bounded(argv: &[String], timeout_s: f64, options: &RunOptions) -> ChildResult {
    assert!(!argv.is_empty(), "argv cannot be empty");
    assert!(timeout_s.is_finite() && timeout_s > 0.0, "timeout_seconds must be positive and finite");
    assert!(options.max_output_bytes >= 128, "max_output_bytes must be at least 128");
    let max = options.max_output_bytes;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(child_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate(&mut command);
    let mut spawned = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ChildResult {
                launch_error: Some(launch_error_name(&error)),
                cleanup_complete: true,
                ..Default::default()
            };
        }
    };
    let pid = spawned.id();
    let overflow = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let stdout = spawned.stdout.take().map(|p| start_drain(p, max, Arc::clone(&overflow), done_tx.clone()));
    let stderr = spawned.stderr.take().map(|p| start_drain(p, max, Arc::clone(&overflow), done_tx.clone()));
    drop(done_tx);
    let mut owned = OwnedChild { child: Some(spawned) };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.min(86_400.0));
    let mut stop_reason: Option<&str> = None;
    let mut status: Option<ExitStatus> = None;
    let mut interrupted = false;
    let child = owned.child.as_mut().expect("child");
    loop {
        if let Ok(Some(exit)) = child.try_wait() {
            status = Some(exit);
            break;
        }
        if overflow.load(Ordering::SeqCst) {
            stop_reason = Some("output");
            break;
        }
        if file_limit_exceeded(&options.file_limits) {
            stop_reason = Some("file");
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            stop_reason = Some("timeout");
            break;
        }
        if interrupt::requested() {
            interrupted = true;
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
    let mut cleanup_complete = true;
    if status.is_none() {
        status = terminate_and_reap(child, 0.2);
        cleanup_complete = status.is_some();
    }
    if !sweep_group(pid) {
        cleanup_complete = false;
    }
    if status.is_some() {
        owned.child = None; // reaped; nothing left for Drop
    }
    // Both drains must reach EOF (or stop at the cap) shortly after the group is gone.
    let expected = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
    let wait_end = Instant::now() + Duration::from_millis(500);
    let mut finished = 0;
    while finished < expected {
        let left = wait_end.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(left) {
            Ok(end) => {
                if !end.eof && !end.truncated {
                    cleanup_complete = false;
                }
                finished += 1;
            }
            Err(_) => {
                cleanup_complete = false;
                break;
            }
        }
    }
    if overflow.load(Ordering::SeqCst) && stop_reason.is_none() {
        stop_reason = Some("output");
    }
    drop(owned);
    if interrupted {
        std::panic::resume_unwind(Box::new(interrupt::Interrupted));
    }
    let text = |pipe: &Option<Pipe>| {
        pipe.as_ref()
            .map(|p| {
                let buf = p.buffer.lock().unwrap_or_else(|e| e.into_inner());
                String::from_utf8_lossy(&buf[..buf.len().min(max)]).into_owned()
            })
            .unwrap_or_default()
    };
    ChildResult {
        stdout: text(&stdout),
        stderr: text(&stderr),
        returncode: status.map(exit_code),
        timed_out: stop_reason == Some("timeout"),
        output_limited: stop_reason == Some("output"),
        file_limited: stop_reason == Some("file"),
        cleanup_complete,
        launch_error: None,
    }
}

/// The default runner as a seam value.
pub fn default_runner() -> impl Fn(&[String], f64, &RunOptions) -> ChildResult {
    run_bounded
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn timeout_terminates_and_reaps_child_with_finite_wait() {
        let started = Instant::now();
        let result = run_bounded(&sh("sleep 5"), 0.1, &RunOptions::output(256));
        assert!(result.timed_out);
        assert!(result.cleanup_complete);
        assert!(result.returncode.is_some());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn normal_exit_drains_final_detector_marker() {
        let result = run_bounded(
            &sh("printf 'progress=end\\n'; printf 'lavfi.freezedetect.freeze_start: 0.0\\n' >&2"),
            5.0,
            &RunOptions::output(256),
        );
        assert!(result.cleanup_complete);
        assert_eq!(result.returncode, Some(0));
        assert_eq!(result.stdout, "progress=end\n");
        assert!(result.stderr.contains("freeze_start"));
    }

    #[test]
    fn output_overflow_is_capped_and_terminates_child() {
        let result = run_bounded(&sh("yes x | head -c 8192; sleep 5"), 5.0, &RunOptions::output(128));
        assert!(result.output_limited);
        assert!(result.stdout.len() <= 128);
        assert!(result.cleanup_complete);
    }

    #[test]
    fn launch_failure_is_reported_not_raised() {
        let result = run_bounded(&["/nonexistent/dead-air-tool".to_string()], 1.0, &RunOptions::output(128));
        assert_eq!(result.launch_error.as_deref(), Some("FileNotFoundError"));
    }

    #[test]
    fn file_limit_stops_the_child() {
        let directory = std::env::temp_dir().join(format!("dead-air-proc-{}", crate::random::uuid4_hex()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("grow.bin");
        let script = format!("head -c 4096 /dev/zero > '{}'; sleep 5", path.display());
        let options = RunOptions { max_output_bytes: 256, file_limits: vec![(path, 1024)] };
        let result = run_bounded(&sh(&script), 5.0, &options);
        assert!(result.file_limited);
        assert!(result.cleanup_complete);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn interruption_stops_and_reaps_the_child_before_unwinding() {
        let marker = std::env::temp_dir().join(format!("dead-air-pid-{}", crate::random::uuid4_hex()));
        let argv = sh(&format!("echo $$ > '{}'; exec sleep 30", marker.display()));
        let path = marker.clone();
        let payload = std::thread::spawn(move || {
            interrupt::set_interruptible(true);
            interrupt::simulate_after(interrupt::SIGINT, Duration::from_millis(300));
            let started = Instant::now();
            let outcome = std::panic::catch_unwind(|| run_bounded(&argv, 10.0, &RunOptions::output(256)));
            assert!(started.elapsed() < Duration::from_secs(3));
            outcome.unwrap_err()
        })
        .join()
        .unwrap();
        assert!(payload.is::<interrupt::Interrupted>());
        let pid: i32 = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
        let _ = std::fs::remove_file(&path);
        // SAFETY: probing a pid with signal 0 sends nothing.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "interrupted child {pid} is still running");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exited_parent_cannot_leave_an_owned_descendant_holding_pipes() {
        let script = "sh -c 'trap \"\" TERM; sleep 30' & echo $!";
        let result = run_bounded(&sh(script), 2.0, &RunOptions::output(256));
        let pid: i32 = result.stdout.trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|rest| rest.split_whitespace().next().map(str::to_string)));
        // SAFETY: cleanup in case the assertion below fails.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert!(matches!(state.as_deref(), None | Some("Z")), "descendant state {state:?}");
        assert!(result.cleanup_complete);
    }

    #[test]
    fn environment_excludes_proxy_and_credentials() {
        let env = child_environment();
        assert!(env.iter().all(|(k, _)| k != "HTTP_PROXY" && k != "HTTPS_PROXY" && !k.contains("TOKEN")));
        assert!(env.contains(&("LC_ALL".to_string(), "C".to_string())));
    }
}
