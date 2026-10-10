//! Host 自己的 sessions-index 观察者（windowHostSessionsIndexObserver.ts 与 zcodeTaskIndexSyncer.ts
//! 的订阅部分）。Node 为 syncer 和 window-controller 各开一条 CLI 订阅；两者都是 Host 内部的被动
//! 消费者，这里合并成每个 workspace 一条，用 Host 自己的 connectionId，与 Renderer 的订阅互不影响。
//!
//! ```text
//! onAgentRuntimeLifecycle available(第 N 代) ─► 订阅 sessions-index/<key>（只用该代进程，不拉起 CLI）
//!   frames ─► Assembler 重组 ─► snapshot：整表替换（该代第一份为 initial）
//!                             └► deltas：fromSeq 必须等于游标，否则 forceSnapshot resync
//!   每次变化向消费者发 IndexEvent（syncer 写 tasks-index，controller 更新投影）
//! onAgentRuntimeLifecycle unavailable(第 N 代) ─► 停止订阅、清空、发 Offline
//! ```
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_async::Task;
use zcode_host_kit::{EventHub, Overflow, log};

use super::runtime::{AgentRuntime, FrameKind, WorkspaceTarget};
use super::service::uuid_v4;
use super::wire::{Assembled, Assembler};

#[derive(Debug, Clone, PartialEq)]
pub enum IndexEvent {
    /// 订阅或 resync 得到的整表；`initial` 表示该进程代的第一份快照（syncer 据此做静默基线）。
    Snapshot {
        target: WorkspaceTarget,
        previous: HashMap<String, Value>,
        sessions: Vec<Value>,
        initial: bool,
    },
    Upserted {
        target: WorkspaceTarget,
        previous: Option<Value>,
        session: Value,
    },
    Removed {
        target: WorkspaceTarget,
        session_id: String,
    },
    /// 进程不可用：该 workspace 的实时摘要全部失效。
    Offline { target: WorkspaceTarget },
}

struct Watched {
    generation: u64,
    task: Task<()>,
    sessions: HashMap<String, Value>,
}

pub struct SessionsIndexObserver {
    runtime: AgentRuntime,
    connection_id: String,
    watched: Mutex<HashMap<String, Watched>>,
    events: EventHub<Arc<IndexEvent>>,
}

fn lifecycle_target(event: &Value) -> Option<(WorkspaceTarget, u64, bool)> {
    let target = WorkspaceTarget::from_value(event).ok()?;
    let generation = event["runtimeIdentity"]["generation"].as_u64()?;
    let available = event["state"] == "available";
    Some((target, generation, available))
}

impl SessionsIndexObserver {
    pub fn start(runtime: AgentRuntime) -> Arc<Self> {
        let observer = Arc::new(Self {
            runtime: runtime.clone(),
            connection_id: format!("host-{}", uuid_v4()),
            watched: Mutex::new(HashMap::new()),
            // 订阅者是 syncer（每条事件写一次 tasks-index）与 controller；快照是一条事件，增量每次会话
            // 摘要变化一条。4096 条约是卡住几分钟的量，满了结束落后的订阅（rust-u2-task-scope.md §6.2）。
            events: EventHub::new("sessions-index-events", 4096, Overflow::CloseSubscriber),
        });
        let mut lifecycle = Box::pin(runtime.lifecycle().subscribe());
        let weak = Arc::downgrade(&observer);
        drop(
            runtime
                .spawner()
                .spawn_resident("sessions-index-lifecycle", async move {
                    while let Some(event) = lifecycle.next().await {
                        let Some(observer) = weak.upgrade() else {
                            return;
                        };
                        let Some((target, generation, available)) = lifecycle_target(&event) else {
                            continue;
                        };
                        if available {
                            observer.watch(target, generation);
                        } else {
                            observer.unwatch(target, generation);
                        }
                    }
                }),
        );
        observer
    }

    pub fn events(&self) -> EventHub<Arc<IndexEvent>> {
        self.events.clone()
    }

