//! 进程存活判断，与 Node `process.kill(pid, 0)` 语义相同：能发信号或无权限都算存活。

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    #[allow(unsafe_code)]
    // SAFETY: kill(pid, 0) 不发送信号，只做存在性与权限检查，不涉及任何内存。
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    #[allow(unsafe_code)]
    // SAFETY: 只在本函数内打开、查询并关闭同一个进程句柄，指针参数指向栈上变量。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // ERROR_ACCESS_DENIED：无权打开同样说明进程存在（Node 的 EPERM 语义）。
            return std::io::Error::last_os_error().raw_os_error() == Some(5);
        }
        let mut code = 0;
        let alive = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
pub fn is_alive(_pid: u32) -> bool {
    true
}
