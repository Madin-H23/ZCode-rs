//! 按 pid 发送终止信号。Node 的 `child.kill()` 默认是 SIGTERM，而 Rust 的 `Child::kill` 是
//! SIGKILL，需要“先温和终止”的场景（git 超时）用这里。
#[cfg(unix)]
pub fn terminate(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    #[allow(unsafe_code)]
    // SAFETY: kill(2) 只按 pid 投递信号，不涉及任何内存。
    let sent = unsafe { libc::kill(pid, libc::SIGTERM) } == 0;
    sent
}
