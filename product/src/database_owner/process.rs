//! Owned process lifetime and pipe workers; a blocked pipe never owns the timer.
use super::protocol::{self, Reply};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

pub(super) struct ExitState {
    child: Mutex<Child>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ExitState {
    pub(super) fn peak_memory(&self) -> Option<(usize, usize)> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then_some((info.PeakProcessMemoryUsed, info.PeakJobMemoryUsed))
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
    pub(super) fn pid(&self) -> u32 {
        self.child.lock().map(|child| child.id()).unwrap_or(0)
    }
    pub(super) fn terminate(&self) {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                self.job.as_raw_handle(),
                124,
            );
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
    pub(super) fn confirmed_dead(&self) -> bool {
        let exited = self
            .child
            .lock()
            .ok()
            .is_some_and(|mut child| matches!(child.try_wait(), Ok(Some(_))));
        if !exited {
            return false;
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            ok != 0 && info.ActiveProcesses == 0
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
}
impl Drop for ExitState {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(super) struct Process {
    pub exit: Arc<ExitState>,
    outbound: mpsc::SyncSender<Vec<u8>>,
    inbound: mpsc::Receiver<Result<Reply, String>>,
}

fn executable() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    if !cfg!(test) {
        return Ok(current);
    }
    let profile = current
        .parent()
        .and_then(|path| path.parent())
        .ok_or("database owner test executable has no profile parent")?;
    let cli = profile.join(if cfg!(windows) {
        "facial-cli.exe"
    } else {
        "facial-cli"
    });
    if !cli.is_file() {
        return Err("database owner tests require the guarded facial-cli build".into());
    }
    Ok(cli)
}

impl Process {
    pub(super) fn spawn() -> Result<Self, String> {
        let mut command = Command::new(executable()?);
        command
            .arg("__database-owner-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if super::runtime::phase_trace_enabled() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            let parent = libc::getpid();
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("database owner parent exited"));
                }
                Ok(())
            });
        }
        // Child waits for the startup frame before opening an engine. If job
        // containment fails, it is killed without ever receiving that frame.
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn database owner: {e}"))?;
        #[cfg(windows)]
        let job = match create_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut input = child
            .stdin
            .take()
            .ok_or("database owner stdin unavailable")?;
        let mut output = child
            .stdout
            .take()
            .ok_or("database owner stdout unavailable")?;
        let exit = Arc::new(ExitState {
            child: Mutex::new(child),
            #[cfg(windows)]
            job,
        });
        let (outbound, outgoing) = mpsc::sync_channel::<Vec<u8>>(1);
        let (incoming, inbound) = mpsc::sync_channel(1);
        let writer_exit = exit.clone();
        std::thread::Builder::new()
            .name("database-owner-write".into())
            .spawn(move || {
                while let Ok(bytes) = outgoing.recv() {
                    if protocol::write_frame(&mut input, &bytes).is_err() {
                        writer_exit.terminate();
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("database-owner-read".into())
            .spawn(move || loop {
                let reply = protocol::read_frame::<Reply>(&mut output);
                let failed = reply.is_err();
                if incoming.send(reply).is_err() || failed {
                    break;
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            exit,
            outbound,
            inbound,
        })
    }

    pub(super) fn exchange(&self, bytes: Vec<u8>, deadline: Instant) -> Result<Reply, String> {
        if Instant::now() >= deadline {
            return Err("safe_unit_timeout: database owner deadline before dispatch".into());
        }
        self.outbound
            .try_send(bytes)
            .map_err(|_| "database_owner_pipe_backpressure".to_string())?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.inbound.recv_timeout(remaining) {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err("safe_unit_timeout: database owner execution deadline".into())
            }
            Err(_) => Err("database_owner_exited".into()),
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.exit.terminate();
    }
}

#[cfg(windows)]
fn create_job(child: &Child) -> Result<std::os::windows::io::OwnedHandle, String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::*;
    unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let job = OwnedHandle::from_raw_handle(raw);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as _,
            std::mem::size_of_val(&limits) as u32,
        ) == 0
            || AssignProcessToJobObject(raw, child.as_raw_handle()) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(job)
    }
}

pub(super) fn retain_until_exit<T: Send + 'static>(exits: Vec<Arc<ExitState>>, value: T) {
    if exits.iter().all(|exit| exit.confirmed_dead()) {
        drop(value);
        return;
    }
    // An extra owner preserves the charge if the observer cannot be spawned.
    let retained = Arc::new(Mutex::new(Some(value)));
    let worker = retained.clone();
    if std::thread::Builder::new()
        .name("database-owner-exit".into())
        .spawn(move || {
            while exits.iter().any(|exit| !exit.confirmed_dead()) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Ok(mut value) = worker.lock() {
                value.take();
            }
        })
        .is_err()
    {
        std::mem::forget(retained);
    }
}
