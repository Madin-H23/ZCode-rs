//! Host 自身的资源样本（packages/desktop/src/host/hostSelfResourceTelemetry.ts）。
//!
//! 每 60 秒采样一次 CPU 占用（相对上一次的 user+system 时间增量，按逻辑核数归一）与常驻内存，
//! 经控制通道发 `host-resource-sample` 给 Main；上报端点由 Main 决定（测试里端点为空）。
//! Rust Host 没有 JS 堆，`heapUsedKb` 固定为 0。任何一步失败只丢当前样本。
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use zcode_async::Spawner;
use zcode_host_control::{ControlSink, HostToMain};

const INTERVAL: Duration = Duration::from_secs(60);

/// 进程累计的 user+system CPU 时间（微秒）。
#[cfg(unix)]
fn cpu_time_us() -> Option<u64> {
    #[allow(unsafe_code)]
    // SAFETY: getrusage 只写入调用方提供的结构体；返回非 0 时不读取。
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
    Some(micros(usage.ru_utime) + micros(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu_time_us() -> Option<u64> {
    None
}

/// 当前常驻内存（字节）。
#[cfg(target_os = "macos")]
fn rss_bytes() -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    #[allow(unsafe_code)]
    // SAFETY: proc_pidinfo 最多写入 `size` 字节到 info；返回值不等于 size 时视为失败。
    let info = unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let written = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        );
        if written != size {
            return None;
        }
        info
    };
    Some(info.pti_resident_size)
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    #[allow(unsafe_code)]
    // SAFETY: sysconf 无副作用。
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| pages * page as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rss_bytes() -> Option<u64> {
    None
}

fn round_metric(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// 由两次读数算出样本（`createNodeSelfResourceSampler.sample`）。
fn sample(
    previous: (u64, Instant),
    next: (u64, Instant),
    rss: u64,
    cpus: usize,
) -> Option<serde_json::Value> {
    let elapsed_us = next.1.checked_duration_since(previous.1)?.as_micros() as f64;
    let cpu_delta = next.0.checked_sub(previous.0)? as f64;
    if elapsed_us <= 0.0 {
        return None;
    }
    let cores = cpu_delta / elapsed_us;
    Some(json!({
        "cpuPercent": round_metric(cores / cpus as f64 * 100.0),
        "rssKb": (rss as f64 / 1024.0).round() as u64,
        "heapUsedKb": 0,
    }))
}

/// 采样是 `spawner` 作用域里的常驻任务 `host-resource-samples`。
pub fn start_host_resource_samples(spawner: &Spawner, control: Arc<dyn ControlSink>) {
    let cpus = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 4096);
    drop(spawner.spawn_resident("host-resource-samples", async move {
        let mut baseline = cpu_time_us().map(|cpu| (cpu, Instant::now()));
        loop {
            tokio::time::sleep(INTERVAL).await;
            let next = cpu_time_us().map(|cpu| (cpu, Instant::now()));
            let previous = std::mem::replace(&mut baseline, next);
            let (Some(previous), Some(next), Some(rss)) = (previous, next, rss_bytes()) else {
                continue;
            };
            if let Some(sample) = sample(previous, next, rss, cpus) {
                control.send(HostToMain::Raw(
                    json!({"type": "host-resource-sample", "sample": sample}),
                ));
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_matches_node_formula() {
        let start = Instant::now();
        let later = start + Duration::from_secs(60);
        let value = sample((0, start), (30_000_000, later), 2048 * 1024, 4).unwrap();
        assert_eq!(
            value,
            json!({"cpuPercent": 12.5, "rssKb": 2048, "heapUsedKb": 0})
        );
        assert!(sample((5, start), (1, later), 0, 1).is_none());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            assert!(rss_bytes().unwrap() > 0);
            assert!(cpu_time_us().is_some());
        }
    }
}
