// Copyright (c) Microsoft Corporation. All rights reserved.

use super::{Error, ErrorCategory, Result, WorkPermit};
use std::io::{BufReader, Read};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PIPE_CHUNK: usize = 32 * 1024;
const STDERR_LIMIT: usize = 4096;

#[derive(Clone)]
pub(crate) struct Control {
    permit: Option<Arc<WorkPermit>>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Control {
    pub(crate) fn bootstrap() -> Self {
        Self {
            permit: None,
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn work(permit: &Arc<WorkPermit>) -> Self {
        Self {
            permit: Some(Arc::clone(permit)),
            deadline: permit.deadline(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorCategory::Cancelled,
                "git-process-cancelled",
                "bounded Git process was cancelled",
            ));
        }
        if let Some(permit) = &self.permit {
            permit.check()?;
        }
        if Instant::now() >= self.deadline {
            return Err(Error::new(
                ErrorCategory::Deadline,
                "git-process-deadline",
                "bounded Git process exceeded its deadline",
            ));
        }
        Ok(())
    }
}

pub(crate) struct Pipe {
    receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    current: std::io::Cursor<Vec<u8>>,
    control: Control,
}

impl Read for Pipe {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            self.control.check().map_err(std::io::Error::other)?;
            let read = self.current.read(output)?;
            if read != 0 {
                return Ok(read);
            }
            match self.receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(bytes) => self.current = std::io::Cursor::new(bytes?),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
    }
}

pub(crate) struct PipedProcess {
    child: OwnedChild,
    pub(crate) input: Option<ChildStdin>,
    pub(crate) output: Option<BufReader<Pipe>>,
    output_thread: Option<JoinHandle<()>>,
    error_thread: Option<JoinHandle<std::io::Result<String>>>,
    control: Control,
    _memory: Option<super::work::MemoryCharge>,
}

impl PipedProcess {
    pub(crate) fn spawn(command: &mut Command, control: Control, input: bool) -> Result<Self> {
        control.check()?;
        let memory = control
            .permit
            .as_ref()
            .map(|permit| permit.memory((PIPE_CHUNK * 8) as u64))
            .transpose()?;
        command
            .stdin(if input { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedChild::spawn(command)?;
        let input = child.child.as_mut().and_then(|child| child.stdin.take());
        let mut stdout = child
            .child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or_else(|| Error::corrupt("Git stdout pipe was not created"))?;
        let mut stderr = child
            .child
            .as_mut()
            .and_then(|child| child.stderr.take())
            .ok_or_else(|| Error::corrupt("Git stderr pipe was not created"))?;
        let (sender, receiver) = mpsc::sync_channel(2);
        let mut process = Self {
            child,
            input,
            output: Some(BufReader::new(Pipe {
                receiver,
                current: std::io::Cursor::new(Vec::new()),
                control: control.clone(),
            })),
            output_thread: None,
            error_thread: None,
            control,
            _memory: memory,
        };
        process.output_thread = Some(
            thread::Builder::new()
                .name("managed-git-output".into())
                .spawn(move || {
                    loop {
                        let mut bytes = vec![0; PIPE_CHUNK];
                        let result = stdout.read(&mut bytes);
                        match result {
                            Ok(0) => break,
                            Ok(read) => {
                                bytes.truncate(read);
                                if sender.send(Ok(bytes)).is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _receiver_closed = sender.send(Err(error));
                                break;
                            }
                        }
                    }
                })?,
        );
        process.error_thread = Some(
            thread::Builder::new()
                .name("managed-git-errors".into())
                .spawn(move || {
                    let mut captured = Vec::new();
                    let mut buffer = [0; PIPE_CHUNK];
                    let mut truncated = false;
                    loop {
                        let read = stderr.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        let kept = read.min(STDERR_LIMIT - captured.len());
                        captured.extend_from_slice(&buffer[..kept]);
                        truncated |= read > kept;
                    }
                    let mut result = String::from_utf8_lossy(&captured).into_owned();
                    if truncated {
                        result.push_str("\n[Git diagnostics truncated at 4096 bytes]");
                    }
                    Ok(result)
                })?,
        );
        Ok(process)
    }

    pub(crate) fn read_output(&mut self, limit: usize) -> Result<Vec<u8>> {
        self.read_output_accounted(limit, None, 1)
    }

    pub(crate) fn read_output_accounted(
        &mut self,
        limit: usize,
        mut memory: Option<&mut super::work::MemoryCharge>,
        expansion: u64,
    ) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut buffer = [0; PIPE_CHUNK];
        let output = self
            .output
            .as_mut()
            .ok_or_else(|| Error::corrupt("Git output already consumed"))?;
        loop {
            let read = output.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if bytes
                .len()
                .checked_add(read)
                .is_none_or(|length| length > limit)
            {
                return Err(Error::pressure("git-output-byte-limit"));
            }
            if let Some(memory) = &mut memory {
                memory.grow(
                    (read as u64)
                        .checked_mul(expansion)
                        .ok_or_else(|| Error::pressure("git-output-account-overflow"))?,
                )?;
            }
            bytes
                .try_reserve(read)
                .map_err(|_| Error::pressure("git-output-memory"))?;
            bytes.extend_from_slice(&buffer[..read]);
        }
        Ok(bytes)
    }

    pub(crate) fn finish(mut self) -> Result<(ExitStatus, String)> {
        self.input.take();
        loop {
            self.control.check()?;
            if self.child.exited()? {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let status = self.child.finish()?;
        self.output.take();
        if let Some(thread) = self.output_thread.take() {
            thread
                .join()
                .map_err(|_| Error::corrupt("Git output worker panicked"))?;
        }
        let diagnostics = self
            .error_thread
            .take()
            .ok_or_else(|| Error::corrupt("Git diagnostic worker missing"))?
            .join()
            .map_err(|_| Error::corrupt("Git diagnostic worker panicked"))??;
        Ok((status, diagnostics))
    }
}

impl Drop for PipedProcess {
    fn drop(&mut self) {
        self.input.take();
        self.output.take();
        if let Err(error) = self.child.stop() {
            eprintln!("managed Git process cleanup failed: {error}");
        }
        if let Some(thread) = self.output_thread.take()
            && thread.join().is_err()
        {
            eprintln!("managed Git output worker panicked during cleanup");
        }
        if let Some(thread) = self.error_thread.take() {
            match thread.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("managed Git diagnostic cleanup failed: {error}"),
                Err(_) => eprintln!("managed Git diagnostic worker panicked during cleanup"),
            }
        }
    }
}

struct OwnedChild {
    child: Option<Child>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            Ok(Self {
                child: Some(command.spawn()?),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::{
                io::{AsRawHandle, FromRawHandle, OwnedHandle},
                process::CommandExt,
            };
            use windows_sys::Win32::System::JobObjects::*;
            use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
            #[link(name = "ntdll")]
            unsafe extern "system" {
                fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
            }
            // SAFETY: the unnamed job is caller-owned; all native structures are correctly sized.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let job = unsafe { OwnedHandle::from_raw_handle(job) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            command.creation_flags(CREATE_SUSPENDED);
            let mut child = command.spawn()?;
            // Assignment precedes the child's first instruction, preventing unowned descendants.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0
            {
                let error = std::io::Error::last_os_error();
                child.kill()?;
                child.wait()?;
                return Err(error.into());
            }
            let process = Self {
                child: Some(child),
                job,
            };
            let status = unsafe {
                NtResumeProcess(process.child.as_ref().expect("owned child").as_raw_handle())
            };
            if status < 0 {
                return Err(Error::io(std::io::Error::other(format!(
                    "cannot resume owned Git process: {status:#x}"
                ))));
            }
            Ok(process)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = command;
            Err(Error::incompatible(
                "bounded child-process ownership is unsupported",
            ))
        }
    }

    fn exited(&mut self) -> Result<bool> {
        let Some(child) = &mut self.child else {
            return Ok(true);
        };
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // WNOWAIT keeps the leader unreaped, so its process-group identity cannot be reused.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(false);
                }
                return Err(error.into());
            }
            #[cfg(target_os = "linux")]
            let pid = unsafe { info.si_pid() };
            #[cfg(not(target_os = "linux"))]
            let pid = info.si_pid;
            Ok(pid != 0)
        }
        #[cfg(windows)]
        {
            Ok(child.try_wait()?.is_some())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(Error::incompatible(
                "child-process lifetime proof is unsupported",
            ))
        }
    }

