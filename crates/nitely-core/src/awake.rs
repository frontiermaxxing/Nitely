//! Keeping the machine from sleeping while something needs it awake: the phone server, so a phone
//! away from home finds the PC (docs/phone.md, "Away from home"). The display may still turn off.
//! Held until the guard is dropped, and let go by the OS when the process ends.

/// While it lives, the machine does not go to sleep on its own.
pub struct KeepAwake {
    /// Windows: dropping it ends the thread that holds the request.
    #[cfg(windows)]
    _release: std::sync::mpsc::Sender<()>,
    /// macOS `caffeinate`, Linux `systemd-inhibit`: each waits on this process, so it lets go by
    /// itself if Nitely dies.
    #[cfg(not(windows))]
    child: Option<std::process::Child>,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        #[cfg(windows)]
        {
            const ES_CONTINUOUS: u32 = 0x8000_0000;
            const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
            let (release, held) = std::sync::mpsc::channel::<()>();
            // The request belongs to the thread that makes it, so one thread holds it for as long as
            // the guard lives.
            std::thread::spawn(move || {
                // SAFETY: takes flags only and has no memory effects.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                let _ = held.recv();
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            });
            KeepAwake { _release: release }
        }
        #[cfg(not(windows))]
        {
            let pid = std::process::id().to_string();
            let child = if cfg!(target_os = "macos") {
                // -i: no idle sleep; -w: until this process exits.
                std::process::Command::new("caffeinate").args(["-i", "-w", &pid]).spawn().ok()
            } else {
                std::process::Command::new("systemd-inhibit")
                    .args(["--what=idle:sleep", "--who=Nitely", "--why=The phone server is on", "--mode=block", "tail", "-f", "--pid", &pid, "/dev/null"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()
            };
            KeepAwake { child }
        }
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(windows))]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().ok();
            c.wait().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_and_lets_go() {
        let first = KeepAwake::new();
        let second = KeepAwake::new();
        #[cfg(windows)]
        {
            // While one is held, Windows reports the system as required. (Other programs may hold it
            // too, so letting go is not checked.)
            #[link(name = "powrprof")]
            extern "system" {
                fn CallNtPowerInformation(level: i32, input: *const std::ffi::c_void, input_len: u32, output: *mut std::ffi::c_void, output_len: u32) -> i32;
            }
            const SYSTEM_EXECUTION_STATE: i32 = 16;
            const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
            let state = || {
                let mut out: u32 = 0;
                // SAFETY: a 4-byte output buffer for the documented EXECUTION_STATE result.
                let status = unsafe { CallNtPowerInformation(SYSTEM_EXECUTION_STATE, std::ptr::null(), 0, (&mut out as *mut u32).cast(), 4) };
                assert_eq!(status, 0);
                out
            };
            let started = std::time::Instant::now();
            while state() & ES_SYSTEM_REQUIRED == 0 {
                assert!(started.elapsed() < std::time::Duration::from_secs(5), "Windows does not report the system as required");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        drop(first);
        drop(second);
    }
}
