//! Main ↔ Host 的设置消息（docs/specs/rust-u4-app-host.md §2.6、§4.1）：`setting-write` 的结果先于它
//! 引起的 `setting-changed`，`setting-changed` 按写入顺序；Renderer 经 `setting` channel 的写入同样推送。
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_control::{ClientMode, ControlRouter, ControlSink};
use zcode_host_kit::{CallContext, RpcValue};
use zcode_host_services::settings::{self, SettingStore};
use zcode_host_services::settings_control;

use super::common::Control;

struct Setup {
    _home: tempfile::TempDir,
    store: SettingStore,
    router: ControlRouter,
    messages: mpsc::UnboundedReceiver<Value>,
}

fn setup() -> Setup {
    let home = tempfile::tempdir().unwrap();
    let (sink, messages) = mpsc::unbounded_channel();
    let control: Arc<dyn ControlSink> = Arc::new(Control(sink));
    let spawner = zcode_host_services::testing::spawner();
    let store = SettingStore::with_change_sink(
        &spawner,
        home.path(),
        settings_control::change_sink(control.clone()),
    );
    let mut router = ControlRouter::default();
    settings_control::register_setting_writes(&spawner, &mut router, store.clone(), control);
    Setup {
        _home: home,
        store,
        router,
        messages,
    }
}

impl Setup {
    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.messages.recv())
            .await
            .expect("control message within 2s")
            .unwrap()
    }

    fn write(&self, request_id: &str, patch: Value) {
        assert!(self.router.dispatch(
            settings_control::SETTING_WRITE,
            json!({"type": "setting-write", "requestId": request_id, "patch": patch}),
        ));
    }
}

#[tokio::test]
async fn setting_write_answers_before_the_change_it_caused_in_write_order() {
    let mut setup = setup();
    setup.write("w1", json!({"desktopZoomLevel": 2}));
    setup.write("w2", json!({"desktopZoomLevel": 3, "desktopWindowSize": {"width": 900, "height": 700, "maximized": false}}));

    let first = setup.next().await;
    assert_eq!(
        first,
        json!({"type": "setting-write-result", "requestId": "w1", "ok": true})
    );
    let changed = setup.next().await;
    assert_eq!(changed["type"], "setting-changed");
    assert_eq!(changed["setting"]["desktopZoomLevel"], 2);
    let second = setup.next().await;
    assert_eq!(
        second,
        json!({"type": "setting-write-result", "requestId": "w2", "ok": true})
    );
    let changed = setup.next().await;
    assert_eq!(changed["type"], "setting-changed");
    // 完整设置：与 `setting` channel 的 `get` 返回值相同。
    assert_eq!(changed["setting"], setup.store.get().await);
    assert_eq!(changed["setting"]["desktopZoomLevel"], 3);

    // 落盘后才回 ok：文件里已经是新值。
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(setup.store.file()).unwrap()).unwrap();
    assert_eq!(saved["desktopZoomLevel"], 3);
}

#[tokio::test]
async fn invalid_setting_writes_fail_without_a_change() {
    let mut setup = setup();
    setup.write("bad", json!({"desktopZoomLevel": 99}));
    let result = setup.next().await;
    assert_eq!(result["requestId"], "bad");
    assert_eq!(result["ok"], false);
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    setup.write("missing", Value::Null);
    let result = setup.next().await;
    assert_eq!(result["requestId"], "missing");
    assert_eq!(result["ok"], false);
    // 没有 requestId 的请求无法应答：忽略。
    assert!(setup.router.dispatch(
        settings_control::SETTING_WRITE,
        json!({"type": "setting-write", "patch": {"desktopZoomLevel": 1}}),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        setup.messages.try_recv().is_err(),
        "no setting-changed for failed writes"
    );
}

#[tokio::test]
async fn renderer_and_host_internal_writes_also_publish_setting_changed() {
    let mut setup = setup();
    let channel = settings::service(setup.store.clone()).build(None);
    let renderer = CallContext::new(1, "a1", ClientMode::DesktopContinuous);
    channel
        .call(
            &renderer,
            "update",
            vec![RpcValue::Json(json!({"localePreference": "en-US"}))],
        )
        .await
        .unwrap();
    let changed = setup.next().await;
    assert_eq!(changed["type"], "setting-changed");
    assert_eq!(changed["setting"]["localePreference"], "en-US");

    // Host 内部写入方（例如数据目录迁移、settings-sync）经同一个 store。
    let patch = settings::schema::parse_patch(&json!({"recentProjects": ["/p"]})).unwrap();
    setup.store.update(&patch, None).await.unwrap();
    let changed = setup.next().await;
    assert_eq!(changed["setting"]["recentProjects"], json!(["/p"]));
    assert_eq!(changed["setting"]["localePreference"], "en-US");
}
