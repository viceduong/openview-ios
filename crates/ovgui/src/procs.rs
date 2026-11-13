//! Child-process plumbing: spawn hidden with piped output (status events
//! and log lines forwarded to the UI), a kill-on-close job so host/relay
//! never outlive a crashed GUI, graceful stop (Ctrl-Break so ovhost restores
//! the wallpaper and removes its UPnP mapping), and lookup of processes by
//! image name.
//!
//! Secrets (the host PIN) are handed to children through the environment
//! (`OPENVIEW_PIN`), never on the command line, which other processes can
//! read.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use ovcommon::status::{self, StatusEvent};

use crate::win::CREATE_NO_WINDOW;

/// A process we can stop: its pid plus an owned handle (no pid-reuse races).
#[derive(Debug)]
pub struct Proc {
    pub pid: u32,
    handle: OwnedHandle,
}

impl Proc {
    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// True while the process runs.
    pub fn alive(&self) -> bool {
        // SAFETY: waiting on our own valid handle with a zero timeout.
        unsafe { WaitForSingleObject(self.raw(), 0) != WAIT_OBJECT_0 }
    }

    /// Block until exit or `timeout`; true if it exited.
    pub fn wait(&self, timeout: Duration) -> bool {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: as above.
        unsafe { WaitForSingleObject(self.raw(), ms) == WAIT_OBJECT_0 }
    }

    /// Open an existing process (for adopting an external host).
    pub fn open(pid: u32) -> Option<Self> {
        // SAFETY: OpenProcess returns an owned handle we wrap immediately.
        let h = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .ok()?;
        // SAFETY: `h` is a fresh handle we own.
        let handle =
            unsafe { <OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(h.0) };
        Some(Self { pid, handle })
    }

    /// Ask nicely (Ctrl-Break to its console group), then terminate.
    pub fn stop(&self, grace: Duration) {
        if !self.alive() {
            return;
        }
        if send_ctrl_break(self.pid) && self.wait(grace) {
            return;
        }
        // SAFETY: terminating a process we hold PROCESS_TERMINATE on.
        unsafe {
            let _ = TerminateProcess(self.raw(), 1);
        }
        self.wait(Duration::from_secs(3));
    }
}

/// Serializes console attach/detach (process-global state).
static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

/// Deliver CTRL_BREAK to `pid`'s process group via its console. Works for
/// processes we started with CREATE_NEW_PROCESS_GROUP (group id = pid) and
/// for hidden-console processes started by the legacy script.
fn send_ctrl_break(pid: u32) -> bool {
    let _g = CONSOLE_LOCK.lock();
    // SAFETY: documented attach/ignore/generate/detach sequence; our own
    // handler is disabled while attached so the event cannot kill us.
    unsafe {
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            return false;
        }
        let _ = SetConsoleCtrlHandler(None, true);
        let ok = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).is_ok();
        let _ = FreeConsole();
        // Give the event a moment to be dispatched before re-enabling.
        std::thread::sleep(Duration::from_millis(50));
        let _ = SetConsoleCtrlHandler(None, false);
        ok
    }
}

struct Job(HANDLE);
// SAFETY: a job handle is a kernel object handle, usable from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

fn job() -> Option<&'static Job> {
    static JOB: OnceLock<Option<Job>> = OnceLock::new();
    JOB.get_or_init(|| {
        // SAFETY: creating an anonymous job and setting a plain limit struct.
        unsafe {
            let h = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                u32::try_from(std::mem::size_of_val(&info)).unwrap_or(0),
            )
            .is_err()
            {
                let _ = CloseHandle(h);
                return None;
            }
            Some(Job(h))
        }
    })
    .as_ref()
}

/// What a spawned child reports back.
#[derive(Debug)]
pub enum Output {
    /// A status event (a JSON line on the child's stdout).
    Status(StatusEvent),
    /// A human-readable log line (stderr, or plain-text stdout).
    Log(String),
    Exited(Option<i32>),
}

/// How to start a child.
pub struct SpawnSpec<'a> {
    pub exe: &'a Path,
    pub args: &'a [String],
    /// Extra environment variables (secrets go here, not into `args`).
    pub envs: &'a [(&'a str, &'a str)],
    pub cwd: &'a Path,
    /// Bind to the kill-on-close job (the child dies with the GUI).
    pub in_job: bool,
    /// Append the child's log lines here.
    pub log_path: Option<&'a Path>,
    /// The child prints [`StatusEvent`] JSON lines on stdout.
    pub status_stdout: bool,
}

