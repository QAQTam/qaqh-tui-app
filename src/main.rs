//! qaqh-tui：QAQ-Harness 的终端前端（qaqh.Ringing v1）。

mod app;
mod protocol;
mod runtime;
mod ui;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use ratatui::crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyEventKind,
};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use app::{App, AppMsg};
use runtime::{Runtime, RuntimeMsg};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("doctor") => return doctor(),
        Some("--help") | Some("-h") | Some("help") => {
            println!(
                "qaqh-tui — QAQ-Harness 终端客户端 (qaqh.Ringing v{})",
                qaqh_client::RINGING_VERSION
            );
            println!();
            println!("用法:");
            println!("  qaqh-tui            连接本地 daemon 并进入 TUI");
            println!("  qaqh-tui --no-spawn 不自动拉起 daemon（仅连接已有实例）");
            println!("  qaqh-tui doctor     自检：发现/pid 判活/open 握手");
            println!();
            println!(
                "环境: QAQH_DATA_DIR（数据目录覆盖）、QAQH_BACKEND_ROOT（daemon 拉起候选）、QAQH_DEFAULT_CWD（新建会话默认目录，支持 ~/ 展开）"
            );
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
    // 先建通道再连接：连接期间 `ClientHandlers` 回调发来的消息先入队，等 App
    // 构造好后一并排空（否则首个 Ready/事件会丢）。
    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppMsg>();
    let (rt_tx, mut rt_rx) = mpsc::unbounded_channel::<RuntimeMsg>();
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

    // 连接生命周期（含「daemon 不在则拉起」）全部交给 qaqh-client。
    let runtime = Runtime::start(rt_tx, !no_spawn)
        .await
        .context("连接 daemon 失败")?;

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

    let mut app = App::new(runtime.clone(), app_tx.clone());
    // 首页：无 tab 时直接展示会话列表，立即拉取一次避免首帧空白
    app.fetch_session_list();

    // 主循环：事件驱动，批量消费后单帧重绘。
    let loop_result: Result<()> = async {
        loop {
            if app.quit {
                break;
            }
            // 渲染缓存键必须与 ui::draw 实际使用的 transcript 内容宽一致
            // （ui::mod 的 transcript_content_width 是唯一事实源）。
            let area =
                ratatui::layout::Rect::new(0, 0, terminal.size()?.width, terminal.size()?.height);
            app.ensure_render_caches(area);
            terminal.draw(|f| ui::draw(f, &app))?;

            // M4（T15）：Ctrl+T 浮层按 `e` 置位 → 帧间挂起终端交给 $PAGER。
            if let Some(text) = app.pending_pager.take() {
                run_pager(&mut terminal, &text);
            }

            let Some(msg) = app_rx.recv().await else {
                break;
            };
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

// ───────────────────────── $PAGER（M4 / T15）─────────────────────────

/// 挂起终端 → 外部分页器全文浏览 → 恢复（重建 + 全量重绘）。
///
/// `$PAGER` 未设置 → `less -R`；`less` 不存在（exit 127）→ 退化为 `cat`。
/// 写临时文件失败则静默放弃（浮层本身仍可滚动，不是功能阻塞）。
fn run_pager(terminal: &mut ratatui::DefaultTerminal, text: &str) {
    let tmp = std::env::temp_dir().join(format!("qaqh-pager-{}.md", std::process::id()));
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    ratatui::restore();
    let cmd = format!(
        "{} {}",
        app::pager::pager_cmd(std::env::var("PAGER").ok().as_deref()),
        app::pager::shell_quote(&tmp.to_string_lossy()),
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status();
    if matches!(status.map(|s| s.code()), Ok(Some(127))) {
        let _ = std::process::Command::new("cat").arg(&tmp).status();
    }
    *terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let _ = terminal.clear();
    let _ = std::fs::remove_file(&tmp);
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
