//! Hidden decoder subprocess; callers must run inside the supervised Match worker.
use std::{ffi::OsString, io::Read, path::Path};

// The coordinator admits 512 MiB; reserve 64 MiB outside FFmpeg for bounded
// decoded output, image conversion, pipe buffers and IPC.
pub(crate) const MEMORY_LIMIT: usize = 448 * 1024 * 1024;
const MAX_PIPE_LIMIT: usize = 16 * 1024 * 1024;

fn validate_limits(stdout: usize, stderr: usize) -> Result<(), String> {
    if stdout == 0 || stderr == 0 || stdout > MAX_PIPE_LIMIT || stderr > MAX_PIPE_LIMIT {
        return Err("decoder pipe limit must be within 1..16 MiB".into());
    }
    Ok(())
}

fn drain(mut input: impl Read, limit: usize, terminate: impl Fn()) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = match input.read(&mut buffer) {
            Ok(count) => count,
            Err(_) => {
                terminate();
                return Err("decoder pipe read failed".into());
            }
        };
        if count == 0 {
            return Ok(result);
        }
        if count > limit.saturating_sub(result.len()) {
            terminate();
            return Err("decoder pipe output exceeded bound".into());
        }
        result.extend_from_slice(&buffer[..count]);
    }
}

// Windows CRT argv rules: quote every argument and double backslashes before
// a quote or the closing quote. Work in UTF-16 to preserve non-Unicode paths.
fn quote_argument(argument: &[u16]) -> Result<Vec<u16>, String> {
    if argument.len() > 32766 {
        return Err("decoder argument exceeds Windows bound".into());
    }
    if argument.contains(&0) {
        return Err("decoder argument contains NUL".into());
    }
    let mut output = vec![b'"' as u16];
    let mut slashes = 0;
    for &unit in argument {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        output.extend(std::iter::repeat_n(
            b'\\' as u16,
            slashes * if unit == b'"' as u16 { 2 } else { 1 },
        ));
        if unit == b'"' as u16 {
            output.push(b'\\' as u16);
        }
        output.push(unit);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    Ok(output)
}

pub(crate) fn run(
    executable: &Path,
    args: &[OsString],
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    validate_limits(stdout_limit, stderr_limit)?;
    platform::run(executable, args, stdout_limit, stderr_limit)
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(super) fn run(
        _: &Path,
        _: &[OsString],
        _: usize,
        _: usize,
    ) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
        Err("isolated Match decoder is unsupported on this platform".into())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        sync::Arc,
    };
    use windows_sys::Win32::{
        Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0},
        Security::SECURITY_ATTRIBUTES,
        System::{JobObjects::*, Pipes::CreatePipe, Threading::*},
    };

    fn error() -> String {
        std::io::Error::last_os_error().to_string()
    }
    fn terminate(job: &OwnedHandle) {
        unsafe {
            TerminateJobObject(job.as_raw_handle(), 124);
        }
    }
    struct Process {
        process: OwnedHandle,
        job: Arc<OwnedHandle>,
    }
    impl Drop for Process {
        fn drop(&mut self) {
            terminate(&self.job);
        }
    }

    fn pipe(parent_reads: bool) -> Result<(OwnedHandle, OwnedHandle), String> {
        let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        unsafe {
            if CreatePipe(&mut read, &mut write, &attributes, 0) == 0 {
                return Err(error());
            }
            let read = OwnedHandle::from_raw_handle(read);
            let write = OwnedHandle::from_raw_handle(write);
            let (parent, child) = if parent_reads {
                (read, write)
            } else {
                (write, read)
            };
            if SetHandleInformation(parent.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) == 0 {
                return Err(error());
            }
            Ok((parent, child))
        }
    }

    pub(super) fn run(
        executable: &Path,
        args: &[OsString],
        stdout_limit: usize,
        stderr_limit: usize,
    ) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
        let executable = executable
            .canonicalize()
            .map_err(|_| "decoder executable unavailable")?;
        let application: Vec<u16> = executable.as_os_str().encode_wide().collect();
        let mut command = quote_argument(&application)?;
        for argument in args {
            command.push(b' ' as u16);
            command.extend(quote_argument(&argument.encode_wide().collect::<Vec<_>>())?);
            if command.len() >= 32767 {
                return Err("decoder command exceeds Windows argument bound".into());
            }
        }
        if command.len() >= 32767 {
            return Err("decoder command exceeds Windows argument bound".into());
        }
        command.push(0);
        let application: Vec<u16> = application.into_iter().chain(Some(0)).collect();
        let (input, child_input) = pipe(false)?;
        let (output, child_output) = pipe(true)?;
        let (errors, child_errors) = pipe(true)?;
        let process = unsafe {
            let raw_job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw_job.is_null() {
                return Err(error());
            }
            let job = Arc::new(OwnedHandle::from_raw_handle(raw_job));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.BasicLimitInformation.ActiveProcessLimit = 1;
            limits.ProcessMemoryLimit = MEMORY_LIMIT;
            limits.JobMemoryLimit = MEMORY_LIMIT;
            if SetInformationJobObject(
                raw_job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(error());
            }
            let mut bytes = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut bytes);
            let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
            let attributes = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attributes, 2, 0, &mut bytes) == 0 {
                return Err(error());
            }
            struct AttributeGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
            impl Drop for AttributeGuard {
                fn drop(&mut self) {
                    unsafe {
                        DeleteProcThreadAttributeList(self.0);
                    }
                }
            }
            let _guard = AttributeGuard(attributes);
            let jobs = [raw_job];
            let inherited: [HANDLE; 3] = [
                child_input.as_raw_handle(),
                child_output.as_raw_handle(),
                child_errors.as_raw_handle(),
            ];
            for (attribute, pointer, size) in [
                (
                    PROC_THREAD_ATTRIBUTE_JOB_LIST,
                    jobs.as_ptr() as *const std::ffi::c_void,
                    std::mem::size_of_val(&jobs),
                ),
                (
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                    inherited.as_ptr() as *const std::ffi::c_void,
                    std::mem::size_of_val(&inherited),
                ),
            ] {
                if UpdateProcThreadAttribute(
                    attributes,
                    0,
                    attribute as usize,
                    pointer,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ) == 0
                {
                    return Err(error());
                }
            }
            let mut startup: STARTUPINFOEXW = std::mem::zeroed();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = inherited[0];
            startup.StartupInfo.hStdOutput = inherited[1];
            startup.StartupInfo.hStdError = inherited[2];
            startup.lpAttributeList = attributes;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | BELOW_NORMAL_PRIORITY_CLASS,
                std::ptr::null(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut info,
            ) == 0
            {
                return Err(error());
            }
            drop(OwnedHandle::from_raw_handle(info.hThread));
            Process {
                process: OwnedHandle::from_raw_handle(info.hProcess),
                job,
            }
        };
        drop((input, child_input, child_output, child_errors)); // stdin EOF; only decoder owns pipe writers.
        let output_job = Arc::clone(&process.job);
        let stdout = std::thread::Builder::new()
            .name("match-decoder-stdout".into())
            .spawn(move || {
                drain(std::fs::File::from(output), stdout_limit, || {
                    terminate(&output_job)
                })
            })
            .map_err(|_| "decoder stdout reader unavailable")?;
        let errors_job = Arc::clone(&process.job);
        let stderr = match std::thread::Builder::new()
            .name("match-decoder-stderr".into())
            .spawn(move || {
                drain(std::fs::File::from(errors), stderr_limit, || {
                    terminate(&errors_job)
                })
            }) {
            Ok(reader) => reader,
            Err(_) => {
                terminate(&process.job);
                let _ = stdout.join();
                return Err("decoder stderr reader unavailable".into());
            }
        };
        let wait = unsafe { WaitForSingleObject(process.process.as_raw_handle(), u32::MAX) };
        if wait != WAIT_OBJECT_0 {
            terminate(&process.job);
        }
        let stdout = stdout.join();
        let stderr = stderr.join();
        if wait != WAIT_OBJECT_0 {
            return Err("decoder process wait failed".into());
        }
        let stdout = stdout.map_err(|_| "decoder stdout reader failed")??;
        let stderr = stderr.map_err(|_| "decoder stderr reader failed")??;
        let mut code = 0;
        if unsafe { GetExitCodeProcess(process.process.as_raw_handle(), &mut code) } == 0 {
            return Err(error());
        }
        Ok((code == 0, stdout, stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_argv_quotes_empty_spaces_quotes_and_trailing_backslashes() {
        let quote = |text: &str| {
            String::from_utf16(&quote_argument(&text.encode_utf16().collect::<Vec<_>>()).unwrap())
                .unwrap()
        };
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\"), "\"a\\\\\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
        assert!(quote_argument(&[0]).is_err());
        assert_eq!(quote_argument(&[0xd800]).unwrap(), vec![34, 0xd800, 34]);
        assert!(quote_argument(&vec![65; 32767]).is_err());
    }
    #[test]
    fn decoder_pipe_limit_terminates_once_without_retaining_excess() {
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            drain(&b"abcd"[..], 4, || calls.set(calls.get() + 1)).unwrap(),
            b"abcd"
        );
        assert_eq!(calls.get(), 0);
        assert!(drain(&b"abcde"[..], 4, || calls.set(calls.get() + 1)).is_err());
        assert_eq!(calls.get(), 1);
        assert!(validate_limits(0, 1).is_err());
        assert!(validate_limits(1, MAX_PIPE_LIMIT + 1).is_err());
        assert!(validate_limits(MAX_PIPE_LIMIT, 1).is_ok());
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture"))
            }
        }
        assert!(drain(Broken, 1, || calls.set(calls.get() + 1)).is_err());
        assert_eq!(calls.get(), 2);
    }
}