/// Spawn hidden, in its own process group, optionally bound to the
/// kill-on-close job. Status events, log lines and the final exit code go
/// to `sink` (called on reader/waiter threads); log lines are also
/// appended to `log_path`.
pub fn spawn(
    spec: &SpawnSpec<'_>,
    sink: impl Fn(Output) + Send + Sync + 'static,
) -> Result<Proc, String> {
    let SpawnSpec {
        exe,
        args,
        envs,
        cwd,
        in_job,
        log_path,
        status_stdout,
    } = *spec;
    let mut child = Command::new(exe)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP.0)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let handle = child
        .as_handle()
        .try_clone_to_owned()
        .map_err(|e| format!("duplicate handle: {e}"))?;
    let pid = child.id();
    if in_job {
        if let Some(j) = job() {
            // SAFETY: both handles are valid.
            unsafe {
                let _ = AssignProcessToJobObject(j.0, HANDLE(handle.as_raw_handle()));
            }
        }
    }
    let log = log_path.and_then(crate::log::open_capped).map(Mutex::new);
    let shared = std::sync::Arc::new((sink, log));
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| (Box::new(s) as Box<dyn Read + Send>, status_stdout)),
        child
            .stderr
            .take()
            .map(|s| (Box::new(s) as Box<dyn Read + Send>, false)),
    ]
    .into_iter()
    .flatten()
    .map(|(stream, is_status)| {
        let shared = shared.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if is_status {
                    if let Some(ev) = status::parse(&line) {
                        (shared.0)(Output::Status(ev));
                        continue;
                    }
                }
                if let Some(f) = &shared.1 {
                    if let Ok(mut f) = f.lock() {
                        use std::io::Write;
                        let _ = writeln!(f, "{line}");
                    }
                }
                (shared.0)(Output::Log(line));
            }
        })
    })
    .collect();
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        // Deliver the tail of the output before the exit.
        for r in readers {
            let _ = r.join();
        }
        (shared.0)(Output::Exited(code));
    });
    Ok(Proc { pid, handle })
}

/// Pids of running processes whose image name matches (case-insensitive).
pub fn find_by_name(image: &str) -> Vec<u32> {
    let mut out = Vec::new();
    // SAFETY: standard toolhelp walk over a snapshot we close afterwards.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..n]);
            if name.eq_ignore_ascii_case(image) && e.th32ProcessID != std::process::id() {
                out.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Launch a detached process (the viewer) that is NOT in our job and does
/// not write into our pipes, so it survives OpenView quitting. Its stderr
/// (logs) goes to the spec's `log_path`, its stdout (status events) to
/// `status_path`; a tail thread forwards new status events to `sink`.
pub fn spawn_detached(
    spec: &SpawnSpec<'_>,
    status_path: &Path,
    sink: impl Fn(Output) + Send + 'static,
) -> Result<Proc, String> {
    let SpawnSpec {
        exe,
        args,
        envs,
        cwd,
        log_path,
        ..
    } = *spec;
    let log_path = log_path.ok_or("no log path")?;
    for p in [log_path, status_path] {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
    }
    let err =
        std::fs::File::create(log_path).map_err(|e| format!("log {}: {e}", log_path.display()))?;
    let out = std::fs::File::create(status_path)
        .map_err(|e| format!("status {}: {e}", status_path.display()))?;
    let child = Command::new(exe)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP.0)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let handle = child
        .as_handle()
        .try_clone_to_owned()
        .map_err(|e| format!("duplicate handle: {e}"))?;
    let proc = Proc {
        pid: child.id(),
        handle,
    };
    let watch = Proc {
        pid: proc.pid,
        handle: proc.handle.try_clone().map_err(|e| e.to_string())?,
    };
    let path = status_path.to_path_buf();
    std::thread::spawn(move || {
        let mut child = child;
        let mut pos = 0u64;
        let mut partial = String::new();
        let pump = |pos: &mut u64, partial: &mut String| {
            use std::io::{Seek, SeekFrom};
            let Ok(mut f) = std::fs::File::open(&path) else {
                return;
            };
            if f.seek(SeekFrom::Start(*pos)).is_err() {
                return;
            }
            let mut buf = Vec::new();
            if let Ok(n) = f.read_to_end(&mut buf) {
                *pos += n as u64;
                partial.push_str(&String::from_utf8_lossy(&buf));
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    if let Some(ev) = status::parse(&line) {
                        sink(Output::Status(ev));
                    }
                }
            }
        };
        loop {
            let done = watch.wait(Duration::from_millis(250));
            pump(&mut pos, &mut partial);
            if done {
                break;
            }
        }
        if let Some(ev) = status::parse(&partial) {
            sink(Output::Status(ev));
        }
        let code = child.wait().ok().and_then(|s| s.code());
        sink(Output::Exited(code));
    });
    Ok(proc)
}

/// Full image path of a process (works for elevated processes too).
fn image_path(pid: u32) -> Option<std::path::PathBuf> {
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    // SAFETY: query-only handle, closed below; buffer/size describe the same
    // allocation.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).unwrap_or(0);
        let r = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        r.ok()?;
        Some(std::path::PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

/// Running processes launched from exactly `exe` (other copies of the same
/// image elsewhere, e.g. test runs, are ignored). A process whose path
/// cannot be read is included: better to show it than to start a duplicate.
pub fn find_by_exe(exe: &Path) -> Vec<u32> {
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let want = exe.to_string_lossy().to_lowercase();
    find_by_name(name)
        .into_iter()
        .filter(|&pid| image_path(pid).is_none_or(|p| p.to_string_lossy().to_lowercase() == want))
        .collect()
}