    /// 该 workspace 当前的实时摘要（按 sessionId）。
    pub fn sessions(&self, workspace_key: &str) -> HashMap<String, Value> {
        self.lock()
            .get(workspace_key)
            .map(|watched| watched.sessions.clone())
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Watched>> {
        self.watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn watch(self: &Arc<Self>, target: WorkspaceTarget, generation: u64) {
        let key = target.key();
        let observer = Arc::downgrade(self);
        let task_target = target;
        // 跟随一代实例的 sessions-index 帧；实例 unavailable 或被下一代替换时中止。
        let task = self
            .runtime
            .spawner()
            .spawn("sessions-index-follow", async move {
                let Some(strong) = observer.upgrade() else {
                    return;
                };
                let runtime = strong.runtime.clone();
                let connection_id = strong.connection_id.clone();
                drop(strong);
                let result = follow(
                    observer,
                    runtime,
                    connection_id,
                    task_target.clone(),
                    generation,
                )
                .await;
                if let Err(error) = result {
                    log::debug(format!(
                        "sessions-index observer for {} stopped: {error}",
                        task_target.key()
                    ));
                }
            });
        let previous = self.lock().insert(
            key,
            Watched {
                generation,
                task,
                sessions: HashMap::new(),
            },
        );
        if let Some(previous) = previous {
            previous.task.abort();
        }
    }

    fn unwatch(&self, target: WorkspaceTarget, generation: u64) {
        let key = target.key();
        let removed = {
            let mut watched = self.lock();
            match watched.get(&key) {
                Some(current) if current.generation <= generation => watched.remove(&key),
                _ => None,
            }
        };
        if let Some(removed) = removed {
            removed.task.abort();
            self.events.emit(Arc::new(IndexEvent::Offline { target }));
        }
    }

    /// 写入最新整表或增量，并返回要发出的事件；代已过期时返回空。
    fn apply(
        &self,
        target: &WorkspaceTarget,
        generation: u64,
        frame: &Value,
        initial: bool,
    ) -> Result<Vec<IndexEvent>, ()> {
        let mut watched = self.lock();
        let Some(entry) = watched
            .get_mut(&target.key())
            .filter(|entry| entry.generation == generation)
        else {
            return Ok(Vec::new());
        };
        let payload = &frame["payload"];
        match payload["kind"].as_str() {
            Some("snapshot") => {
                let sessions: Vec<Value> = payload["snapshot"]["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let next: HashMap<String, Value> = sessions
                    .iter()
                    .filter_map(|session| {
                        Some((session["sessionId"].as_str()?.to_owned(), session.clone()))
                    })
                    .collect();
                let previous = std::mem::replace(&mut entry.sessions, next);
                Ok(vec![IndexEvent::Snapshot {
                    target: target.clone(),
                    previous,
                    sessions,
                    initial,
                }])
            }
            Some("deltas") => {
                let mut events = Vec::new();
                for delta in payload["deltas"].as_array().into_iter().flatten() {
                    match delta["op"].as_str() {
                        Some("session.upserted") => {
                            let session = delta["session"].clone();
                            let Some(id) = session["sessionId"].as_str().map(str::to_owned) else {
                                continue;
                            };
                            let previous = entry.sessions.insert(id, session.clone());
                            events.push(IndexEvent::Upserted {
                                target: target.clone(),
                                previous,
                                session,
                            });
                        }
                        Some("session.removed") => {
                            let Some(id) = delta["sessionId"].as_str() else {
                                continue;
                            };
                            entry.sessions.remove(id);
                            events.push(IndexEvent::Removed {
                                target: target.clone(),
                                session_id: id.to_owned(),
                            });
                        }
                        _ => return Err(()),
                    }
                }
                Ok(events)
            }
            _ => Err(()),
        }
    }
}

struct Cursor {
    log_epoch: Value,
    seq: u64,
}

async fn follow(
    observer: std::sync::Weak<SessionsIndexObserver>,
    runtime: AgentRuntime,
    connection_id: String,
    target: WorkspaceTarget,
    generation: u64,
) -> Result<(), String> {
    let key = target.key();
    let process = runtime
        .existing(&target)
        .await
        .filter(|process| process.generation == generation)
        .ok_or("runtime is gone")?;
    // 先挂上帧流再发订阅：CLI 在响应行之后立即写初始快照。
    let frame_key = key.clone();
    let mut frames = Box::pin(runtime.frames().subscribe_filtered(move |frame| {
        frame.kind == FrameKind::SessionsIndex
            && frame.workspace_key == frame_key
            && frame.generation == generation
    }));
    let topic = format!("sessions-index/{key}");
    let ack = process
        .request_value(
            "v4/conversation/subscribe",
            &json!({"topic": topic, "connectionId": connection_id, "clientMode": "desktop-continuous"}),
        )
        .await
        .map_err(|error| error.message)?;
    let subscription_id = ack["ack"]["subscriptionId"]
        .as_str()
        .ok_or("subscribe ack without subscriptionId")?
        .to_owned();
    let mut assembler = Assembler::new();
    let mut cursor: Option<Cursor> = None;
    let mut initial = true;
    while let Some(frame) = frames.next().await {
        if frame.subscription_id != subscription_id {
            continue;
        }
        let Some(observer) = observer.upgrade() else {
            return Ok(());
        };
        let wire: Value =
            serde_json::from_str(frame.raw.get()).map_err(|error| error.to_string())?;
        let logical = match assembler.accept(&wire) {
            Assembled::Pending => continue,
            Assembled::Frame { frame, .. } => Some(frame),
            Assembled::Fault(code) => {
                log::warn(format!("sessions-index frame fault for {key}: {code}"));
                None
            }
        };
        let mut recover = logical.is_none();
        if let Some(logical) = logical {
            let is_snapshot = logical["payload"]["kind"] == "snapshot";
            let from_seq = logical["fromSeq"].as_u64();
            let to_seq = logical["toSeq"].as_u64().unwrap_or_default();
            if !is_snapshot && cursor.as_ref().map(|cursor| cursor.seq) != from_seq {
                recover = true;
            } else {
                match observer.apply(&target, generation, &logical, initial && is_snapshot) {
                    Ok(events) => {
                        if is_snapshot {
                            initial = false;
                            cursor = Some(Cursor {
                                log_epoch: logical["payload"]["snapshot"]["logEpoch"].clone(),
                                seq: to_seq,
                            });
                        } else if let Some(cursor) = cursor.as_mut() {
                            cursor.seq = to_seq;
                        }
                        for event in events {
                            observer.events.emit(Arc::new(event));
                        }
                    }
                    Err(()) => recover = true,
                }
            }
        }
        if recover {
            assembler.reset();
            let base = cursor
                .as_ref()
                .map(|cursor| json!({"logEpoch": cursor.log_epoch, "seq": cursor.seq}))
                .unwrap_or(Value::Null);
            process
                .request_value(
                    "v4/conversation/resync",
                    &json!({
                        "topic": topic, "connectionId": connection_id,
                        "subscriptionId": subscription_id, "base": base, "forceSnapshot": true,
                    }),
                )
                .await
                .map_err(|error| error.message)?;
        }
    }
    Ok(())
}
