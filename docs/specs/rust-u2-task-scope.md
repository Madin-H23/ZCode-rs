# U2：任务作用域

日期：2026-10-09。上级规划：[rust-unified-host.md](rust-unified-host.md) 的 U2（“所有 spawn 归入 `TaskScope`”）与 [rust-engineering-practices.md](rust-engineering-practices.md) 的 E7。前置：[rust-u2-host-structure.md](rust-u2-host-structure.md)（crate 拆分）。

## 1. 要解决的问题

Host 有约 139 处、runtime 有约 32 处直接的 `tokio::spawn`。它们的共同问题：

- **没有归属**。连接断开、workspace 关闭、runtime 实例结束之后，它们派生的任务是否还在运行，只能靠每处自己保存句柄或取消令牌来保证。U3 把 runtime 放进 Host 进程之后，已经出现过一次泄漏：Engine 循环 panic 后 run 任务仍在请求模型（rust-u3-single-process.md 2.5）。
- **panic 无声**。tokio 只结束出错的任务；没有人 await 它的句柄时，没有日志，等待它结果的一方永远等不到。
- **关闭时无法确认**。Host 退出前不知道还有哪些任务在运行。

## 2. 规则

1. 每个任务属于一个作用域。作用域有唯一的所有者；所有者释放作用域时，其中仍在运行的任务被中止。任务不能比它的作用域活得更久。
2. 作用域可以有子作用域。关闭或释放父作用域，等同于关闭或释放所有子作用域。
3. 任务有名字（静态字符串，说明它在做什么）。panic 被捕获并交给根作用域的上报函数（作用域路径、任务名、panic 信息），等待该任务结果的一方得到错误，而不是一直等待。
4. 关闭作用域分三步：发出取消信号 → 在给定的宽限期内等任务自行结束 → 中止剩余任务并返回它们的名字。关闭之后在这个作用域上启动的任务不会运行，其句柄立即得到“作用域已关闭”。
5. `tokio::spawn` 与 `tokio::task::spawn_blocking` 只允许出现在 `zcode-async` 内。各 crate 迁移完成后由 `clippy.toml` 的 `disallowed-methods` 强制；迁移期间按 crate 逐个开启。

## 3. 接口（`zcode-async`）

```rust
/// 作用域的所有者。不可克隆；释放时中止其中（含子作用域）仍在运行的任务。
pub struct TaskScope { /* … */ }
/// 在某个作用域里启动任务的权限。可克隆，可以交给任务自己；不延长作用域的寿命。
#[derive(Clone)]
pub struct Spawner { /* … */ }

impl TaskScope {
    /// 根作用域。`report` 在任务 panic 时被调用一次（在出错任务所在的线程上，不能阻塞）。
    pub fn root(name: impl Into<String>, report: impl Fn(&TaskPanic) + Send + Sync + 'static) -> Self;
    pub fn spawner(&self) -> Spawner;
    /// 取消信号 → 等待至多 `grace` → 中止其余任务。返回被中止的任务（作用域路径与任务名）。
    pub async fn close(&self, grace: Duration) -> Vec<String>;
}
impl Spawner {
    /// 子作用域：路径为 `<父路径>/<name>`，取消信号随父作用域传播。
    pub fn child(&self, name: impl Into<String>) -> TaskScope;
    pub fn spawn<F>(&self, name: &'static str, future: F) -> Task<F::Output>;
    pub fn spawn_blocking<F, T>(&self, name: &'static str, work: F) -> Task<T>;
    /// 作用域收到取消信号时完成；需要自行收口的任务用它。
    pub fn cancelled(&self) -> impl Future<Output = ()>;
    pub fn is_closed(&self) -> bool;
    /// 仍在运行的任务名（含子作用域），用于关闭日志与测试。
    pub fn running(&self) -> Vec<String>;
}

/// 任务句柄。丢弃句柄不影响任务（任务仍属于作用域）；`abort` 中止它。
pub struct Task<T> { /* … */ }
impl<T> Future for Task<T> { type Output = Result<T, TaskError>; }
pub enum TaskError { Panicked(Panicked), Aborted, ScopeClosed }
pub struct TaskPanic { pub scope: String, pub task: &'static str, pub message: String }
```

- `spawn_blocking` 的任务无法被中止（线程里的同步代码不会让出）；关闭时它们计入“仍在运行”，宽限期过后不再等待。
- 作用域内部状态用 `std::sync::Mutex` 保护，不跨 await 持有。

## 4. 作用域的划分

| 作用域                    | 所有者                                      | 何时关闭                              |
| ------------------------- | ------------------------------------------- | ------------------------------------- |
| `host`（根）              | `HostCore`                                  | Host 退出（`dispose` / 控制通道关闭） |
| `host/connection:<id>`    | 连接                                        | 连接断开                              |
| `host/<service>`          | 各 service（组装时由根派生）                | 随 Host                               |
| `host/agent/<key>#<代次>` | runtime 实例的监视任务所在的 `AgentRuntime` | 实例结束或被回收                      |
| `host/remote/<session>`   | 远端连接注册表里的一个会话                  | 会话断开或被回收                      |

runtime 内部的任务（run、标题、hook、辅助模型请求）属于它所在实例的作用域：Engine 结束（包括 panic 之后的收口）时关闭。

迁移顺序：先让每个 service 从组装根拿到自己的 `Spawner`，把直接的 `tokio::spawn` 换成 `spawner.spawn("…", …)`；寿命比 service 短的任务（连接、实例、会话）再收进更小的作用域。迁移不改变任务做的事。

## 5. 验收

- `zcode-async` 的测试：panic 上报且句柄得到错误；释放作用域中止任务；`close` 的三步（自行结束的不算中止，超时的被中止并列出）；关闭后启动的任务不运行；子作用域随父作用域关闭；任务结束后不再出现在 `running` 里。
- Host 与 runtime 的非测试代码中，`tokio::spawn` / `spawn_blocking` 只出现在 `zcode-async`，由 clippy 强制。
- Host 退出时日志里列出被中止的任务；正常退出时这个列表为空或只含已知的常驻任务。
- 现有测试与桌面 E2E 全部通过。
