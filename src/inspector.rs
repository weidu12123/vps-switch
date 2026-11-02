use crate::{
    db::Database,
    models::{OsInfo, LOCAL_HOST_ID},
    ssh::{exec_short_command, SshPool},
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageItem {
    pub mount: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInspection {
    pub host_id: String,
    pub sampled_at: String,
    pub uptime_seconds: u64,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub tcp_cc: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_cores: u32,
    pub cpu_usage_pct: f64,
    pub cpu_iowait_pct: f64,
    pub cpu_steal_pct: f64,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
    pub mem_cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub disk_mount: String,
    pub disk_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disks: Vec<DiskUsageItem>,
    pub net_iface: String,
    pub net_rx_bytes_total: u64,
    pub net_tx_bytes_total: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

#[derive(Clone, Copy)]
struct CpuTicks {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTicks {
    fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    fn idle_all(&self) -> u64 {
        self.idle + self.iowait
    }
}

struct PrevHostSample {
    at: Instant,
    cpu: CpuTicks,
    net_iface: String,
    net_rx: u64,
    net_tx: u64,
    last_cpu_pct: f64,
    last_iowait_pct: f64,
    last_steal_pct: f64,
}

#[derive(Default)]
pub struct InspectorEngine {
    prev_samples: Mutex<HashMap<String, PrevHostSample>>,
    latest_snapshots: Mutex<HashMap<String, HostInspection>>,
    last_wave_at: Mutex<Option<Instant>>,
}

/// 纯内核虚拟文件读取脚本：
/// 1. 开头先用纯 shell builtin `read < /proc/stat` 取样 CPU1，在读取其他内核文件后再取 CPU2，
///    保证即使首次点开也能通过真实 `/proc/stat` 差分算出准确 CPU%，绝不用容器里失真的 loadavg 估算；
/// 2. 磁盘统一使用 POSIX 标准 `df -Pk`（1024-byte blocks，兼容 Alpine BusyBox 与所有发行版）。
const LINUX_INSPECT_SCRIPT: &str = r#"export LC_ALL=C
read -r _ u1 n1 s1 i1 w1 q1 sq1 st1 _ < /proc/stat 2>/dev/null
CPU1="$u1 $n1 $s1 $i1 $w1 $q1 $sq1 $st1"
read -r UP _ < /proc/uptime 2>/dev/null || UP=0
read -r L1 L5 L15 _ < /proc/loadavg 2>/dev/null || { L1=0; L5=0; L15=0; }
CC=$(cat /proc/sys/net/ipv4/tcp_congestion_control 2>/dev/null || echo "")
IFACE=$(awk '$2 == "00000000" {print $1; exit}' /proc/net/route 2>/dev/null)
if [ -z "$IFACE" ]; then
  IFACE=$(awk -F: 'NR>2 {gsub(/^[ \t]+/,"",$1); if ($1 !~ /^(lo|docker|veth|br-|tun|tap|wg|tailscale|flannel|cni|virbr)/) {print $1; exit}}' /proc/net/dev 2>/dev/null)
fi
NET=$(awk -F'[: ]+' -v dev="$IFACE" '$0 ~ "^[ \t]*"dev":" {
  for(i=1;i<=NF;i++) if($i == dev) { print $(i+1), $(i+9); exit }
}' /proc/net/dev 2>/dev/null)
MEM=$(awk '
  /^MemTotal:/ {mt=$2}
  /^MemFree:/ {mf=$2}
  /^MemAvailable:/ {ma=$2}
  /^Buffers:/ {buf=$2}
  /^Cached:/ {ca=$2}
  /^SReclaimable:/ {sr=$2}
  /^SwapTotal:/ {st=$2}
  /^SwapFree:/ {sf=$2}
  END {
    avail = (ma > 0) ? ma : (mf + buf + ca);
    used = (mt > avail) ? (mt - avail) : 0;
    cache = buf + ca + sr;
    sw_used = (st > sf) ? (st - sf) : 0;
    printf "%.0f %.0f %.0f %.0f %.0f", mt*1024, used*1024, cache*1024, st*1024, sw_used*1024
  }
' /proc/meminfo 2>/dev/null)
DISKS=$(df -Pk 2>/dev/null | awk 'NR>1 && $1 !~ /^(tmpfs|devtmpfs|udev|none|shm|overlay)$/ && $6 !~ /^\/(dev|proc|sys|run|boot\/efi)/ {printf "%s:%.0f:%.0f|", $6, $2*1024, $3*1024}')
if [ -z "$DISKS" ]; then
  DISKS=$(df -Pk / 2>/dev/null | awk 'NR==2 {printf "%s:%.0f:%.0f|", $6, $2*1024, $3*1024}')
fi
MODEL=$(awk -F: '/model name|Model name/ {gsub(/^[ \t]+/,"",$2); print $2; exit}' /proc/cpuinfo 2>/dev/null)
CORES=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null || echo 1)
if [ -f /etc/os-release ]; then . /etc/os-release; fi
UNAME_R=$(uname -r 2>/dev/null)
UNAME_M=$(uname -m 2>/dev/null)
sleep 0.12 2>/dev/null || true
read -r _ u2 n2 s2 i2 w2 q2 sq2 st2 _ < /proc/stat 2>/dev/null
CPU2="$u2 $n2 $s2 $i2 $w2 $q2 $sq2 $st2"
printf "UP=%s\nLOAD=%s %s %s\nCC=%s\nCPU1=%s\nCPU2=%s\nIFACE=%s\nNET=%s\nMEM=%s\nDISKS=%s\nMODEL=%s\nCORES=%s\nOS_ID=%s\nOS_VER=%s\nOS_PRETTY=%s\nKERNEL=%s\nARCH=%s\n" \
  "$UP" "$L1" "$L5" "$L15" "$CC" "$CPU1" "$CPU2" "$IFACE" "$NET" "$MEM" "$DISKS" "$MODEL" "$CORES" "${ID:-linux}" "${VERSION_ID:-}" "${PRETTY_NAME:-Linux}" "$UNAME_R" "$UNAME_M"
"#;

/// Windows 采集脚本：本机与远端共用一份（远端经 `-EncodedCommand` 送达）。
/// 顺带带回 Caption / Version / PROCESSOR_ARCHITECTURE，让仓库里的系统信息是真读出来的。
const WINDOWS_INSPECT_SCRIPT: &str = r#"$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$disks = Get-CimInstance Win32_LogicalDisk -Filter "DriveType=3" | ForEach-Object { "$($_.DeviceID)/:$($_.Size):$($_.Size - $_.FreeSpace)" }
$net = Get-NetAdapterStatistics | Where-Object { $_.ReceivedBytes -gt 0 } | Sort-Object ReceivedBytes -Descending | Select-Object -First 1
$up = [int]((Get-Date) - $os.LastBootUpTime).TotalSeconds
$mt = [uint64]$os.TotalVisibleMemorySize * 1024
$mf = [uint64]$os.FreePhysicalMemory * 1024
$mu = if ($mt -gt $mf) { $mt - $mf } else { 0 }
Write-Output "UP=$up"
Write-Output "CPU_PCT=$($cpu.LoadPercentage)"
Write-Output "CPU_NAME=$($cpu.Name)"
Write-Output "CORES=$([Environment]::ProcessorCount)"
Write-Output "MEM=$mt $mu"
Write-Output "DISKS=$($disks -join '|')"
Write-Output "NET=$($net.Name)|$($net.ReceivedBytes)|$($net.SentBytes)"
Write-Output "OS_PRETTY=$($os.Caption)"
Write-Output "OS_VER=$($os.Version)"
Write-Output "ARCH=$($env:PROCESSOR_ARCHITECTURE)"
"#;

/// 远端 Windows 探针命令：UTF-16LE + base64，绕开 cmd.exe 转义与中文乱码。
fn windows_inspect_command() -> String {
    let utf16: Vec<u8> = WINDOWS_INSPECT_SCRIPT
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&utf16);
    format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {b64}")
}

/// 两套探针脚本都以 `MEM=<总字节> <已用字节>` 开头报内存，真机上总量不可能为 0。
/// 因此"有没有拿到正数的 MEM"就是"这套脚本跑对了没有"的唯一判据，不必为每个系统写一套判定。
fn probe_output_usable(raw: &str) -> bool {
    raw.lines()
        .find_map(|l| l.trim_start().strip_prefix("MEM="))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|total| total > 0)
        .unwrap_or(false)
}

/// 失败时把远端原话带回给调用方（含 agent），不做静默吞咽。
fn summarize_probe_output(raw: &str) -> String {
    let brief: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ");
    if brief.is_empty() {
        "无任何输出".to_string()
    } else if brief.chars().count() > 240 {
        brief.chars().take(240).collect::<String>() + "…"
    } else {
        brief
    }
}

/// 连接失败里区分"暂态/未决（超时、本机代理未就绪、本机网络未就绪）"与"远端明确拒绝/认证失败"。
/// 1. 超时只说明这次没连上、可能只是线路慢（慢 VPS 的 SSH 握手常超过 8 秒）；
/// 2. 开机自启瞬间网卡尚未拨号完成，或用户配置了走本机代理（127.0.0.1:7890）但代理软件还没启动完，
///    此时连不上本机代理端口并不代表远端 VPS 离线；
/// 以上情况均记为 `unknown`（灰灯），待后续自动补检恢复，避免开机全员误判红灯。
pub fn connect_error_is_indeterminate(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    msg.contains("超时")
        || msg.contains("无法连接本机代理端口")
        || msg.contains("无法连接本机 HTTP 代理端口")
        || lower.contains("deadline has elapsed")
        || lower.contains("timed out")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("failed to lookup address")
        || lower.contains("os error 10051")
        || lower.contains("os error 10065")
        || lower.contains("wsano_data")
}

impl InspectorEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_all_cached(&self) -> HashMap<String, HostInspection> {
        self.latest_snapshots.lock().unwrap().clone()
    }

    pub fn mark_wave_now(&self) {
        *self.last_wave_at.lock().unwrap() = Some(Instant::now());
    }

    pub fn try_claim_wave(&self, min_interval: std::time::Duration) -> bool {
        let mut guard = self.last_wave_at.lock().unwrap();
        if guard.is_some_and(|at| at.elapsed() < min_interval) {
            return false;
        }
        *guard = Some(Instant::now());
        true
    }

    pub async fn inspect_host(
        &self,
        pool: &SshPool,
        db: &Database,
        host_id: &str,
    ) -> Result<HostInspection> {
        if host_id == LOCAL_HOST_ID {
            #[cfg(windows)]
            let ins = self.inspect_local_windows().await?;
            #[cfg(not(windows))]
            let ins = self.inspect_local_linux().await?;
            self.latest_snapshots
                .lock()
                .unwrap()
                .insert(host_id.to_string(), ins.clone());
            return Ok(ins);
        }

        let record = db
            .get_record(host_id)?
            .ok_or_else(|| anyhow!("主机不存在: {host_id}"))?;
        let (jump, proxy_port) = db.resolve_ssh_route(&record)?;

        let connect_started = Instant::now();
        let session = match pool
            .get_or_connect(&record, jump.as_ref(), proxy_port, db.master_key())
            .await
        {
            Ok(s) => s,
            Err(err) => {
                // 连不上要分两种情况：**超时**（不确定，可能只是线路慢）与**拒绝/认证失败**
                // （确定连不上）。前者若也判离线，慢线路（跨境高延迟那类）会长期误红，
                // 所以记成 unknown 交给界面用灰灯表示"这次没判定出来"，绝不冒充"离线"。
                let msg = format!("{err:#}");
                let status = if connect_error_is_indeterminate(&msg) {
                    "unknown"
                } else {
                    "offline"
                };
                let _ = db.update_check_result(host_id, status, None, None, Some(msg));
                return Err(err);
            }
        };
        // 「SSH 响应」只算连接（握手+认证）耗时，不含后面的排队与探针——否则探针慢会把延迟撑到几十秒。
        let latency_ms = connect_started.elapsed().as_millis() as i64;

        let known_windows = record
            .os_info
            .as_ref()
            .and_then(|o| o.os.as_deref())
            .map(|s| s.eq_ignore_ascii_case("windows"))
            .unwrap_or(false);

        // 探针同样占用一条 session channel，纳入并发闸
        let _exec_permit = pool.acquire_exec_slot(host_id).await?;

        // 探针本身就是 OS 探测器：先按仓库里记着的系统跑，输出不成形就换另一套再跑一次，
        // 探到哪套算哪套并写回 os_info。手工新建的 Windows 主机因此第一次探测就被认出来
        // （此前只有 1Shell 导入才带得进 osInfo），重装换系统的主机也会自动纠正。
        let mut parsed: Option<(HostInspection, OsInfo)> = None;
        let mut failures: Vec<String> = Vec::new();
        let mut exec_errored = false;
        for try_windows in [known_windows, !known_windows] {
            let label = if try_windows { "Windows" } else { "Linux" };
            let cmd = if try_windows {
                windows_inspect_command()
            } else {
                LINUX_INSPECT_SCRIPT.to_string()
            };
            let raw = match exec_short_command(&session, &cmd).await {
                Ok(r) => r,
                Err(err) => {
                    exec_errored = true;
                    failures.push(format!("{label} 探针执行失败: {err:#}"));
                    continue;
                }
            };
            if !probe_output_usable(&raw) {
                failures.push(format!(
                    "{label} 探针输出不可用: {}",
                    summarize_probe_output(&raw)
                ));
                continue;
            }
            parsed = Some(if try_windows {
                self.parse_windows_inspection_text(host_id, &raw)?
            } else {
                self.parse_linux_inspection(host_id, &raw)?
            });
            break;
        }

        let (inspection, os_info) = match parsed {
            Some(v) => v,
            None => {
                let detail = failures.join("；");
                // 走到这里说明 SSH **已经连上**了（get_or_connect 成功）——这台机器是可连的。
                // 探针跑不出来只代表"这次没采到数据"，不该把灯判成离线：慢/抖线路上 channel
                // 中途掉一次（例如回退的 Windows 探针返回 Disconnected）就会让一台能连的主机
                // 常年标红（真机实测复现过）。所以保持在线，把采集失败原因写进 last_error。
                if exec_errored {
                    // exec 层面出过错，池里这条连接不再可信，回收它下次重建
                    pool.invalidate_host(host_id).await;
                }
                let _ = db.update_check_result(
                    host_id,
                    "online",
                    Some(latency_ms),
                    None,
                    Some(detail.clone()),
                );
                return Err(anyhow!("探针无法识别远端系统：{detail}"));
            }
        };

        let _ = db.update_check_result(host_id, "online", Some(latency_ms), Some(os_info), None);
        self.latest_snapshots
            .lock()
            .unwrap()
            .insert(host_id.to_string(), inspection.clone());

        Ok(inspection)
    }

    fn parse_linux_inspection(
        &self,
        host_id: &str,
        raw: &str,
    ) -> Result<(HostInspection, OsInfo)> {
        let mut map = HashMap::new();
        for line in raw.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim(), v.trim());
            }
        }

        let uptime_seconds = map
            .get("UP")
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut load_parts = map
            .get("LOAD")
            .copied()
            .unwrap_or("0 0 0")
            .split_whitespace();
        let load_1 = load_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let load_5 = load_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let load_15 = load_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);

        let tcp_cc = map
            .get("CC")
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());

        let cpu1 = parse_cpu_ticks(map.get("CPU1").copied().unwrap_or(""));
        let cpu2 = parse_cpu_ticks(map.get("CPU2").copied().unwrap_or(""));

        let net_iface = map
            .get("IFACE")
            .copied()
            .filter(|s| !s.is_empty())
            .unwrap_or("eth0")
            .to_string();

        let mut net_parts = map.get("NET").copied().unwrap_or("0 0").split_whitespace();
        let net_rx = net_parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let net_tx = net_parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);

        let mut mem_parts = map
            .get("MEM")
            .copied()
            .unwrap_or("0 0 0 0 0")
            .split_whitespace();
        let mem_total_bytes = mem_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mem_used_bytes = mem_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mem_cached_bytes = mem_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let swap_total_bytes = mem_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let swap_used_bytes = mem_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        let mut disks = Vec::new();
        let mut seen_mounts = std::collections::HashSet::new();
        for item in map.get("DISKS").copied().unwrap_or("").split('|') {
            if item.trim().is_empty() {
                continue;
            }
            let mut p = item.split(':');
            if let (Some(mount), Some(tot), Some(used)) = (
                p.next(),
                p.next().and_then(|s| s.parse::<u64>().ok()),
                p.next().and_then(|s| s.parse::<u64>().ok()),
            ) {
                if tot > 0 && seen_mounts.insert(mount.to_string()) {
                    disks.push(DiskUsageItem {
                        mount: mount.to_string(),
                        total_bytes: tot,
                        used_bytes: used,
                    });
                }
            }
        }
        disks.sort_by(|a, b| {
            (a.mount != "/")
                .cmp(&(b.mount != "/"))
                .then_with(|| a.mount.cmp(&b.mount))
        });

        let primary_disk = disks.first().cloned().unwrap_or(DiskUsageItem {
            mount: "/".to_string(),
            total_bytes: 0,
            used_bytes: 0,
        });

        let cpu_model = map
            .get("MODEL")
            .copied()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let cpu_cores = map
            .get("CORES")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);

        let now_inst = Instant::now();
        let (cpu_usage_pct, cpu_iowait_pct, cpu_steal_pct, net_rx_bps, net_tx_bps) = {
            let mut guard = self.prev_samples.lock().unwrap();
            if let Some(prev) = guard.get(host_id) {
                let elapsed = now_inst.duration_since(prev.at).as_secs_f64();
                // 若距离上次采样在 15 秒内，直接用跨周期的精确硬件计数差值；否则使用本次脚本内 CPU1->CPU2 的差值
                let base_cpu = if elapsed <= 15.0 { prev.cpu } else { cpu1 };
                let (cpu_pct, iowait_pct, steal_pct) = compute_cpu_diff(&base_cpu, &cpu2)
                    .unwrap_or((prev.last_cpu_pct, prev.last_iowait_pct, prev.last_steal_pct));

                let (rx_bps, tx_bps) = if elapsed > 0.2 && elapsed <= 15.0 && prev.net_iface == net_iface {
                    let drx = net_rx.saturating_sub(prev.net_rx);
                    let dtx = net_tx.saturating_sub(prev.net_tx);
                    ((drx as f64 / elapsed) as u64, (dtx as f64 / elapsed) as u64)
                } else {
                    (0, 0)
                };

                guard.insert(
                    host_id.to_string(),
                    PrevHostSample {
                        at: now_inst,
                        cpu: cpu2,
                        net_iface: net_iface.clone(),
                        net_rx,
                        net_tx,
                        last_cpu_pct: cpu_pct,
                        last_iowait_pct: iowait_pct,
                        last_steal_pct: steal_pct,
                    },
                );

                (cpu_pct, iowait_pct, steal_pct, rx_bps, tx_bps)
            } else {
                // 首次采样：直接用脚本内 CPU1 -> CPU2 的真实 /proc/stat 差值，绝不用 loadavg 瞎猜！
                let (cpu_pct, iowait_pct, steal_pct) =
                    compute_cpu_diff(&cpu1, &cpu2).unwrap_or((0.0, 0.0, 0.0));
                guard.insert(
                    host_id.to_string(),
                    PrevHostSample {
                        at: now_inst,
                        cpu: cpu2,
                        net_iface: net_iface.clone(),
                        net_rx,
                        net_tx,
                        last_cpu_pct: cpu_pct,
                        last_iowait_pct: iowait_pct,
                        last_steal_pct: steal_pct,
                    },
                );
                (cpu_pct, iowait_pct, steal_pct, 0, 0)
            }
        };

        let now_iso = Utc::now().to_rfc3339();
        let os_info = OsInfo {
            os: Some("linux".to_string()),
            distro_id: map.get("OS_ID").map(|s| s.to_lowercase()),
            version_id: map.get("OS_VER").filter(|s| !s.is_empty()).map(|s| s.to_string()),
            pretty_name: map.get("OS_PRETTY").filter(|s| !s.is_empty()).map(|s| s.to_string()),
            arch: map.get("ARCH").filter(|s| !s.is_empty()).map(|s| s.to_string()),
            kernel: map.get("KERNEL").filter(|s| !s.is_empty()).map(|s| s.to_string()),
            cpu_model: cpu_model.clone(),
            cpu_cores: Some(cpu_cores),
            detected_at: Some(now_iso.clone()),
        };

        let inspection = HostInspection {
            host_id: host_id.to_string(),
            sampled_at: now_iso,
            uptime_seconds,
            load_1,
            load_5,
            load_15,
            tcp_cc,
            cpu_model,
            cpu_cores,
            cpu_usage_pct: (cpu_usage_pct * 10.0).round() / 10.0,
            cpu_iowait_pct: (cpu_iowait_pct * 10.0).round() / 10.0,
            cpu_steal_pct: (cpu_steal_pct * 10.0).round() / 10.0,
            mem_total_bytes,
            mem_used_bytes,
            mem_cached_bytes,
            swap_total_bytes,
            swap_used_bytes,
            disk_mount: primary_disk.mount,
            disk_total_bytes: primary_disk.total_bytes,
            disk_used_bytes: primary_disk.used_bytes,
            disks,
            net_iface,
            net_rx_bytes_total: net_rx,
            net_tx_bytes_total: net_tx,
            net_rx_bps,
            net_tx_bps,
        };

        Ok((inspection, os_info))
    }

    async fn inspect_local_windows(&self) -> Result<HostInspection> {
        let mut cmd = tokio::process::Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_INSPECT_SCRIPT,
        ]);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let out = cmd.output().await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (ins, _) = self.parse_windows_inspection_text(LOCAL_HOST_ID, &text)?;
        Ok(ins)
    }

    #[cfg(not(windows))]
    async fn inspect_local_linux(&self) -> Result<HostInspection> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", LINUX_INSPECT_SCRIPT]);
        let out = cmd.output().await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (ins, _) = self.parse_linux_inspection(LOCAL_HOST_ID, &text)?;
        Ok(ins)
    }

    fn parse_windows_inspection_text(
        &self,
        host_id: &str,
        text: &str,
    ) -> Result<(HostInspection, OsInfo)> {
        let mut map = HashMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim(), v.trim());
            }
        }

        let uptime_seconds = map.get("UP").and_then(|s| s.parse().ok()).unwrap_or(0);
        let cpu_usage_pct = map.get("CPU_PCT").and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let cpu_model = map.get("CPU_NAME").map(|s| s.to_string());
        let cpu_cores = map.get("CORES").and_then(|s| s.parse().ok()).unwrap_or(8);

        let mut mem_it = map.get("MEM").copied().unwrap_or("0 0").split_whitespace();
        let mem_total_bytes = mem_it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mem_used_bytes = mem_it.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        let mut disks = Vec::new();
        for item in map.get("DISKS").copied().unwrap_or("").split('|') {
            if item.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = item.split(':').collect();
            if parts.len() >= 4 {
                let mount = format!("{}:{}", parts[0], parts[1]);
                let tot = parts[2].parse::<u64>().unwrap_or(0);
                let used = parts[3].parse::<u64>().unwrap_or(0);
                if tot > 0 {
                    disks.push(DiskUsageItem {
                        mount,
                        total_bytes: tot,
                        used_bytes: used,
                    });
                }
            } else {
                let mut p = item.split(':');
                if let (Some(m), Some(tot), Some(used)) = (
                    p.next(),
                    p.next().and_then(|s| s.parse::<u64>().ok()),
                    p.next().and_then(|s| s.parse::<u64>().ok()),
                ) {
                    if tot > 0 {
                        disks.push(DiskUsageItem {
                            mount: m.to_string(),
                            total_bytes: tot,
                            used_bytes: used,
                        });
                    }
                }
            }
        }
        let primary_disk = disks.first().cloned().unwrap_or(DiskUsageItem {
            mount: "C:/".to_string(),
            total_bytes: 0,
            used_bytes: 0,
        });

        let mut net_it = map.get("NET").copied().unwrap_or("Ethernet|0|0").split('|');
        let net_iface = net_it.next().unwrap_or("Ethernet").to_string();
        let net_rx: u64 = net_it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let net_tx: u64 = net_it.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        let now_inst = Instant::now();
        let (net_rx_bps, net_tx_bps) = {
            let mut guard = self.prev_samples.lock().unwrap();
            let rates = if let Some(prev) = guard.get(host_id) {
                let elapsed = now_inst.duration_since(prev.at).as_secs_f64();
                if elapsed > 0.2 {
                    (
                        (net_rx.saturating_sub(prev.net_rx) as f64 / elapsed) as u64,
                        (net_tx.saturating_sub(prev.net_tx) as f64 / elapsed) as u64,
                    )
                } else {
                    (0, 0)
                }
            } else {
                (0, 0)
            };
            guard.insert(
                host_id.to_string(),
                PrevHostSample {
                    at: now_inst,
                    cpu: CpuTicks {
                        user: 0,
                        nice: 0,
                        system: 0,
                        idle: 0,
                        iowait: 0,
                        irq: 0,
                        softirq: 0,
                        steal: 0,
                    },
                    net_iface: net_iface.clone(),
                    net_rx,
                    net_tx,
                    last_cpu_pct: cpu_usage_pct,
                    last_iowait_pct: 0.0,
                    last_steal_pct: 0.0,
                },
            );
            rates
        };

        let inspection = HostInspection {
            host_id: host_id.to_string(),
            sampled_at: Utc::now().to_rfc3339(),
            uptime_seconds,
            load_1: 0.0,
            load_5: 0.0,
            load_15: 0.0,
            tcp_cc: None,
            cpu_model: cpu_model.clone(),
            cpu_cores,
            cpu_usage_pct,
            cpu_iowait_pct: 0.0,
            cpu_steal_pct: 0.0,
            mem_total_bytes,
            mem_used_bytes,
            mem_cached_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            disk_mount: primary_disk.mount,
            disk_total_bytes: primary_disk.total_bytes,
            disk_used_bytes: primary_disk.used_bytes,
            disks,
            net_iface,
            net_rx_bytes_total: net_rx,
            net_tx_bytes_total: net_tx,
            net_rx_bps,
            net_tx_bps,
        };

        let os_info = OsInfo {
            os: Some("windows".to_string()),
            distro_id: Some("windows".to_string()),
            version_id: map
                .get("OS_VER")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            pretty_name: Some(
                map.get("OS_PRETTY")
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Windows".to_string()),
            ),
            kernel: map
                .get("OS_VER")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            arch: map
                .get("ARCH")
                .filter(|s| !s.is_empty())
                .map(|s| match s.to_ascii_uppercase().as_str() {
                    "AMD64" => "x86_64".to_string(),
                    "ARM64" => "aarch64".to_string(),
                    "X86" => "i686".to_string(),
                    other => other.to_ascii_lowercase(),
                }),
            cpu_model,
            cpu_cores: Some(cpu_cores as u32),
            detected_at: Some(Utc::now().to_rfc3339()),
        };

        Ok((inspection, os_info))
    }
}