    #[cfg(target_os = "macos")]
    fn only_exited_leader_in_group(&mut self) -> Result<bool> {
        // sys/proc_info.h selector; libc exposes proc_listpids but not this constant.
        const PROC_PGRP_ONLY: libc::c_uint = 2;
        if !self.exited()? {
            return Ok(false);
        }
        let id = self
            .child
            .as_ref()
            .ok_or_else(|| Error::corrupt("process-group leader is already reaped"))?
            .id();
        // XNU's group-signal filter excludes zombies and can return EPERM for
        // an otherwise empty group. Its locked group inventory includes zombies.
        // Two entries distinguish our sole unreaped leader from any other member
        // without a truncated inventory ever becoming proof of quiescence.
        let mut members: [libc::pid_t; 2] = [0; 2];
        let capacity = std::mem::size_of_val(&members);
        let copied = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                id,
                members.as_mut_ptr().cast(),
                capacity as libc::c_int,
            )
        };
        if copied < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let copied = copied as usize;
        if copied > capacity || !copied.is_multiple_of(std::mem::size_of::<libc::pid_t>()) {
            return Err(Error::corrupt("invalid native process-group inventory"));
        }
        Ok(copied == std::mem::size_of::<libc::pid_t>() && members[0] == id as libc::pid_t)
    }

    fn terminate_group(&mut self) -> Result<()> {
        let Some(id) = self.child.as_ref().map(Child::id) else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            // The unreaped, directly owned leader prevents process-group ID reuse.
            if unsafe { libc::killpg(id as libc::pid_t, libc::SIGKILL) } != 0 {
                let error = std::io::Error::last_os_error();
                #[cfg(target_os = "macos")]
                if error.raw_os_error() == Some(libc::EPERM)
                    && self.only_exited_leader_in_group()?
                {
                    return Ok(());
                }
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error.into());
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let _ = id;
            if unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job.as_raw_handle(),
                    1,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<ExitStatus> {
        self.terminate_group()?;
        let status = self
            .child
            .as_mut()
            .ok_or_else(|| Error::corrupt("Git process already reaped"))?
            .wait()?;
        self.child.take();
        Ok(status)
    }

    fn stop(&mut self) -> Result<()> {
        if self.child.is_some() {
            self.finish()?;
        }
        Ok(())
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("owned Git process could not be reaped: {error}");
        }
    }
}

