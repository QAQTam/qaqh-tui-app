//! `/remote` 页的状态：直连目标、局域网快照、配对票据与设备表。
//!
//! 三条边界写在这里，改代码前先读：
//! 1. **token 只在进程内**。`draft_token` / `RemoteTarget.token` 都不落盘、不写日志、
//!    不进 argv；界面一律画 `●●●●`。
//! 2. **本客户端自己的连接面只走 `http://`**（见 [`crate::runtime::RemoteTarget::new`]：
//!    客户端没有指纹锚定能力，自签证书会在握手期失败）。
//! 3. **二维码里的 `base_url` 是手机的入口，不是我们自己的**：它取 daemon.json 的
//!    `lan_endpoint`，那是 daemon 在**非回环 bind** 上开的 TLS 面，所以二维码里出现
//!    `https://` 是对的，别按第 2 条"顺手改成 http"——原生端拿 `tls_fp` 做 pinning。

use std::time::Instant;

use qaqh_client::{DaemonDiscovery, RingingV2DeviceWire};

use crate::runtime::RemoteTarget;

/// 配对票据的 wire 载荷版本（与桌面壳 `src-tauri/src/pairing.rs` 同一形状，
/// 后端 `docs/spec-daemon-auth-devices.md` 认这个 `kind`）。
pub const PAIR_PAYLOAD_VERSION: u8 = 1;

/// 二维码能容纳的设备名上限：载荷越长码越宽，80 列终端放不下就是废码。
pub const MAX_DEVICE_NAME_CHARS: usize = 24;

/// 局域网快照（来自本机 `daemon.json`，远端机器上的 TUI 读不到别的机的这个文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanSnapshot {
    /// 回环面（本客户端日常连的那个）。
    pub endpoint: String,
    /// 局域网面；`None` = daemon 当前没开 `server --bind`。
    pub lan_endpoint: Option<String>,
    /// 自签证书 SPKI 指纹 `sha256:<hex>`，只进二维码给原生端锚定用。
    pub tls_fingerprint: Option<String>,
    pub pid: u32,
    pub daemon_version: String,
}

impl LanSnapshot {
    /// 从权威 discovery 类型取值（本仓不手解 daemon.json）。
    pub fn from_discovery(d: &DaemonDiscovery) -> Self {
        Self {
            endpoint: d.endpoint.clone(),
            lan_endpoint: d.lan_endpoint.clone(),
            tls_fingerprint: d.tls_fingerprint.clone(),
            pid: d.pid,
            daemon_version: d.daemon_version.clone(),
        }
    }
}

/// 一次配对票据的签发结果（120 秒寿命）。
#[derive(Debug, Clone)]
pub struct PairTicket {
    /// 签给谁的档位名（wire 值）。
    pub scope: &'static str,
    pub device_name: String,
    /// 二维码载荷文本（见 [`build_pair_payload`]）。
    ///
    /// 一次性令牌**只存在于这个串里**：不再单独存一份字段，界面上也不打印它——
    /// 少一份副本就少一处泄露点（载荷本身 120 秒后作废）。
    pub payload: String,
    /// 过期时刻（本地单调钟，不受系统时间调整影响）。
    pub expires_at: Instant,
}

impl PairTicket {
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    /// 剩余秒数；过期后归零（`saturating_duration_since` 天然不会出现负数）。
    pub fn remaining_secs(&self, now: Instant) -> u64 {
        self.expires_at.saturating_duration_since(now).as_secs()
    }
}

/// 组装二维码载荷。
///
/// `lan_base_url` 必须是 daemon 的**局域网**地址（`lan_endpoint`）；回环地址对手机
/// 没有意义。`tls_fp` 允许为空（daemon 没起 TLS 面时就没有），空值照样进载荷，
/// 原生端会按"无指纹可锚"处理，比我们在本地猜一个默认值要好。
pub fn build_pair_payload(
    lan_base_url: &str,
    pairing_token: &str,
    tls_fp: &str,
    host_name: &str,
) -> String {
    // 手工拼而不是 serde_json::json!：这不是 wire 契约（daemon 不解析它，只有手机
    // 扫码后解析），而 `json!` 会把里面的 `/` 转义成 `\/` 的诱惑留给人踩。
    // 三个字符串字段都要做 JSON 转义，否则设备名里一个引号就能破掉载荷结构。
    let esc = |s: &str| -> String {
        let mut out = String::with_capacity(s.len() + 8);
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    };
    format!(
        "{{\"v\":{PAIR_PAYLOAD_VERSION},\"kind\":\"qaqh-pair\",\"base_url\":\"{}\",\
         \"pairing_token\":\"{}\",\"tls_fp\":\"{}\",\"host_name\":\"{}\"}}",
        esc(lan_base_url),
        esc(pairing_token),
        esc(tls_fp),
        esc(host_name)
    )
}

