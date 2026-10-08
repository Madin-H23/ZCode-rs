//! 位置参数解码。`ProxyChannel.toService` 把调用参数按数组传过来，TS 侧省略的尾部可选参数
//! 在数组里根本不存在，`undefined` 则解成 `null`；两者都按“未提供”处理。
use serde::de::DeserializeOwned;
use serde_json::Value;
use zcode_host_rpc::{RpcError, RpcValue};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args(Vec<Value>);

impl Args {
    pub fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    pub fn from_rpc(values: Vec<RpcValue>) -> Self {
        Self(values.into_iter().map(RpcValue::into_json).collect())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn raw(&self, index: usize) -> Option<&Value> {
        self.0.get(index).filter(|value| !value.is_null())
    }

    /// 必填参数；缺失或类型不符时回与 zod 校验失败同类的错误。
    pub fn get<T: DeserializeOwned>(&self, index: usize, name: &str) -> Result<T, RpcError> {
        let value = self
            .raw(index)
            .ok_or_else(|| invalid_argument(name, "is required"))?;
        serde_json::from_value(value.clone())
            .map_err(|error| invalid_argument(name, &error.to_string()))
    }

    pub fn opt<T: DeserializeOwned>(
        &self,
        index: usize,
        name: &str,
    ) -> Result<Option<T>, RpcError> {
        match self.raw(index) {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| invalid_argument(name, &error.to_string())),
        }
    }
}

pub fn invalid_argument(name: &str, detail: &str) -> RpcError {
    RpcError::new("Error", format!("invalid argument {name}: {detail}"))
        .with_code("fault.invalidArgument")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_and_null_are_absent() {
        let args = Args::new(vec![json!("a"), Value::Null]);
        assert_eq!(args.get::<String>(0, "path").unwrap(), "a");
        assert_eq!(args.opt::<String>(1, "x").unwrap(), None);
        assert_eq!(args.opt::<String>(5, "y").unwrap(), None);
        assert!(args.get::<String>(1, "x").is_err());
        assert!(args.get::<u32>(0, "n").is_err());
    }
}
