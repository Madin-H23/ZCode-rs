//! capability 登记：签发时绑定 attachment 元数据，单次有效、到期作废（rust-host-r0.md §3）。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 元数据的类型属于 Gateway 的连接注册表；这里只负责签发与兑换。
pub use zcode_host_gateway::AttachmentMeta;

struct Pending {
    meta: AttachmentMeta,
    expires_at: Instant,
}

pub struct AttachmentRegistry {
    ttl: Duration,
    pending: Mutex<HashMap<String, Pending>>,
}

impl AttachmentRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self, meta: AttachmentMeta) -> String {
        let capability = crate::random_hex(32);
        let now = Instant::now();
        let mut pending = self.lock();
        // 顺带清掉过期项，避免 Renderer 拿到 endpoint 却没连上时登记无限增长。
        pending.retain(|_, entry| entry.expires_at > now);
        pending.insert(
            capability.clone(),
            Pending {
                meta,
                expires_at: now + self.ttl,
            },
        );
        capability
    }

    pub fn redeem(&self, capability: &str) -> Option<AttachmentMeta> {
        let entry = self.lock().remove(capability)?;
        (entry.expires_at > Instant::now()).then_some(entry.meta)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        // 登记表只做插入和删除，持锁时 panic 也不会留下半更新状态，可以继续使用。
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcode_host_control::{AttachmentScope, ClientMode, WindowId};

    fn meta() -> AttachmentMeta {
        AttachmentMeta {
            attachment_id: "a1".into(),
            client_mode: ClientMode::DesktopContinuous,
            scope: AttachmentScope::Local,
            window_id: WindowId::new(3),
        }
    }

    #[test]
    fn capability_is_single_use() {
        let registry = AttachmentRegistry::new(Duration::from_secs(30));
        let capability = registry.issue(meta());
        assert_eq!(capability.len(), 64);
        assert_eq!(registry.redeem(&capability), Some(meta()));
        assert_eq!(registry.redeem(&capability), None);
        assert_eq!(registry.redeem("forged"), None);
    }

    #[test]
    fn expired_capability_is_rejected() {
        let registry = AttachmentRegistry::new(Duration::ZERO);
        let capability = registry.issue(meta());
        assert_eq!(registry.redeem(&capability), None);
    }
}
