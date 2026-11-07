//! Zero orphans (MASTER_PLAN D4): no spawned MCP server may outlive Patchbay.
//!
//! **Windows** — a Job Object: one process-wide, `KILL_ON_JOB_CLOSE`, assigned
//! to each upstream child, so the OS kills the whole tree (cmd -> npx -> node)
//! when Patchbay exits, even on a crash or a kill.
//!
//! **macOS / Unix** — process groups: each child is spawned as the leader of
//! its own group (`process_group(0)` in `stdio.rs`) and its pid is registered
//! here. Stopping a jack kills the whole group ([`kill_tree`]), and quitting
//! kills every registered group ([`kill_all`]). A pid stays reserved until the
//! child is reaped, and `kill_tree` runs before the reap, so a recycled pid is
//! never hit. There is no OS-level kill-on-crash on Unix: if Patchbay itself is
//! killed, a child sees EOF on its stdin, which ends a well-behaved MCP server.
//!
//! Assigns **by pid** via `OpenProcess` (we hold the tokio `Child`, but a pid is
//! the simplest cross-module handle and the Job Object API only needs a process
//! handle). `CREATE_NO_WINDOW` is applied by the caller when spawning, not here.

// ---- Windows implementation ------------------------------------------------

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    use crate::utils::log::log;

    /// Newtype wrapper around a Win32 `HANDLE` that is `Send`+`Sync` so it can
    /// live in shared state. The handle is owned: `Drop` closes it.
    #[derive(Debug)]
    pub struct SendHandle(pub HANDLE);

    // SAFETY: Win32 HANDLEs are opaque kernel-object references, not tied to a
    // thread; operations on them are thread-safe syscalls.
    unsafe impl Send for SendHandle {}
    unsafe impl Sync for SendHandle {}

    /// An open handle to a Job Object with `KILL_ON_JOB_CLOSE`. When the last
    /// handle (this one) closes — Patchbay exits, or the manager is dropped —
    /// Windows auto-terminates every process assigned to the job.
    #[derive(Debug)]
    pub struct Job(SendHandle);

    impl Job {
        /// Create the process-wide Job Object configured to kill all assigned
        /// children when the handle closes. Returns `None` (and logs) on failure;
        /// callers proceed without orphan protection rather than panicking.
        pub fn create_kill_on_close() -> Option<Self> {
            // SAFETY: creating a Job Object and configuring its limits is a safe-
            // by-contract syscall pair; the returned handle is owned and closed on
            // Drop / on the error path below.
            unsafe {
                let job = CreateJobObjectW(None, None).ok()?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok.is_err() {
                    log("process: failed to configure Job Object (KILL_ON_JOB_CLOSE)");
                    let _ = CloseHandle(job);
                    return None;
                }
                log("process: Job Object created — upstream children will be auto-killed on exit");
                Some(Job(SendHandle(job)))
            }
        }

        /// Assign a running process (by pid) to this job. Opens the process with
        /// the minimum access the assignment needs
        /// (`PROCESS_SET_QUOTA | PROCESS_TERMINATE`), assigns it, then closes the
        /// process handle (the job retains the association). Failure is logged,
        /// never panicked — the Job Object is a best-effort backstop.
        pub fn assign_pid(&self, pid: u32) {
            // SAFETY: OpenProcess/AssignProcessToJobObject/CloseHandle are FFI
            // syscalls invoked with a pid we just spawned; the process handle is
            // closed before return.
            unsafe {
                let access = PROCESS_ACCESS_RIGHTS(PROCESS_SET_QUOTA.0 | PROCESS_TERMINATE.0);
                // bInheritHandle = false: we never inherit this handle into
                // children. (windows 0.61 takes a plain `bool` here; the
                // `BOOL` newtype this used to pass no longer exists.)
                let proc_handle = match OpenProcess(access, false, pid) {
                    Ok(h) => h,
                    Err(e) => {
                        log(&format!("process: OpenProcess({}) failed: {}", pid, e));
                        return;
                    }
                };
                if let Err(e) = AssignProcessToJobObject((self.0).0, proc_handle) {
                    log(&format!(
                        "process: AssignProcessToJobObject({}) failed: {}",
                        pid, e
                    ));
                }
                let _ = CloseHandle(proc_handle);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing a job handle is a safe-by-contract syscall; on the
            // last close Windows kills all assigned children (the entire point of
            // KILL_ON_JOB_CLOSE).
            unsafe {
                let _ = CloseHandle((self.0).0);
            }
        }
    }
}

// ---- Unix implementation: process groups -----------------------------------

#[cfg(unix)]
mod imp {
    use std::collections::HashSet;

    use parking_lot::Mutex;

    use crate::utils::log::log;

    /// Process-group ids (= leader pids) of live upstream children.
    static GROUPS: Mutex<Option<HashSet<i32>>> = Mutex::new(None);

    /// Mirrors the Windows `Job` API so callers are identical on every target.
    #[derive(Debug)]
    pub struct Job;

    impl Job {
        pub fn create_kill_on_close() -> Option<Self> {
            log("process: upstream children run in their own process groups, killed on stop and quit");
            Some(Job)
        }

        /// Register a child spawned with `process_group(0)`: its pid is its pgid.
        pub fn assign_pid(&self, pid: u32) {
            GROUPS.lock().get_or_insert_with(HashSet::new).insert(pid as i32);
        }
    }

    fn killpg(pgid: i32) {
        // SAFETY: killpg is a plain syscall on an integer id; the worst outcome
        // of a stale id is ESRCH, which is ignored.
        let rc = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                log(&format!("process: killpg({pgid}) failed: {err}"));
            }
        }
    }

    pub fn kill_tree(pid: u32) {
        let pgid = pid as i32;
        let known = GROUPS.lock().as_mut().map(|g| g.remove(&pgid)).unwrap_or(false);
        if known {
            killpg(pgid);
        }
    }

    pub fn kill_all() {
        let groups: Vec<i32> = GROUPS
            .lock()
            .as_mut()
            .map(|g| g.drain().collect())
            .unwrap_or_default();
        if !groups.is_empty() {
            log(&format!("process: killing {} upstream process group(s) on exit", groups.len()));
        }
        for g in groups {
            killpg(g);
        }
    }
}

#[cfg(windows)]
mod tree {
    /// The Job Object reaps the tree; nothing to do per child.
    pub fn kill_tree(_pid: u32) {}
    /// The Job Object kills everything when the process exits.
    pub fn kill_all() {}
}

#[cfg(unix)]
mod tree {
    pub use super::imp::{kill_all, kill_tree};
}

pub use imp::Job;

/// Kill an upstream child together with everything it started. Call it while
/// the child is still un-reaped (before `wait`). No-op on Windows (Job Object).
pub fn kill_tree(pid: u32) {
    tree::kill_tree(pid)
}

/// Kill every upstream child still running. Call it right before the process
/// exits. No-op on Windows (Job Object).
pub fn kill_all() {
    tree::kill_all()
}