#[cfg(feature = "managed-test-hooks")]
#[doc(hidden)]
pub struct SupervisedChild {
    inner: OwnedChild,
    id: u32,
    status: Option<ExitStatus>,
}

#[cfg(feature = "managed-test-hooks")]
impl SupervisedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let inner = OwnedChild::spawn(command)?;
        let id = inner.child.as_ref().expect("newly owned child").id();
        Ok(Self {
            inner,
            id,
            status: None,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.status.is_none() && self.inner.exited()? {
            self.status = Some(self.inner.finish()?);
        }
        Ok(self.status)
    }

    pub fn kill(&mut self) -> Result<()> {
        self.inner.terminate_group()
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                self.inner.stop()?;
                return Err(Error::new(
                    super::ErrorCategory::Deadline,
                    "child-wait-deadline",
                    "the owned test process exceeded its deadline and was terminated",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub fn stdin(&mut self) -> Option<&mut std::process::ChildStdin> {
        self.inner
            .child
            .as_mut()
            .and_then(|child| child.stdin.as_mut())
    }

    pub fn close_stdin(&mut self) {
        if let Some(child) = self.inner.child.as_mut() {
            drop(child.stdin.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

    const HELPER: &str = "managed::process::tests::owned_process_helper";

    fn helper(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--nocapture"])
            .env("TGREP_OWNED_PROCESS_TEST", mode);
        command
    }

    #[test]
    #[allow(
        clippy::zombie_processes,
        reason = "The outer test owns this entire job/process group and verifies descendant termination."
    )]
    fn owned_process_helper() {
        let Ok(mode) = std::env::var("TGREP_OWNED_PROCESS_TEST") else {
            return;
        };
        if mode == "exit" {
            return;
        }
        if mode == "bytes" {
            std::io::stdout()
                .write_all(&vec![b'x'; 128 * 1024])
                .unwrap();
            return;
        }
        let path = std::path::PathBuf::from(std::env::var_os("TGREP_OWNED_PROCESS_GUARD").unwrap());
        if mode == "descendant" {
            let guard = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            fs2::FileExt::lock_exclusive(&guard).unwrap();
            std::fs::write(path.with_extension("ready"), b"held").unwrap();
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        assert!(matches!(mode.as_str(), "parent" | "orphan"));
        let mut command = helper("descendant");
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.with_extension("ready").exists() {
            assert!(
                Instant::now() < deadline,
                "descendant did not acquire its lifetime guard"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "descendant exited before readiness"
            );
            thread::sleep(Duration::from_millis(5));
        }
        println!("OWNED_DESCENDANT_READY");
        std::io::stdout().flush().unwrap();
        if mode == "orphan" {
            return;
        }
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn exited_leaders_are_reaped_without_losing_live_descendant_cleanup() {
        for mode in ["exit", "orphan"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("descendant.lock");
            let guard = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let mut command = helper(mode);
            command
                .env("TGREP_OWNED_PROCESS_GUARD", &path)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut process = OwnedChild::spawn(&mut command).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !process.exited().unwrap() {
                assert!(Instant::now() < deadline, "owned leader did not exit");
                thread::sleep(Duration::from_millis(5));
            }
            #[cfg(target_os = "macos")]
            assert_eq!(
                process.only_exited_leader_in_group().unwrap(),
                mode == "exit"
            );
            if mode == "orphan" {
                assert!(fs2::FileExt::try_lock_exclusive(&guard).is_err());
            }
            assert!(process.finish().unwrap().success());
            loop {
                match fs2::FileExt::try_lock_exclusive(&guard) {
                    Ok(()) => break,
                    Err(error) => {
                        assert!(
                            Instant::now() < deadline,
                            "owned descendant retained its lifetime guard: {error}"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
    }

    #[test]
    fn cancelled_process_releases_a_real_descendants_os_lifetime_guard() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("descendant.lock");
        let guard = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let control = Control::bootstrap();
        let mut command = helper("parent");
        command.env("TGREP_OWNED_PROCESS_GUARD", &path);
        let mut process = PipedProcess::spawn(&mut command, control.clone(), false).unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                process
                    .output
                    .as_mut()
                    .unwrap()
                    .read_line(&mut line)
                    .unwrap(),
                0
            );
            if line.trim() == "OWNED_DESCENDANT_READY" {
                break;
            }
        }
        assert!(fs2::FileExt::try_lock_exclusive(&guard).is_err());
        control.cancelled.store(true, Ordering::Release);
        assert_eq!(
            process.read_output(1024).unwrap_err().category,
            ErrorCategory::Cancelled
        );
        drop(process);
        fs2::FileExt::try_lock_exclusive(&guard).unwrap();
    }

    #[test]
    fn child_output_limit_is_checked_before_accumulating_unbounded_data() {
        let mut process =
            PipedProcess::spawn(&mut helper("bytes"), Control::bootstrap(), false).unwrap();
        assert_eq!(
            process.read_output(1024).unwrap_err().category,
            ErrorCategory::ResourcePressure
        );
    }
}
