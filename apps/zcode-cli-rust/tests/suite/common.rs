//! 合并前在几个测试文件里逐字复制的辅助代码，现在共用一份。
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;
use zcode_cli_rust::contract::RuntimeClock;

/// 读一个 HTTP 请求，按 `content-length` 返回 body；连接在读完之前关闭时返回 `None`。
pub(super) async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))?
        .trim()
        .parse()
        .ok()?;
    while bytes.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&bytes[header_end..header_end + length]).into_owned())
}

/// id 从给定值递增、时间固定的时钟。
pub(super) struct Clock(pub(super) AtomicUsize);
impl RuntimeClock for Clock {
    fn id(&self) -> String {
        self.0.fetch_add(1, Ordering::SeqCst).to_string()
    }
    fn now(&self) -> u64 {
        1000
    }
}
