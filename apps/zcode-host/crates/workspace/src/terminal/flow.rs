//! 终端输出的流控（docs/specs/rust-u2-gateway.md §2.3）。
//!
//! 修复（2026-10-10 桌面 E2E）：之前 PTY 读多快，输出就以多快的速度进入连接的出站队列，而 Renderer
//! 渲染终端输出每秒只有几 MB。终端里运行 `yes`，积压每秒涨 40–50 MB，约 10 s 越过 256 MiB 上限，
//! Gateway 关闭窗口的连接，窗口停在“正在重连”。现在按连接流控：读某个终端输出的连接里只要有一条
//! 处于 saturated，就暂停读这个终端的 PTY；PTY 缓冲写满后 shell 里的进程写阻塞，与真实终端相同。
//! 这些连接都 drained、关闭或取消订阅后恢复。
//!
//! 每个终端一个门（[`watch`] 的发送端），PTY 读循环每次读之前等门打开。门由这里持有：子进程退出或
//! 终端被移除时 [`Flow::close`] 丢弃发送端，读循环之后不再等待，剩余输出照常读完。
//!
//! ```text
//! onDynamicData(conn) ─► Reader 登记 ─┐
//! saturated / drained(conn) ──────────┼─► 门 = 任一读者连接 saturated ─► PTY 读循环等门打开
//! 订阅结束（Reader 释放）/ 连接关闭 ───┘
//! ```
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use zcode_host_kit::FlowState;

/// 整个 Host 的终端流控表。锁只保护表本身，持有期间不 await；边沿由 Gateway 的连接循环同步送来。
#[derive(Default)]
pub(super) struct Flow {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 出站积压越过高水位、还没回落的连接。
    saturated: HashSet<u64>,
    /// 终端 id → 它的门。
    gates: HashMap<String, Gate>,
}

struct Gate {
    paused: watch::Sender<bool>,
    /// 正在读这个终端输出的连接 → 订阅数。
    readers: HashMap<u64, usize>,
}

impl Gate {
    fn refresh(&self, saturated: &HashSet<u64>) {
        let paused = self.readers.keys().any(|reader| saturated.contains(reader));
        self.paused.send_if_modified(|current| {
            let changed = *current != paused;
            *current = paused;
            changed
        });
    }
}

impl Flow {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 新终端的门（打开）；返回 PTY 读循环等待用的接收端。
    pub(super) fn open(&self, id: &str) -> watch::Receiver<bool> {
        let (paused, receiver) = watch::channel(false);
        self.lock().gates.insert(
            id.to_owned(),
            Gate {
                paused,
                readers: HashMap::new(),
            },
        );
        receiver
    }

    /// 终端的子进程已退出或启动失败：丢弃门，读循环不再等待。
    pub(super) fn close(&self, id: &str) {
        let gate = self.lock().gates.remove(id);
        drop(gate);
    }

    /// `connection` 开始读终端 `id` 的输出；返回的登记释放时（订阅结束）撤销。
    pub(super) fn reader(self: &Arc<Self>, id: &str, connection: u64) -> Reader {
        let mut state = self.lock();
        let State { saturated, gates } = &mut *state;
        if let Some(gate) = gates.get_mut(id) {
            *gate.readers.entry(connection).or_default() += 1;
            gate.refresh(saturated);
        }
        Reader {
            flow: self.clone(),
            id: id.to_owned(),
            connection,
        }
    }

    /// Gateway 送来的水位边沿。连接先 saturated、后订阅时，[`Self::reader`] 立即关门。
    pub(super) fn set_state(&self, connection: u64, flow: FlowState) {
        let mut state = self.lock();
        let changed = match flow {
            FlowState::Saturated => state.saturated.insert(connection),
            FlowState::Drained => state.saturated.remove(&connection),
        };
        if changed {
            state.refresh_readers_of(connection);
        }
    }

    /// 连接关闭：它不再算 saturated（它的订阅已随连接任务结束）。
    pub(super) fn connection_closed(&self, connection: u64) {
        let mut state = self.lock();
        if state.saturated.remove(&connection) {
            state.refresh_readers_of(connection);
        }
    }
}

impl State {
    fn refresh_readers_of(&self, connection: u64) {
        for gate in self.gates.values() {
            if gate.readers.contains_key(&connection) {
                gate.refresh(&self.saturated);
            }
        }
    }
}

/// 一个输出订阅的登记，随订阅流一起释放。
pub(super) struct Reader {
    flow: Arc<Flow>,
    id: String,
    connection: u64,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut state = self.flow.lock();
        let State { saturated, gates } = &mut *state;
        let Some(gate) = gates.get_mut(&self.id) else {
            return;
        };
        if let Some(count) = gate.readers.get_mut(&self.connection) {
            *count -= 1;
            if *count == 0 {
                gate.readers.remove(&self.connection);
            }
        }
        gate.refresh(saturated);
    }
}

/// PTY 读循环每次读之前调用：门关着就等它打开；门已被丢弃（子进程退出）时立即返回。
pub(super) async fn wait_open(paused: &mut watch::Receiver<bool>) {
    if *paused.borrow() {
        let _ = paused.wait_for(|paused| !*paused).await;
    }
}