/// 主机名回退链：`COMPUTERNAME`（Windows）→ `HOSTNAME` → 固定串。与桌面壳同序。
pub fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "qaqh-tui".to_string())
}

/// 设备名里允许进载荷的字符上限（超出就截断，别让一个长名字把整张码撑破）。
pub fn clamp_device_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= MAX_DEVICE_NAME_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_DEVICE_NAME_CHARS).collect()
}

/// 签发请求的载荷上下文。
///
/// 令牌是异步结果（`spawn_api` 回来时已经拿不到借用），而二维码需要的是 **daemon 的
/// 局域网地址 + 指纹 + 主机名**——这三样都在发起那一刻的 discovery 快照里，所以随
/// 请求暂存一份，结果落地时用。丢了就明确报错，不猜（猜出来的 base_url 会让手机
/// 连到一个不相干的机器）。
#[derive(Debug, Clone)]
pub struct RemotePending {
    pub base_url: String,
    pub machine: String,
    pub device_name: String,
    pub scope: qaqh_client::PairScope,
}

/// `/remote` 页 UI 状态。随 `Overlay::Remote` 持有，Esc 关闭即丢弃草稿。
#[derive(Debug, Default, Clone)]
pub struct RemoteState {
    /// 面板：0 = 直连目标，1 = 局域网与配对。
    pub panel: usize,
    /// 面板内字段游标。
    pub focus: usize,
    /// 设备列表选中行（面板 1）。
    pub device_sel: usize,
    /// 正在编辑的字段（None = 非编辑态）。
    pub editing: Option<RemoteField>,
    /// 编辑缓冲（沿用设置页那套 Vec<char> + cursor）。
    pub buffer: Vec<char>,
    pub cursor: usize,
    /// 表单草稿（未提交）。
    pub draft_url: String,
    pub draft_token: String,
    /// 配对设备名与档位游标（0=view 1=interact 2=admin）。
    pub device_name: String,
    pub scope_index: usize,
    /// discovery 快照。
    pub lan: Option<LanSnapshot>,
    /// 当前在途的配对票据（过期后保留给"重新生成"提示，渲染时按 expired 分支）。
    pub ticket: Option<PairTicket>,
    /// 已配对设备列表（不含任何 token 材料，可直接上屏）。
    pub devices: Vec<RingingV2DeviceWire>,
    /// 两步确认中的设备 id（第一次按 x 只置位，第二次才真吊销）。
    pub revoke_armed: Option<String>,
    /// 页面底部的状态行：`None` = 无，`Some(false/true)` = 提示/错误。
    pub status: Option<String>,
    pub status_is_error: bool,
    /// 有动作在途（连接 / 签发 / 拉列表 / 吊销），期间拒绝重复触发。
    pub busy: bool,
    /// 页面自身滚动行（二维码一屏放不下，滚动条由滚轮/PgUp 驱动）。
    pub scroll: usize,
    /// 配对码剩余秒数（由 tick 刷新，渲染层不自己读时钟——保持 draw 是纯函数）。
    pub ticket_countdown: Option<u64>,
}

impl RemoteState {
    /// 由 tick 驱动：刷新倒计时，返回**是否需要重绘**。
    ///
    /// 过期不只是数字归零：过期后必须把二维码撤下（后端那个令牌已经作废，继续挂着
    /// 一张能扫但必然失败的码是误导）。
    pub fn advance_clock(&mut self, now: Instant) -> bool {
        let countdown = match self.ticket.as_ref() {
            None => None,
            Some(ticket) if ticket.is_expired(now) => Some(0),
            Some(ticket) => Some(ticket.remaining_secs(now)),
        };
        if countdown == self.ticket_countdown {
            return false;
        }
        let expired = countdown == Some(0);
        self.ticket_countdown = countdown;
        if expired {
            // 过期即丢码：`ticket` 留着会让下一次渲染仍然画出一张死码。
            self.ticket = None;
            self.status = Some("配对码已过期，重新生成".to_string());
            self.status_is_error = false;
        }
        true
    }
}

