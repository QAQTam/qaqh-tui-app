//! qaqh-tui：QAQ-Harness 的终端前端（qaqh.Ringing v1）。

mod app;
mod protocol;
mod runtime;
mod transport;
mod ui;

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use ratatui::crossterm::event::{
    Event, EventStream, EnableBracketedPaste, EnableMouseCapture, KeyEventKind,
};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use app::{App, AppMsg};
use runtime::{Runtime, RuntimeMsg};
use transport::discovery::{ensure_daemon, read_discovery};
use transport::http::{new_instance_id, ApiError, HttpClient};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("doctor") => return doctor(),
        Some("--help") | Some("-h") | Some("help") => {
            println!("qaqh-tui — QAQ-Harness 终端客户端 (qaqh.Ringing v{})", protocol::RINGING_VERSION);
            println!();
            println!("用法:");
            println!("  qaqh-tui            连接本地 daemon 并进入 TUI");
            println!("  qaqh-tui --no-spawn 不自动拉起 daemon（仅连接已有实例）");
            println!("  qaqh-tui doctor     自检：发现/健康/open 握手");
            println!();
            println!("环境: QAQH_DATA_DIR（数据目录覆盖）、QAQH_BACKEND_ROOT（daemon 拉起候选）、QAQH_DEFAULT_CWD（新建会话默认目录，支持 ~/ 展开）");
            return Ok(());
        }
        _ => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 tokio runtime")?;
    runtime.block_on(run_tui(args.iter().any(|a| a == "--no-spawn")))
}

async fn run_tui(no_spawn: bool) -> Result<()> {
    // ── 发现 + 客户端 ──
    let discovery = ensure_daemon(!no_spawn).await.context("daemon 发现失败")?;
    let client = Arc::new(HttpClient::new(
        discovery.base_url(),
        discovery.token.clone(),
        new_instance_id(),
    ));

    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppMsg>();

    // runtime → app 桥接。
    let (rt_tx, mut rt_rx) = mpsc::unbounded_channel::<RuntimeMsg>();
    let runtime = Arc::new(Runtime::start(client.clone(), rt_tx));
    {
        let bridge_tx = app_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rt_rx.recv().await {
                if bridge_tx.send(AppMsg::Runtime(msg)).is_err() {
                    break;
                }
            }
        });
    }

    // 终端初始化（ratatui 0.30：init/restore + panic hook）。
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)
        .context("启用鼠标/粘贴")?;

    // 输入任务。
    {
        let input_tx = app_tx.clone();
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            while let Some(ev) = reader.next().await {
                match ev {
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                        if input_tx.send(AppMsg::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Mouse(m)) => {
                        if input_tx.send(AppMsg::Mouse(m)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Paste(s)) => {
                        if input_tx.send(AppMsg::Paste(s)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(_w, _h)) => {
                        if input_tx.send(AppMsg::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // 心跳任务（toast 过期 / Ctrl+C 双击窗口 / 时钟 / 动画 200ms）。
    {
        let tick_tx = app_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                interval.tick().await;
                if tick_tx.send(AppMsg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(client.clone(), runtime.clone(), app_tx.clone());
    // 首页：无 tab 时直接展示会话列表，立即拉取一次避免首帧空白
    app.fetch_session_list();

    // 主循环：事件驱动，批量消费后单帧重绘。
    let loop_result: Result<()> = async {
        loop {
            if app.quit {
                break;
            }
            let width = terminal.size()?.width;
            app.ensure_render_caches(width);
            terminal.draw(|f| ui::draw(f, &app))?;

            let Some(msg) = app_rx.recv().await else { break };
            app.handle(msg);
            // 排空积压（一帧内合并多个事件）。
            while let Ok(msg) = app_rx.try_recv() {
                app.handle(msg);
                if app.quit {
                    break;
                }
            }
        }
        Ok(())
    }
    .await;

    runtime.shutdown().await;
    ratatui::restore();
    loop_result
}

// ───────────────────────── doctor 自检 ─────────────────────────

fn doctor() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(doctor_async())
}

async fn doctor_async() -> Result<()> {
    println!("== qaqh-tui doctor ==");

    // 1) discovery + pid 存活；失效则尝试拉起 daemon。
    let discovery = match read_discovery() {
        Some(d) if transport::discovery::pid_alive(d.pid) => {
            println!(
                "[1] daemon.json: endpoint={} pid={} epoch={} version={} channel={}",
                d.endpoint, d.pid, d.server_epoch, d.daemon_version, d.channel
            );
            println!("[2] pid {} 存活", d.pid);
            d
        }
        stale => {
            match stale {
                Some(d) => println!("[1] daemon.json 过期（pid {} 已退出），尝试拉起…", d.pid),
                None => println!(
                    "[1] daemon.json 缺失（数据目录: {}），尝试拉起…",
                    transport::discovery::data_dir().display()
                ),
            }
            let d = ensure_daemon(true).await?;
            println!("[2] daemon 已就绪：endpoint={} pid={}", d.endpoint, d.pid);
            d
        }
    };

    let client = HttpClient::new(discovery.base_url(), discovery.token.clone(), new_instance_id());

    match client.health().await {
        Ok(body) => println!("[3] /health: {body}"),
        Err(e) => bail!("[3] /health 失败: {e}"),
    }

    match client.open().await {
        Ok(resp) => {
            print_open(&resp);
            println!("[5] OK —— 可以运行 qaqh-tui");
        }
        Err(ApiError::UnsupportedVersion(m)) => {
            bail!("[4] open 被拒（协议代差）: {m} —— 请更新客户端或 daemon")
        }
        Err(e) => bail!("[4] open 失败: {e}"),
    }
    Ok(())
}

fn print_open(resp: &protocol::capability::ClientOpenResponse) {
    println!(
        "[4] open: accepted={} session={} lease_ttl={}ms renew={}ms",
        resp.accepted, resp.client_session_id, resp.lease_ttl_ms, resp.renew_interval_ms
    );
}
