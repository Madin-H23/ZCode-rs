use super::Sink;
use crate::domain::{MAX_REQUEST_BYTES, protocol::Request};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::io::BufRead;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::contract::HostError;
pub use crate::contract::Input;

/// Start stdin framing: one NDJSON message per line, bounded by `MAX_REQUEST_BYTES`.
pub fn start(input_closed: CancellationToken) -> mpsc::Receiver<Input> {
    let (in_tx, in_rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut line = Vec::new();
        while let Ok(available) = reader.fill_buf() {
            if available.is_empty() {
                if !line.is_empty() {
                    dispatch(&in_tx, &line);
                }
                break;
            }
            let take = available
                .iter()
                .position(|b| *b == b'\n')
                .map_or(available.len(), |i| i + 1);
            if line.len() + take > MAX_REQUEST_BYTES {
                let _ = in_tx.blocking_send(Input::TooLarge);
                break;
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if line.last() == Some(&b'\n') {
                if !dispatch(&in_tx, &line) {
                    return;
                }
                line.clear();
            }
        }
        input_closed.cancel();
        let _ = in_tx.blocking_send(Input::Eof);
    });
    in_rx
}
fn dispatch(tx: &mpsc::Sender<Input>, line: &[u8]) -> bool {
    match parse(line) {
        Some(input) => tx.blocking_send(input).is_ok(),
        None => true,
    }
}

/// One NDJSON line as an [`Input`]; blank lines carry nothing.
fn parse(line: &[u8]) -> Option<Input> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return Some(Input::Invalid);
    };
    if value.get("method").is_none()
        && let Some(id) = value["id"].as_str()
        && (value.get("result").is_some() ^ value.get("error").is_some())
    {
        // Host 的 error 应答必须原样交给等待方：以前它被当成 null 结果，Host 给出的错误原因
        // （如鉴权失败的具体说明）随之丢失，等待方只能报通用失败。
        let result = match value.get("error") {
            Some(error) => Err(HostError::from_error_object(error)),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        return Some(Input::Response {
            id: id.into(),
            result,
        });
    }
    let request = {
        if value["method"] == "startup/storagePathReady" {
            serde_json::from_value(
                json!({"method":"startup/storagePathReady","params":{"reuse":value["reuse"]}}),
            )
        } else {
            serde_json::from_value::<Request>(value)
        }
    };
    Some(match request {
        Ok(r) if !r.method.trim().is_empty() => Input::Request(r),
        _ => Input::Invalid,
    })
}

/// Same framing as [`start`] over any async reader (an in-memory pipe when the
/// runtime is embedded, spec rust-u1-runtime-library §3). The reader is a task
/// of `tasks`: it stops at EOF, when the receiver is dropped, or with the scope.
pub fn start_reader<R>(
    mut reader: R,
    input_closed: CancellationToken,
    tasks: &zcode_async::Spawner,
) -> mpsc::Receiver<Input>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let (in_tx, in_rx) = mpsc::channel(64);
    tasks.spawn("stdio-reader", async move {
        let mut line = Vec::new();
        while let Ok(available) = reader.fill_buf().await {
            if available.is_empty() {
                if let Some(input) = parse(&line) {
                    let _ = in_tx.send(input).await;
                }
                break;
            }
            let take = available
                .iter()
                .position(|b| *b == b'\n')
                .map_or(available.len(), |i| i + 1);
            if line.len() + take > MAX_REQUEST_BYTES {
                let _ = in_tx.send(Input::TooLarge).await;
                break;
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if line.last() == Some(&b'\n') {
                if let Some(input) = parse(&line)
                    && in_tx.send(input).await.is_err()
                {
                    return;
                }
                line.clear();
            }
        }
        input_closed.cancel();
        let _ = in_tx.send(Input::Eof).await;
    });
    in_rx
}

pub async fn finish(writer: std::thread::JoinHandle<()>) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !writer.is_finished() {
        if tokio::time::Instant::now() >= deadline {
            bail!("Protocol output drain timed out");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("Protocol writer failed"))
}

/// [`finish`] for the writer task of [`Sink::writer`]; a writer that does not
/// finish in time is aborted with its handle.
pub async fn finish_task(writer: zcode_async::OwnedTask<()>) -> Result<()> {
    match tokio::time::timeout(std::time::Duration::from_secs(2), writer).await {
        Ok(joined) => joined.map_err(|_| anyhow::anyhow!("Protocol writer failed")),
        Err(_) => bail!("Protocol output drain timed out"),
    }
}

pub async fn storage_prepare(
    path: &std::path::Path,
    input: &mut mpsc::Receiver<Input>,
    output: &mut Sink,
) -> Result<()> {
    output
        .send_values(vec![
            json!({"method":"startup/storagePath","params":{"path":path}}),
        ])
        .await?;
    match tokio::time::timeout(std::time::Duration::from_secs(30), input.recv()).await? {
        Some(Input::Request(request)) if request.method == "startup/storagePathReady" => Ok(()),
        _ => bail!("Storage preparation handshake failed"),
    }
}

#[cfg(test)]
#[path = "stdio_tests.rs"]
mod tests;
