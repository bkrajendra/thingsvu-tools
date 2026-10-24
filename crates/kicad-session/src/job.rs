// SPDX-License-Identifier: GPL-3.0-or-later
//! A Windows Job Object that kills its processes when the last handle closes. The handle lives
//! in this process, so a headless KiCad server cannot outlive us — even after a crash.

use std::io;
use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub(crate) struct KillOnCloseJob(HANDLE);

// The handle is only used through Win32 calls that are safe from any thread.
unsafe impl Send for KillOnCloseJob {}
unsafe impl Sync for KillOnCloseJob {}

impl KillOnCloseJob {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain Win32 calls; every pointer passed is valid for the duration of the call.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: `process` is a live process handle owned by the caller's `Child`.
        let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: the handle was created by CreateJobObjectW and is closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
