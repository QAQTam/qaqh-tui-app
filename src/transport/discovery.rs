//! daemon 发现（镜像 `qaqh-client/src/discovery.rs` 的行为）。
//!
//! `daemon.json` 是 daemon 写入的运行时发现记录（含 Bearer token），
//! 位于 data_dir；Windows 下 ACL 限定当前用户。token 仅内存持有。

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// 镜像 `qaqh-proto/src/control.rs::DaemonDiscovery`（字段随 wire 全量镜像）。
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonDiscovery {
    /// `http://<host>:<port>`（legacy `ws://host:port/control/v1` 仍可解析）。
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub server_epoch: String,
    pub protocol_version: u16,
    #[serde(default)]
    pub daemon_version: String,
    #[serde(default)]
    pub build_id: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub executable: String,
}

impl DaemonDiscovery {
    /// 与 `DaemonDiscovery::base_url` 一致：去掉 legacy 路径，保留 scheme+host:port。
    pub fn base_url(&self) -> String {
        let ep = self.endpoint.trim().trim_end_matches('/');
        match ep.find("://") {
            Some(idx) => {
                let rest = &ep[idx + 3..];
                let host = rest.split('/').next().unwrap_or(rest);
                format!("http://{host}")
            }
            None => format!("http://{ep}"),
        }
    }
}

/// 数据目录：env `QAQH_DATA_DIR` 覆盖；Windows `%USERPROFILE%\.qaqh`；
/// Unix `$XDG_CONFIG_HOME/qaqh` 或 `~/.config/qaqh`。
pub fn data_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("QAQH_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    if cfg!(windows) {
        let home = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        home.join(".qaqh")
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        base.join("qaqh")
    }
}

fn discovery_path() -> std::path::PathBuf {
    data_dir().join("daemon.json")
}

pub fn read_discovery() -> Option<DaemonDiscovery> {
    let bytes = std::fs::read(discovery_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 宽松读取：文件存在但解析失败时给出可读错误。
#[allow(dead_code)]
pub fn read_discovery_strict() -> Result<DaemonDiscovery> {
    let path = discovery_path();
    let bytes = std::fs::read(&path)
        .with_context(|| format!("读取 {} 失败（daemon 未运行？）", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("解析 {} 失败", path.display()))
}

/// 进程存活检测（与 `discovery.rs:217-238` 等价）。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code);
            let _ = CloseHandle(handle);
            ok != 0 && exit_code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// `daemon.lock` 持有者是否存活（避免双 daemon）。
fn lock_holder_alive() -> bool {
    let path = data_dir().join("daemon.lock");
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content
        .split_whitespace()
        .next()
        .and_then(|first| first.trim().parse::<u32>().ok())
        .map(pid_alive)
        .unwrap_or(false)
}

/// 尝试拉起 `qaqh-daemon run`（detached）。候选顺序与 `discovery.rs:148-186` 一致。
fn spawn_daemon_detached() -> Result<()> {
    let exe_names = if cfg!(windows) { "qaqh-daemon.exe" } else { "qaqh-daemon" };
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("QAQH_BACKEND_ROOT") {
        candidates.push(std::path::PathBuf::from(root).join("target/debug").join(exe_names));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("target/debug").join(exe_names));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("resources").join(exe_names));
            candidates.push(dir.join(exe_names));
        }
    }

    let found = candidates.iter().find(|p| p.is_file()).cloned();
    let Some(daemon_exe) = found else {
        bail!("未找到 qaqh-daemon 可执行文件（可设置 QAQH_BACKEND_ROOT 或手动启动 daemon）");
    };

    let mut cmd = std::process::Command::new(&daemon_exe);
    cmd.arg("run").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0800_0000 | 0x0000_0200);
    }
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().with_context(|| format!("启动 {} 失败", daemon_exe.display()))?;
    Ok(())
}

/// 确保有一个健康的 daemon：读 discovery → pid 存活 → /health 探活；
/// 失败且允许时尝试拉起 daemon 并轮询 discovery 就绪。
pub async fn ensure_daemon(spawn_if_missing: bool) -> Result<DaemonDiscovery> {
    if let Some(d) = read_discovery() {
        if pid_alive(d.pid) {
            return Ok(d);
        }
    }
    if !spawn_if_missing {
        bail!("daemon.json 缺失或已失效（daemon 未运行）");
    }
    if !lock_holder_alive() {
        spawn_daemon_detached()?;
    }
    // 与 SDK 一致：120ms 轮询 discovery，超时 25s（冷启动余量）。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        if let Some(d) = read_discovery() {
            if pid_alive(d.pid) {
                return Ok(d);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("等待 daemon 就绪超时（25s）");
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_strips_legacy_path() {
        let d = |endpoint: &str| DaemonDiscovery {
            endpoint: endpoint.to_string(),
            token: "t".into(),
            pid: 1,
            server_epoch: "e".into(),
            protocol_version: 1,
            daemon_version: String::new(),
            build_id: String::new(),
            channel: String::new(),
            executable: String::new(),
        };
        assert_eq!(d("http://127.0.0.1:64413").base_url(), "http://127.0.0.1:64413");
        assert_eq!(d("ws://127.0.0.1:64413/control/v1").base_url(), "http://127.0.0.1:64413");
        assert_eq!(d("http://localhost:1/").base_url(), "http://localhost:1");
    }
}
