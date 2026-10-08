//! 与 Node 写入方共享文件时用的基础设施（docs/specs/rust-host.md §2.6）：
//! 跨进程锁目录协议、原子写入、进程存活判断。格式必须与 TS 实现逐字节一致，
//! 因为 Main、TS CLI、scheduler 仍在读写同一批文件。
pub mod atomic;
pub mod lock;
mod process;

pub use atomic::{atomic_write_private_text, atomic_write_text};
pub use lock::{FileLock, LockOptions, acquire_file_lock, with_file_lock};
// bots 运行锁（bots-runtime-locks）判断持有者存活。
pub use process::is_alive;