/// 页面上可聚焦的字段（顺序即 UI 顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteField {
    // 面板 0
    BaseUrl,
    Token,
    Connect,
    GoLocal,
    // 面板 1（局域网状态只读：本页面能开的是「配对」，不是「替用户改 daemon 的 bind」）
    DeviceName,
    Scope,
    Issue,
    RefreshDevices,
}

impl RemoteState {
    /// 面板 0 的字段（当前只在面板 0 里循环）。
    pub const PANEL_ZERO: [RemoteField; 4] = [
        RemoteField::BaseUrl,
        RemoteField::Token,
        RemoteField::Connect,
        RemoteField::GoLocal,
    ];
    pub const PANEL_ONE: [RemoteField; 4] = [
        RemoteField::DeviceName,
        RemoteField::Scope,
        RemoteField::Issue,
        RemoteField::RefreshDevices,
    ];

    pub fn fields(&self) -> &'static [RemoteField] {
        if self.panel == 0 {
            &Self::PANEL_ZERO
        } else {
            &Self::PANEL_ONE
        }
    }

    /// 当前聚焦字段（越界回绕到 0，面板切换后 `focus` 可能大于新面板的行数）。
    pub fn focus(&self) -> RemoteField {
        let fields = self.fields();
        fields
            .get(self.focus % fields.len())
            .copied()
            .unwrap_or(fields[0])
    }

    /// 字段间环形移动。
    pub fn move_focus(&mut self, delta: isize) {
        let len = self.fields().len() as isize;
        self.focus = (self.focus as isize + delta).rem_euclid(len) as usize;
    }

    /// 进入编辑态：缓冲预填当前值（token 恒空——不回填也不回显）。
    pub fn begin_edit(&mut self) {
        let field = self.focus();
        let seed = match field {
            RemoteField::BaseUrl => self.draft_url.clone(),
            RemoteField::Token => String::new(),
            RemoteField::DeviceName => self.device_name.clone(),
            other => {
                debug_assert!(false, "{other:?} 不是文本字段，不该进编辑态");
                return;
            }
        };
        self.cursor = seed.chars().count();
        self.buffer = seed.chars().collect();
        self.editing = Some(field);
    }

    /// 提交编辑缓冲到对应草稿字段。
    pub fn apply_edit(&mut self, field: RemoteField, text: &str) {
        match field {
            RemoteField::BaseUrl => self.draft_url = text.trim().to_string(),
            RemoteField::Token => self.draft_token = text.trim().to_string(),
            RemoteField::DeviceName => self.device_name = clamp_device_name(text),
            other => debug_assert!(false, "{other:?} 不走文本提交"),
        }
    }

    /// 重新读一遍本机 daemon.json（局域网面可能是刚开的）。
    ///
    /// 这是一次**小文件同步读**，只在打开页面与签发配对码前发生，不在热路径上。
    /// 解析失败不覆盖旧快照：daemon 重启那一瞬读不到，比留着上一次的值得显示。
    pub fn refresh_lan(&mut self) {
        if let Ok(discovery) = qaqh_client::read_discovery() {
            self.lan = Some(LanSnapshot::from_discovery(&discovery));
        }
    }

    /// 草稿是否够发起一次直连。
    pub fn draft_target(&self) -> Result<RemoteTarget, String> {
        RemoteTarget::new(&self.draft_url, &self.draft_token)
    }

    pub fn set_status(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = Some(text.into());
        self.status_is_error = is_error;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 载荷必须是合法 JSON：设备名里带引号/反斜杠是用户能敲出来的东西，
    /// 破掉结构的话手机扫码只会得到一个解不开的串。
    #[test]
    fn pair_payload_is_valid_json_with_hostile_names() {
        let payload = build_pair_payload(
            "https://192.168.1.8:64413",
            "pt-abc\"def",
            "sha256:11aa\\bb",
            "里子\\的\"主机\n",
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("载荷必须是合法 JSON");
        assert_eq!(parsed["v"], PAIR_PAYLOAD_VERSION);
        assert_eq!(parsed["kind"], "qaqh-pair");
        assert_eq!(parsed["base_url"], "https://192.168.1.8:64413");
        assert_eq!(parsed["pairing_token"], "pt-abc\"def");
        assert_eq!(parsed["tls_fp"], "sha256:11aa\\bb");
        assert_eq!(parsed["host_name"], "里子\\的\"主机\n");
        assert!(
            payload.contains("https://"),
            "二维码里的 base_url 是手机的入口，局域网面就是 https，不许被改成 http"
        );
    }

    /// 载荷形状必须能被后端/原生端按同一套键解出来（缺键 = 手机侧解析失败）。
    #[test]
    fn pair_payload_carries_every_required_key() {
        let parsed: serde_json::Value =
            serde_json::from_str(&build_pair_payload("https://h:1", "t", "", "host"))
                .expect("json");
        for key in [
            "v",
            "kind",
            "base_url",
            "pairing_token",
            "tls_fp",
            "host_name",
        ] {
            assert!(parsed.get(key).is_some(), "载荷缺键 {key}");
        }
        assert_eq!(parsed["tls_fp"], "", "空指纹要保留成空串，不是缺席");
    }

    #[test]
    fn device_name_is_clamped_not_rejected() {
        assert_eq!(clamp_device_name("  studio-phone  "), "studio-phone");
        let long = "a".repeat(80);
        assert_eq!(
            clamp_device_name(&long).chars().count(),
            MAX_DEVICE_NAME_CHARS,
            "超长设备名截断，别让二维码撑破终端宽度"
        );
    }

    /// `LanSnapshot` 只认 discovery 的字段；`lan_endpoint` 缺席 = 没开局域网。
    #[test]
    fn snapshot_reads_lan_and_fingerprint() {
        let mut d = discovery_fixture();
        let snap = LanSnapshot::from_discovery(&d);
        assert_eq!(snap.lan_endpoint, None, "回环模式下没有局域网面");
        d.lan_endpoint = Some("https://192.168.1.8:64413".into());
        d.tls_fingerprint = Some("sha256:aa".into());
        let snap = LanSnapshot::from_discovery(&d);
        assert_eq!(
            snap.lan_endpoint.as_deref(),
            Some("https://192.168.1.8:64413")
        );
        assert_eq!(snap.tls_fingerprint.as_deref(), Some("sha256:aa"));
    }

    fn discovery_fixture() -> DaemonDiscovery {
        serde_json::from_value(serde_json::json!({
            "endpoint": "http://127.0.0.1:64413",
            "token": "t",
            "pid": 1,
            "server_epoch": "e",
            "protocol_version": 2,
            "daemon_version": "2.0.0-beta.3",
            "build_id": "b",
            "channel": "dev",
            "executable": "/bin/qaqh-daemon",
        }))
        .expect("discovery 缺 lan 字段必须可解析（serde default）")
    }

    /// 倒计时只读语义：过期后 `remaining_secs` 归零，且不会因时钟回拨变负。
    #[test]
    fn ticket_expiry_counts_down_to_zero_only() {
        let now = Instant::now();
        let ticket = PairTicket {
            scope: "view",
            device_name: "phone".into(),
            payload: "{}".into(),
            expires_at: now + std::time::Duration::from_secs(30),
        };
        assert!(!ticket.is_expired(now));
        assert!(ticket.remaining_secs(now) <= 30);
        // 已经过期：0 而不是负数 / panic。
        let late = now + std::time::Duration::from_secs(31);
        assert!(ticket.is_expired(late));
        assert_eq!(ticket.remaining_secs(late), 0);
    }

    /// 过期必须把码撤掉：令牌在后端已经作废,屏上继续挂一张能扫但必然失败的二维码
    /// 比不给码更糟。秒数没变时返回 false（绘制门控靠它决定要不要重绘）。
    #[test]
    fn expired_ticket_is_dropped_and_asks_for_redraw_only_on_change() {
        let now = Instant::now();
        let mut st = RemoteState {
            ticket: Some(PairTicket {
                scope: "view",
                device_name: "phone".into(),
                payload: "{\"pairing_token\":\"pt\"}".into(),
                expires_at: now + std::time::Duration::from_secs(5),
            }),
            ..Default::default()
        };
        // 同一秒内二次推进：不重绘。
        assert!(st.advance_clock(now), "首次写入倒计时要重绘");
        assert!(!st.advance_clock(now), "秒数没变不该重绘");
        // 过期：码消失 + 提示重新生成 + 需要重绘。
        let late = now + std::time::Duration::from_secs(6);
        assert!(st.advance_clock(late));
        assert!(st.ticket.is_none(), "过期后不得继续挂二维码");
        assert!(
            st.status.as_deref().is_some_and(|s| s.contains("过期")),
            "{:?}",
            st.status
        );
    }
}
