//! Starting and stopping agent processes and everything they start.

use std::process::{Command, Stdio};

/// No console window for the CLI, and on Unix its own process group, so
/// `kill_tree` can take the whole group.
#[cfg(windows)]
pub(crate) fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
pub(crate) fn hide_window(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Agent CLIs start their own children (shells, servers); take them all down.
pub(crate) fn kill_tree(pid: u32) {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    } else {
        let mut c = Command::new("kill");
        c.args(["-TERM", &format!("-{pid}")]);
        c
    };
    hide_window(&mut cmd);
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Ties a process, and whatever it starts from now on, to desk's lifetime.
/// If desk dies without cleaning up (a crash, Task Manager), Windows ends
/// them too, so the next supervisor never finds a CLI still writing to a
/// session it's about to resume. On Unix this is a no-op for now.
#[cfg(windows)]
pub(crate) fn adopt(pid: u32) {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    struct Job(HANDLE);
    // The handle is only ever passed to thread-safe Win32 calls.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
    static JOB: OnceLock<Option<Job>> = OnceLock::new();

    // SAFETY: plain Win32 calls on handles we own; `info` outlives the call
    // that reads it, and every opened process handle is closed.
    let job = JOB.get_or_init(|| unsafe {
        let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if h.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        if SetInformationJobObject(h, JobObjectExtendedLimitInformation, &info as *const _ as *const _, size)
            == 0
        {
            CloseHandle(h);
            return None;
        }
        // Deliberately never closed: closing it is what ends the processes.
        Some(Job(h))
    });
    let Some(job) = job else { return };
    unsafe {
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if !process.is_null() {
            AssignProcessToJobObject(job.0, process);
            CloseHandle(process);
        }
    }
}

#[cfg(unix)]
pub(crate) fn adopt(_pid: u32) {}
