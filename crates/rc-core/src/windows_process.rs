//! Windows-only ownership of shell process trees and nonblocking pipe drains.
//! Children must be created suspended, attached to a job, then resumed. This
//! closes the race where a newly spawned shell could create untracked children.
//! A job is lifecycle management, NOT a filesystem/network security sandbox.

use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{ChildStderr, ChildStdout};
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

pub const CREATE_SUSPENDED: u32 = 0x0000_0004;

#[derive(Debug)]
pub struct Job(OwnedHandle, Option<tempfile::TempDir>);

impl Job {
    /// Attach a freshly created, suspended child and resume its primary thread.
    /// On failure the caller must kill/reap the still-suspended direct child.
    ///
    /// # Safety
    /// `process` must be a live handle owned by `pid`, created with CREATE_SUSPENDED.
    pub unsafe fn attach_and_resume(process: HANDLE, pid: u32) -> io::Result<Self> {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObject returned a unique, valid owned handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) }, None);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
            || unsafe { AssignProcessToJobObject(raw, process) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        resume_primary_thread(pid)?;
        Ok(job)
    }

    pub fn kill(&self) {
        // SAFETY: this handle owns only the tool's assigned process tree.
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }

    /// Keep command scripts alive until the background supervisor releases us.
    pub fn with_scripts(mut self, scripts: tempfile::TempDir) -> Self {
        self.1 = Some(scripts);
        self
    }
}

fn resume_primary_thread(pid: u32) -> io::Result<()> {
    // std/tokio Child expose the process handle but not the initial thread.
    // A CREATE_SUSPENDED process has not run application code or spawned children.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::other(
        "could not find the suspended shell's primary thread",
    ))
}

/// Read only bytes already in the pipe. The supervisor is its sole reader;
/// no other thread can consume the bytes between PeekNamedPipe and ReadFile.
pub fn read_available<R: Read + AsRawHandle>(
    stdout: &mut R,
    bytes: &mut [u8],
) -> io::Result<usize> {
    let mut available = 0;
    if unsafe {
        PeekNamedPipe(
            stdout.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let count = bytes.len().min(available as usize);
    stdout.read(&mut bytes[..count])
}

pub struct Output {
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

impl Output {
    pub fn drain_into(&mut self, writer: &mut impl Write) -> bool {
        fn drain_one(pipe: &mut (impl Read + AsRawHandle), writer: &mut impl Write) -> bool {
            let mut bytes = [0u8; 16 * 1024];
            if let Ok(count) = read_available(pipe, &mut bytes) {
                if count > 0 {
                    let _ = writer.write_all(&bytes[..count]);
                    return true;
                }
            }
            false
        }
        let mut wrote = false;
        for _ in 0..32 {
            let stdout = drain_one(&mut self.stdout, writer);
            let stderr = drain_one(&mut self.stderr, writer);
            if !stdout && !stderr {
                break;
            }
            wrote = true;
        }
        wrote
    }
}
