//! Windows Job Object: one process-wide, `KILL_ON_JOB_CLOSE`, assigned to each
//! upstream child so no spawned MCP server outlives Patchbay (even on
//! crash/kill). MASTER_PLAN D4 "zero orphans".
//!
//! Assigns **by pid** via `OpenProcess` (we hold the tokio `Child`, but a pid is
//! the simplest cross-module handle and the Job Object API only needs a process
//! handle). `CREATE_NO_WINDOW` is applied by the caller when spawning, not here.

// ---- Windows implementation ------------------------------------------------

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
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
                // BOOL(0) == FALSE for bInheritHandle (we never inherit this
                // handle into children). Explicit BOOL avoids relying on the
                // `bool: Param<BOOL>` conversion.
                let proc_handle = match OpenProcess(access, BOOL(0), pid) {
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

// ---- Non-Windows stub (keeps the module compiling off-target) --------------

#[cfg(not(windows))]
mod imp {
    use crate::utils::log::log;

    /// No-op Job placeholder on non-Windows targets. The public API mirrors the
    /// Windows variant so callers are identical regardless of target.
    #[derive(Debug)]
    pub struct Job;

    impl Job {
        pub fn create_kill_on_close() -> Option<Self> {
            log("process: Job Object is Windows-only; orphan-kill disabled on this target");
            Some(Job)
        }

        pub fn assign_pid(&self, _pid: u32) {
            // No-op on non-Windows.
        }
    }
}

pub use imp::Job;
