//! `system.probeIntranet`（systemService.ts）：TCP connect 与 HTTP(S) 服务探测，逐条移植
//! 目标归一化、重试、阈值与错误文案。HTTP 与 Node `fetch` 一样不走 HTTP(S)_PROXY。
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

const DEFAULT_TIMEOUT_MS: u64 = 800;
const DEFAULT_ATTEMPTS: u64 = 2;
const MAX_ATTEMPTS: u64 = 3;
const DEFAULT_PORT: u16 = 22;

#[derive(Debug, Clone, PartialEq)]
enum Target {
    Tcp {
        id: String,
        host: String,
        port: u16,
        timeout_ms: u64,
    },
    Service {
        id: String,
        url: String,
        expected_marker: Option<String>,
        token: Option<String>,
        timeout_ms: u64,
    },
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn trimmed(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn normalize_timeout(value: &Value) -> u64 {
    finite(value)
        .map(|ms| ms.floor().clamp(100.0, 10_000.0) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn normalize_target(target: &Value) -> Option<Target> {
    let timeout_ms = normalize_timeout(&target["timeoutMs"]);
    if target["kind"] == "service" {
        let parsed = url::Url::parse(target["url"].as_str()?.trim()).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let url = parsed.to_string();
        return Some(Target::Service {
            id: trimmed(&target["id"]).unwrap_or_else(|| url.clone()),
            url,
            expected_marker: trimmed(&target["expectedMarker"]),
            token: trimmed(&target["token"]),
            timeout_ms,
        });
    }
    let host = target["host"].as_str()?.trim().to_owned();
    if host.is_empty() {
        return None;
    }
    let port = target["port"]
        .as_f64()
        .filter(|port| port.fract() == 0.0 && (1.0..=65535.0).contains(port))
        .map(|port| port as u16)
        .unwrap_or(DEFAULT_PORT);
    Some(Target::Tcp {
        id: trimmed(&target["id"]).unwrap_or_else(|| format!("{host}:{port}")),
        host,
        port,
        timeout_ms,
    })
}

/// Node `net.Socket` 风格的错误文案（`connect ECONNREFUSED 127.0.0.1:22`）。
fn connect_error(error: &std::io::Error, host: &str, port: u16) -> String {
    use std::io::ErrorKind;
    let code = match error.kind() {
        ErrorKind::ConnectionRefused => "ECONNREFUSED",
        ErrorKind::ConnectionReset => "ECONNRESET",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::HostUnreachable => "EHOSTUNREACH",
        ErrorKind::NetworkUnreachable => "ENETUNREACH",
        _ => return error.to_string(),
    };
    format!("connect {code} {host}:{port}")
}

async fn tcp_probe(host: &str, port: u16, timeout_ms: u64) -> Result<u64, String> {
    let started = Instant::now();
    let connect = tokio::net::TcpStream::connect((host, port));
    match tokio::time::timeout(Duration::from_millis(timeout_ms), connect).await {
        Err(_) => Err(format!("timeout({timeout_ms}ms)")),
        Ok(Err(error))
            if error.to_string().contains("lookup")
                || error.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(format!("getaddrinfo ENOTFOUND {host}"))
        }
        Ok(Err(error)) => Err(connect_error(&error, host, port)),
        Ok(Ok(_stream)) => Ok(started.elapsed().as_millis() as u64),
    }
}

/// `parseProbeServiceResponse`：返回 marker。
fn parse_service_response(payload: &Value) -> Result<Option<String>, String> {
    let Some(object) = payload.as_object() else {
        return Err("invalid service response".into());
    };
    let Some(ok) = object.get("ok").and_then(Value::as_bool) else {
        return Err("invalid service response: missing ok".into());
    };
    let marker = match object.get("marker") {
        None => None,
        Some(Value::String(marker)) => Some(marker.clone()),
        Some(_) => return Err("invalid service response: marker must be string".into()),
    };
    if !ok {
        return Err("service returned ok=false".into());
    }
    Ok(marker)
}

async fn service_probe(
    url: &str,
    token: Option<&str>,
    timeout_ms: u64,
) -> Result<(u64, Option<String>), String> {
    let started = Instant::now();
    // rustls-no-provider 不自动选择算法；与 CLI 一样统一用 ring。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| error.to_string())?;
    let request = async {
        let mut builder = client.get(url);
        if let Some(token) = token {
            builder = builder.header("x-zcode-intranet-token", token);
        }
        // Node fetch 的网络错误统一是 `fetch failed`。
        let response = builder
            .send()
            .await
            .map_err(|_| "fetch failed".to_owned())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| "fetch failed".to_owned())?;
        let payload: Value =
            serde_json::from_slice(&body).map_err(|error| format!("invalid JSON: {error}"))?;
        parse_service_response(&payload)
    };
    match tokio::time::timeout(Duration::from_millis(timeout_ms), request).await {
        Err(_) => Err(format!("timeout({timeout_ms}ms)")),
        Ok(Err(error)) => Err(error),
        Ok(Ok(marker)) => Ok((started.elapsed().as_millis() as u64, marker)),
    }
}

async fn probe_target(target: &Target, attempts: u64) -> Value {
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        match target {
            Target::Tcp {
                id,
                host,
                port,
                timeout_ms,
            } => match tcp_probe(host, *port, *timeout_ms).await {
                Ok(latency) => {
                    return json!({"targetId": id, "kind": "tcp", "host": host, "port": port,
                                  "reachable": true, "attemptCount": attempt, "latencyMs": latency});
                }
                Err(error) => last_error = error,
            },
            Target::Service {
                id,
                url,
                expected_marker,
                token,
                timeout_ms,
            } => {
                let outcome = service_probe(url, token.as_deref(), *timeout_ms)
                    .await
                    .and_then(|(latency, marker)| match expected_marker {
                        Some(expected) if marker.as_deref() != Some(expected.as_str()) => {
                            Err(format!(
                                "marker mismatch(expected={expected}, actual={})",
                                marker.as_deref().unwrap_or("<empty>")
                            ))
                        }
                        _ => Ok((latency, marker)),
                    });
                match outcome {
                    Ok((latency, marker)) => {
                        let mut result = json!({"targetId": id, "kind": "service", "url": url,
                            "reachable": true, "attemptCount": attempt, "latencyMs": latency});
                        if let Some(marker) = marker {
                            result["marker"] = json!(marker);
                        }
                        return result;
                    }
                    Err(error) => last_error = error,
                }
            }
        }
    }
    let error = if last_error.is_empty() {
        "probe failed".to_owned()
    } else {
        last_error
    };
    let mut result = Map::new();
    match target {
        Target::Tcp { id, host, port, .. } => {
            result.insert("targetId".into(), json!(id));
            result.insert("kind".into(), json!("tcp"));
            result.insert("host".into(), json!(host));
            result.insert("port".into(), json!(port));
        }
        Target::Service { id, url, .. } => {
            result.insert("targetId".into(), json!(id));
            result.insert("kind".into(), json!("service"));
            result.insert("url".into(), json!(url));
        }
    }
    result.insert("reachable".into(), json!(false));
    result.insert("attemptCount".into(), json!(attempts));
    result.insert("latencyMs".into(), Value::Null);
    result.insert("error".into(), json!(error));
    Value::Object(result)
}

pub async fn probe(request: &Value) -> Value {
    let targets: Vec<Target> = request["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(normalize_target)
        .collect();
    let attempts = finite(&request["attempts"])
        .map(|value| value.floor().clamp(1.0, MAX_ATTEMPTS as f64) as u64)
        .unwrap_or(DEFAULT_ATTEMPTS);
    let total = targets.len();
    let required = match finite(&request["requiredSuccessCount"]) {
        Some(value) if total > 0 => value.floor().clamp(1.0, total as f64) as usize,
        _ => 1,
    };
    let results =
        futures_util::future::join_all(targets.iter().map(|target| probe_target(target, attempts)))
            .await;
    let reached = results
        .iter()
        .filter(|result| result["reachable"] == true)
        .count();
    let all_tcp = targets
        .iter()
        .all(|target| matches!(target, Target::Tcp { .. }));
    let all_service = targets
        .iter()
        .all(|target| matches!(target, Target::Service { .. }));
    let strategy = if all_tcp {
        "tcp-connect"
    } else if all_service {
        "service-http"
    } else {
        "mixed"
    };
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    json!({
        "isIntranet": total > 0 && reached >= required,
        "reachedTargetCount": reached,
        "requiredSuccessCount": required,
        "totalTargets": total,
        "checkedAt": checked_at,
        "strategy": strategy,
        "results": results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 127.0.0.1 上的一次性 HTTP 服务：每个连接回同一份 body。
    async fn http_server(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_lowercase();
                let body = if request.contains("x-zcode-intranet-token: secret") {
                    body.replace("TOKEN", "seen")
                } else {
                    body.to_owned()
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{address}/probe")
    }

    #[test]
    fn normalizes_targets_like_node() {
        let tcp = normalize_target(&json!({"host": " 10.0.0.1 ", "port": 70000, "timeoutMs": 5}))
            .unwrap();
        assert_eq!(
            tcp,
            Target::Tcp {
                id: "10.0.0.1:22".into(),
                host: "10.0.0.1".into(),
                port: 22,
                timeout_ms: 100
            }
        );
        let service = normalize_target(
            &json!({"kind": "service", "url": "http://a.example", "timeoutMs": 99999}),
        )
        .unwrap();
        assert!(
            matches!(service, Target::Service { ref url, timeout_ms: 10_000, .. } if url == "http://a.example/")
        );
        assert!(normalize_target(&json!({"kind": "service", "url": "ftp://x"})).is_none());
        assert!(normalize_target(&json!({"host": "  "})).is_none());
    }

    #[tokio::test]
    async fn probes_tcp_and_service_targets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_port = listener.local_addr().unwrap().port();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let ok = http_server("200 OK", r#"{"ok":true,"marker":"TOKEN"}"#).await;
        let bad = http_server("503 Service Unavailable", "{}").await;
        let result = probe(&json!({
            "attempts": 1,
            "requiredSuccessCount": 2,
            "targets": [
                {"host": "127.0.0.1", "port": open_port},
                {"host": "127.0.0.1", "port": closed_port},
                {"kind": "service", "url": ok, "token": "secret", "expectedMarker": "seen"},
                {"kind": "service", "url": bad},
            ],
        }))
        .await;
        assert_eq!(result["strategy"], "mixed");
        assert_eq!(result["totalTargets"], 4);
        assert_eq!(result["reachedTargetCount"], 2);
        assert_eq!(result["isIntranet"], true);
        let results = result["results"].as_array().unwrap();
        assert_eq!(results[0]["reachable"], true);
        assert_eq!(results[1]["reachable"], false);
        assert_eq!(
            results[1]["error"],
            format!("connect ECONNREFUSED 127.0.0.1:{closed_port}")
        );
        assert_eq!(results[2]["marker"], "seen");
        assert_eq!(results[3]["error"], "HTTP 503");
        assert_eq!(results[3]["latencyMs"], Value::Null);
        drop(listener);
    }

    #[tokio::test]
    async fn empty_targets_are_not_intranet() {
        let result = probe(&json!({"targets": []})).await;
        assert_eq!(result["isIntranet"], false);
        assert_eq!(result["requiredSuccessCount"], 1);
        assert_eq!(result["strategy"], "tcp-connect");
    }
}