fn compute_cpu_diff(a: &CpuTicks, b: &CpuTicks) -> Option<(f64, f64, f64)> {
    let total_delta = b.total().saturating_sub(a.total());
    if total_delta == 0 {
        return None;
    }
    let idle_delta = b.idle_all().saturating_sub(a.idle_all());
    let busy_delta = total_delta.saturating_sub(idle_delta);
    let io_delta = b.iowait.saturating_sub(a.iowait);
    let st_delta = b.steal.saturating_sub(a.steal);
    Some((
        ((busy_delta as f64 / total_delta as f64) * 100.0).clamp(0.0, 100.0),
        ((io_delta as f64 / total_delta as f64) * 100.0).clamp(0.0, 100.0),
        ((st_delta as f64 / total_delta as f64) * 100.0).clamp(0.0, 100.0),
    ))
}

fn parse_cpu_ticks(s: &str) -> CpuTicks {
    let mut it = s.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
    CpuTicks {
        user: it.next().unwrap_or(0),
        nice: it.next().unwrap_or(0),
        system: it.next().unwrap_or(0),
        idle: it.next().unwrap_or(0),
        iowait: it.next().unwrap_or(0),
        irq: it.next().unwrap_or(0),
        softirq: it.next().unwrap_or(0),
        steal: it.next().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn classifies_boot_and_proxy_errors_as_indeterminate() {
        assert!(connect_error_is_indeterminate(
            "无法连接本机代理端口 127.0.0.1:7890（请确认代理软件已启动）: 由于目标计算机积极拒绝，无法连接。 (os error 10061)"
        ));
        assert!(connect_error_is_indeterminate(
            "SSH 连接超时: deadline has elapsed"
        ));
        assert!(connect_error_is_indeterminate(
            "SSH 连接失败: Network is unreachable (os error 10051)"
        ));
        assert!(!connect_error_is_indeterminate(
            "SSH 连接失败: Connection refused (os error 10061)"
        ));
        assert!(!connect_error_is_indeterminate("SSH 密码认证失败"));
    }

    #[test]
    fn wave_cooldown_throttles_duplicate_triggers() {
        let engine = InspectorEngine::new();
        assert!(engine.try_claim_wave(Duration::from_secs(10)));
        assert!(!engine.try_claim_wave(Duration::from_secs(10)));
        assert!(engine.try_claim_wave(Duration::ZERO));
    }
}
