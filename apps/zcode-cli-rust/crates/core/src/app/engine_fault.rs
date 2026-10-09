//! 调试构建专用的故障注入（docs/specs/rust-u3-single-process.md §2.5）：让测试在 Engine 循环里
//! 触发一次 panic，验证实例崩溃后的收口与嵌入方的重启。发布构建不编译这个模块。
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Fault(Option<String>);

impl Fault {
    pub(crate) fn new(marker: Option<String>) -> Self {
        Self(marker.filter(|marker| !marker.is_empty()))
    }

    /// 请求参数里出现标记时 panic。
    pub(crate) fn check(&self, params: &Value) {
        if let Some(marker) = &self.0
            && params.to_string().contains(marker.as_str())
        {
            panic!("fault injection: the request contains the marker");
        }
    }
}
