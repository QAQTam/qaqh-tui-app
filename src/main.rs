//! qaqh-tui：QAQ-Harness 的终端前端（V2 Fullscreen；Ringing v2）。

mod app;
mod protocol;
mod runtime;
mod terminal;
mod theme;
mod ui;

use anyhow::{Context, Result, bail};

/// 极简文件 logger：设了 `QAQH_TUI_LOG=<path>` 才安装。
///
/// **为什么需要**：TUI 自身不记日志，而 `qaqh-client` 的诊断（timeline 重连原因、
/// 快照恢复失败、非法 cursor 告警……）全部走 `log` 门面。没有 logger 时这些**全被
/// 丢掉**——真机排查只剩 UI 上那句「timeline[….] 断开，1000ms 后重连」，而
/// `ReconnectReason` 只覆盖「服务端主动终止流」，普通 HTTP 错误（401 等）不带
/// reason，等于**没有原因**。这个缺口直接卡住过故障钩子的接线排查。
///
/// 默认关闭：不安装 logger 时 `log` 门面是空操作，行为与之前完全一致。
struct FileLogger;

impl log::Log for FileLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let Ok(path) = std::env::var("QAQH_TUI_LOG") else {
            return;
        };
        // 每条记录开关一次文件：这是**诊断开关**，不在热路径上，不值得为它引入
        // 全局文件句柄与锁。
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write as _;
            let _ = writeln!(file, "[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn init_logging() {
    if std::env::var_os("QAQH_TUI_LOG").is_none() {
        return;
    }
    static LOGGER: FileLogger = FileLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}

fn main() -> Result<()> {
    init_logging();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("doctor") => return doctor(),
        Some("--version") | Some("-V") | Some("version") => {
            println!("qaqh-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") | Some("help") => {
            println!(
                "qaqh-tui {} — QAQ-Harness 终端客户端 (qaqh.Ringing v{})",
                env!("CARGO_PKG_VERSION"),
                qaqh_client::RINGING_VERSION
            );
            println!();
            println!("用法:");
            println!("  qaqh-tui            连接本地 daemon 并进入 V2 全屏 TUI");
            println!("  qaqh-tui resume     直接浏览当前 cwd 下的会话");
            println!("  qaqh-tui --no-spawn 不自动拉起 daemon（仅连接已有实例）");
            println!("  qaqh-tui doctor     自检：发现/pid 判活/open 握手");
            println!("  qaqh-tui --version  打印版本");
            println!();
            println!(
                "环境: QAQH_DATA_DIR（数据目录覆盖）、QAQH_BACKEND_ROOT（daemon 拉起候选）、QAQH_DEFAULT_CWD（新建会话默认目录，支持 ~/ 展开）、QAQH_THEME=night|day|terminal|auto"
            );
            return Ok(());
        }
        _ => {}
    }

    if args.iter().any(|arg| arg == "--v1") {
        bail!("v1 全屏兼容路径已删除；当前仅支持 V2 fullscreen");
    }
    if args.iter().any(|arg| arg == "--v2-inline") || std::env::var_os("QAQH_V2_INLINE").is_some() {
        bail!("v2 inline 旧版设计已删除；当前仅支持 V2 fullscreen");
    }

    let resume = args.iter().any(|arg| arg == "resume");
    let no_spawn = args.iter().any(|arg| arg == "--no-spawn");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 tokio runtime")?;
    runtime.block_on(terminal::agent::run(no_spawn, resume))
}

// ───────────────────────── doctor 自检 ─────────────────────────

fn doctor() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(doctor_async())
}

async fn doctor_async() -> Result<()> {
    println!("== qaqh-tui doctor ==");

    // 1) discovery + pid 存活。
    match qaqh_client::read_discovery() {
        Ok(d) if qaqh_client::discovery::process_is_running(d.pid) => {
            println!(
                "[1] daemon.json: endpoint={} pid={} epoch={} version={} channel={}",
                d.endpoint, d.pid, d.server_epoch, d.daemon_version, d.channel
            );
            println!("[2] pid {} 存活", d.pid);
        }
        Ok(d) => {
            println!("[1] daemon.json 过期（pid {} 已退出）", d.pid);
            println!("[2] 运行 qaqh-tui 时会尝试拉起 daemon");
        }
        Err(e) => {
            println!(
                "[1] daemon.json 不可用（数据目录: {}）：{e}",
                qaqh_client::discovery::data_dir().display()
            );
            println!("[2] 运行 qaqh-tui 时会尝试拉起 daemon");
        }
    }

    // 2) open 握手。协议代差/凭据问题都在这一步暴露。
    match qaqh_client::Client::connect_async(qaqh_client::ClientOptions {
        launch_daemon_if_missing: true,
        ..Default::default()
    })
    .await
    {
        Ok(client) => {
            match client.session_state().await {
                Some(state) => println!(
                    "[3] open: session={} epoch={} lease_ttl={}ms renew={}ms",
                    state.client_session_id,
                    state.server_epoch,
                    state.lease_ttl_ms,
                    state.renew_interval_ms
                ),
                None => bail!("[3] open 成功但未协商出 session"),
            }
            println!("[4] OK —— 可以运行 qaqh-tui");
            client.close();
            Ok(())
        }
        Err(qaqh_client::ClientError::Negotiation(m)) => {
            bail!("[3] open 被拒（协议代差）: {m} —— 请更新客户端或 daemon")
        }
        Err(e) => bail!("[3] open 失败: {e}"),
    }
}
