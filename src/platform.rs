//! Platform-specific bits: child-process-tree cleanup on Windows, graceful
//! SIGTERM + process-group kill on Unix, and the whitelist `signal` action.

/// Whitelisted signals for the `signal` action (actions spec): TERM, INT,
/// HUP, QUIT, USR1, USR2. KILL/STOP/CONT are deliberately excluded and
/// rejected with an explanation by [`Signal::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Int,
    Hup,
    Quit,
    Usr1,
    Usr2,
}

impl Signal {
    /// Canonical uppercase name.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Int => "INT",
            Signal::Hup => "HUP",
            Signal::Quit => "QUIT",
            Signal::Usr1 => "USR1",
            Signal::Usr2 => "USR2",
        }
    }

    /// Case-insensitive whitelist parse; an optional `SIG` prefix is accepted
    /// (`sigusr1` == `USR1` == `usr1`). Off-whitelist names are rejected with
    /// an explanatory message (KILL/STOP/CONT get their reasons spelled out).
    pub fn parse(s: &str) -> Result<Signal, String> {
        let t = s.trim().to_ascii_uppercase();
        let t = t.strip_prefix("SIG").unwrap_or(&t);
        match t {
            "TERM" => Ok(Signal::Term),
            "INT" => Ok(Signal::Int),
            "HUP" => Ok(Signal::Hup),
            "QUIT" => Ok(Signal::Quit),
            "USR1" => Ok(Signal::Usr1),
            "USR2" => Ok(Signal::Usr2),
            "KILL" => Err(format!(
                "signal {s:?} is not in the whitelist (TERM, INT, HUP, QUIT, USR1, USR2); \
                 to stop the program use `xkeeper stop <name>` instead of KILL"
            )),
            "STOP" | "CONT" => Err(format!(
                "signal {s:?} is not in the whitelist (TERM, INT, HUP, QUIT, USR1, USR2); \
                 STOP/CONT would freeze/resume the process behind the supervisor's back \
                 and distort its state machine"
            )),
            other => Err(format!(
                "signal {other:?} is not in the whitelist (TERM, INT, HUP, QUIT, USR1, USR2)"
            )),
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use super::Signal;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
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

    /// POSIX signals do not exist on Windows: the `signal` action refuses
    /// loudly instead of silently degrading (actions spec).
    pub fn send_signal(_pid: u32, sig: Signal) -> Result<(), String> {
        Err(format!(
            "the signal action is not supported on windows (cannot deliver {}); \
             use `xkeeper restart <name>` for equivalent service control",
            sig.name()
        ))
    }
}

#[cfg(unix)]
mod imp {
    use std::process::Child;

    use super::Signal;

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

    /// Deliver one whitelist signal to the pid itself — never its process
    /// group (group cleanup is `stop`'s semantics, actions spec).
    pub fn send_signal(pid: u32, sig: Signal) -> Result<(), String> {
        let n = match sig {
            Signal::Term => libc::SIGTERM,
            Signal::Int => libc::SIGINT,
            Signal::Hup => libc::SIGHUP,
            Signal::Quit => libc::SIGQUIT,
            Signal::Usr1 => libc::SIGUSR1,
            Signal::Usr2 => libc::SIGUSR2,
        };
        // SAFETY: kill(2) with a plain signal is always memory-safe.
        let rc = unsafe { libc::kill(pid as libc::pid_t, n) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!(
                "failed to deliver {} to pid {pid}: {}",
                sig.name(),
                std::io::Error::last_os_error()
            ))
        }
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// The `signal` action whitelist parses case-insensitively (with or
    /// without a SIG prefix) and rejects everything else with reasons.
    #[test]
    fn signal_parse_whitelist() {
        for (text, want) in [
            ("TERM", Signal::Term),
            ("term", Signal::Term),
            ("sigusr1", Signal::Usr1),
            ("Usr2", Signal::Usr2),
            (" INT ", Signal::Int),
            ("HUP", Signal::Hup),
            ("QUIT", Signal::Quit),
        ] {
            assert_eq!(Signal::parse(text).unwrap(), want, "{text:?}");
        }
        // KILL is rejected with the stop alternative spelled out.
        let e = Signal::parse("KILL").err().unwrap();
        assert!(e.contains("whitelist") && e.contains("stop"), "{e}");
        // STOP/CONT are rejected as state-distorting.
        for bad in ["STOP", "CONT"] {
            let e = Signal::parse(bad).err().unwrap();
            assert!(
                e.contains("whitelist") && e.contains("state machine"),
                "{e}"
            );
        }
        // Anything else is plainly off-list.
        assert!(Signal::parse("PWR").unwrap_err().contains("whitelist"));
        // The canonical round-trip.
        for s in [
            Signal::Term,
            Signal::Int,
            Signal::Hup,
            Signal::Quit,
            Signal::Usr1,
            Signal::Usr2,
        ] {
            assert_eq!(Signal::parse(s.name()).unwrap(), s);
        }
    }

    /// unix: a whitelist signal reaches the pid and terminates the child.
    #[test]
    #[cfg(unix)]
    fn send_signal_terminates_child() {
        use std::time::{Duration, Instant};
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        send_signal(child.id(), Signal::Term).expect("SIGTERM delivered");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "child did not die on SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
