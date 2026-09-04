//! Platform-specific bits: child-process-tree cleanup on Windows, graceful
//! SIGTERM + process-group kill on Unix.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Owns a Windows job object the child was assigned to. When the handle is
    /// dropped — or xkeeper itself dies — every process left in the job is
    /// killed by the kernel, so no orphaned grandchildren survive.
    pub struct JobHandle(HANDLE);

    // HANDLE is just a raw pointer; the job object is safe to use from any thread.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        pub fn attach(child: &Child) -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    CloseHandle(job);
                    return None;
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    // Can fail if the parent chain already puts us in an
                    // incompatible job; the child is still killable directly.
                    CloseHandle(job);
                    return None;
                }
                Some(JobHandle(job))
            }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::process::Child;

    /// Placeholder: on Unix, stopping is done with signals instead of a job object.
    pub struct JobHandle;

    impl JobHandle {
        pub fn attach(_child: &Child) -> Option<Self> {
            None
        }
    }

    /// Ask a child's whole process group to terminate gracefully.
    /// The child was started with `Command::process_group(0)`, so its pgid
    /// equals its pid.
    pub fn terminate_gracefully(pid: u32) -> bool {
        // SAFETY: killpg(2) with a plain signal is always memory-safe.
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) == 0 }
    }

    /// Hard-kill a child's whole process group.
    pub fn kill_group(pid: u32) -> bool {
        // SAFETY: killpg(2) with a plain signal is always memory-safe.
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) == 0 }
    }
}

pub use imp::*;
