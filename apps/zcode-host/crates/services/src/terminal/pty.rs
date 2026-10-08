//! PTY 子进程与读写（替代 node-pty）。
//!
//! 输出按读到的块原样转发（不合并、不节流），UTF-8 用流式解码，多字节字符不会被拆开。
//! POSIX 上 master 的读写走 tokio `AsyncFd`，关闭时能真正关掉 fd（node-pty 在退出 200 ms 后
//! 会销毁 socket，残留的后台进程随之收到挂断）；Windows ConPTY 用阻塞线程读写，关闭伪控制台后
//! 读线程自然结束。
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

use crate::support::env::ProcessEnv;

pub struct Spawned {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
}

pub fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// 阻塞调用（openpty + spawn）。
pub fn spawn(
    shell: &str,
    cwd: &str,
    env: &ProcessEnv,
    cols: u16,
    rows: u16,
) -> anyhow::Result<Spawned> {
    let pair = native_pty_system().openpty(size(cols, rows))?;
    let mut command = CommandBuilder::new(shell);
    command.cwd(cwd);
    // CommandBuilder 默认带上本进程环境；清空后只用算好的终端环境。
    command.env_clear();
    for (key, value) in env.vars() {
        command.env(key, value);
    }
    if cfg!(unix) {
        // node-pty 在 POSIX 上会把 PWD 设为 cwd。
        command.env("PWD", cwd);
    }
    let child = pair.slave.spawn_command(command)?;
    // 父进程不持有 slave 端，子进程全部退出后读端才能读到 EOF。
    drop(pair.slave);
    let killer = child.clone_killer();
    Ok(Spawned {
        master: pair.master,
        child,
        killer,
    })
}

/// 流式 UTF-8 解码（等价于 Node 的 StringDecoder）：不完整的尾部留到下一块。
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(invalid) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[invalid..];
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }

    /// 流结束：残留的不完整序列输出为替换字符。
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.pending.clear();
        char::REPLACEMENT_CHARACTER.to_string()
    }
}

/// 正在运行的读写通道。
pub struct PtyIo {
    pub input: mpsc::UnboundedSender<String>,
    /// 读端到 EOF（所有 slave 端关闭）或出错时完成。
    pub reader_done: oneshot::Receiver<()>,
    #[cfg(unix)]
    tasks: Vec<tokio::task::AbortHandle>,
}

impl PtyIo {
    /// 停止读写并释放读写句柄。
    pub fn close(self) {
        #[cfg(unix)]
        for task in self.tasks {
            task.abort();
        }
    }
}

#[cfg(unix)]
mod unix_io {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
    use std::sync::Arc;

    use tokio::io::unix::AsyncFd;

    /// dup 出 master 的独立 fd 并设为非阻塞；它与 master 共享打开文件描述，只用于读写。
    pub fn open(raw: std::os::fd::RawFd) -> std::io::Result<Arc<AsyncFd<File>>> {
        let owned = duplicate(raw)?;
        set_nonblocking(&owned)?;
        Ok(Arc::new(AsyncFd::new(File::from(owned))?))
    }

    #[allow(unsafe_code)]
    fn duplicate(raw: std::os::fd::RawFd) -> std::io::Result<OwnedFd> {
        // SAFETY: raw 是 portable-pty master 持有且仍然打开的 fd，这里只借用来 dup，不接管所有权。
        unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned()
    }

    #[allow(unsafe_code)]
    fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: 对自己持有的有效 fd 调用 fcntl，只读写文件状态标志，不涉及内存。
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: 同上，只追加 O_NONBLOCK。
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// 读到 EOF / EIO（slave 全部关闭）为止。
    pub async fn read(fd: Arc<AsyncFd<File>>, mut on_chunk: impl FnMut(&[u8])) {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let Ok(mut guard) = fd.readable().await else {
                return;
            };
            match guard.try_io(|inner| {
                let mut file: &File = inner.get_ref();
                file.read(&mut buffer)
            }) {
                Ok(Ok(0)) => return,
                Ok(Ok(read)) => on_chunk(&buffer[..read]),
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(_)) => return,
                Err(_would_block) => {}
            }
        }
    }

    pub async fn write(
        fd: Arc<AsyncFd<File>>,
        mut input: tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        while let Some(data) = input.recv().await {
            let mut bytes = data.as_bytes();
            while !bytes.is_empty() {
                let Ok(mut guard) = fd.writable().await else {
                    return;
                };
                match guard.try_io(|inner| {
                    let mut file: &File = inner.get_ref();
                    file.write(bytes)
                }) {
                    Ok(Ok(written)) => bytes = &bytes[written..],
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Ok(Err(_)) => return,
                    Err(_would_block) => {}
                }
            }
        }
    }
}

/// 启动读写；每块输出解码后交给 `on_data`（可能在任意线程调用）。
#[cfg(unix)]
pub fn start(
    master: &dyn MasterPty,
    on_data: impl Fn(String) + Send + Sync + 'static,
) -> std::io::Result<PtyIo> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| std::io::Error::other("pty master has no file descriptor"))?;
    let fd = unix_io::open(raw)?;
    let (input, receiver) = mpsc::unbounded_channel();
    let (done, reader_done) = oneshot::channel();
    let reader = tokio::spawn({
        let fd = fd.clone();
        async move {
            let mut decoder = Utf8Stream::default();
            unix_io::read(fd, |chunk| {
                let text = decoder.push(chunk);
                if !text.is_empty() {
                    on_data(text);
                }
            })
            .await;
            let tail = decoder.finish();
            if !tail.is_empty() {
                on_data(tail);
            }
            let _ = done.send(());
        }
    });
    let writer = tokio::spawn(unix_io::write(fd, receiver));
    Ok(PtyIo {
        input,
        reader_done,
        tasks: vec![reader.abort_handle(), writer.abort_handle()],
    })
}

#[cfg(windows)]
pub fn start(
    master: &dyn MasterPty,
    on_data: impl Fn(String) + Send + Sync + 'static,
) -> std::io::Result<PtyIo> {
    use std::io::{Read, Write};

    let to_io = |error: anyhow::Error| std::io::Error::other(error.to_string());
    let mut reader = master.try_clone_reader().map_err(to_io)?;
    let mut writer = master.take_writer().map_err(to_io)?;
    let (input, mut receiver) = mpsc::unbounded_channel::<String>();
    let (done, reader_done) = oneshot::channel();
    std::thread::spawn(move || {
        let mut decoder = Utf8Stream::default();
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let text = decoder.push(&buffer[..read]);
            if !text.is_empty() {
                on_data(text);
            }
        }
        let tail = decoder.finish();
        if !tail.is_empty() {
            on_data(tail);
        }
        let _ = done.send(());
    });
    std::thread::spawn(move || {
        while let Some(data) = receiver.blocking_recv() {
            if writer
                .write_all(data.as_bytes())
                .and_then(|()| writer.flush())
                .is_err()
            {
                return;
            }
        }
    });
    Ok(PtyIo { input, reader_done })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_stream_never_splits_characters() {
        let mut stream = Utf8Stream::default();
        let bytes = "中a".as_bytes();
        assert_eq!(stream.push(&bytes[..1]), "");
        assert_eq!(stream.push(&bytes[1..]), "中a");
        assert_eq!(stream.push(&[0xff, b'b']), "\u{fffd}b");
        assert_eq!(stream.push(&[0xe4]), "");
        assert_eq!(stream.finish(), "\u{fffd}");
    }
}
