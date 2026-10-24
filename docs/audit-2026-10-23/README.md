# 项目审计（2026-10-23）

## 结论与范围

当前代码仍有可复现的输入泄漏、失败恢复、展示完整性和性能问题，不能认定前后端已经完全接洽。设置页没有发现已被**当前本地后端**删除的可编辑字段；主要问题是异步保存、连接能力、发布依赖及部分能力缺口。

审计基线：TUI `46b2304` 加审计开始时已有未提交修改；本地后端 `c9ba369` 加其已有 `crates/qaqh-config/src/config.rs` 修改。结论针对这份工作区，不能直接外推到已发布的 2.0.0 二进制。审计保留了既有修改，只新增本目录的报告、复现及日志，没有修复产品代码。

- 原有 Rust 测试：598 passed；发布脚本 Python 测试：5 passed。
- 新增隔离审计断言：13 条，当前全部失败，证实对应行为缺口；不接入默认测试。
- 其中 Windows 项只在 Linux 复现了危险 URL 可通过门禁，命令执行链由代码与官方文档核对，**没有执行 Windows 原生攻击验证**。
- `git diff --check` 通过；`cargo fmt -- --check` 未通过，现存格式差异位于 `account.rs`、`anim.rs`、`workspace.rs`，未替用户改写。
- 未启动或重启真实 daemon，未改真实配置，未发送消息或凭据，未做宿主 CPU/RSS 采样、依赖 CVE 全扫描、Windows/macOS 实机验证。

证据：[原有测试结果](baseline-output.txt)、[审计断言结果](repro-output.txt)、[断言源码](repros.rs)。所有凭据与正文均是假数据。

## 需要优先处理的问题

### F01 · P1 · 远端 Token 粘贴进入隐藏聊天输入框

位置：[handle_paste](../../src/app/mod.rs#L1204)、[Remote 编辑器](../../src/app/remote_ops.rs#L151)。

`handle_paste` 只识别 AttachPath 和 Settings 编辑器，没有处理 Remote 编辑器，也没有在其他覆盖层存在时停止向聊天透传。活动会话中打开 `/remote` → 编辑 Token → 粘贴，会把 Token 写入底层 `session.composer`，远端输入框反而为空。退出页面后可能把这段凭据作为普通消息发给模型。

复现：`audit_remote_token_paste_must_not_reach_chat`。已确认粘贴落到了聊天输入框；没有真的发送凭据。CwdInput 等未处理覆盖层也有同类路由问题。

建议：统一按可见输入目标分派粘贴；覆盖层存在时禁止回落隐藏 composer。为 Remote Token、URL、路径和无输入覆盖层分别验证。

### F02 · P1 · Windows 浏览器启动存在命令注入风险

位置：[URL 门禁与启动](../../src/app/browser.rs#L13)、[后台 URL 调用点](../../src/app/account_ops.rs#L248)。

Windows 分支实际执行 `cmd /C start "" <url>`。门禁只限制 scheme、空白与控制字符，允许 `&`、`|`、`%` 等 shell 元字符。`.arg(url)` 对 `cmd.exe` 的解析规则不构成充分保护。普通 OAuth URL 的多个查询参数也可能被 `&` 拆开，造成授权失败。

复现只调用 `is_supported("https://example.invalid/&echo.AUDIT")`，确认门禁放行；没有启动浏览器或执行命令。利用要求 Windows 环境及攻击者能够影响服务端返回的授权/用量 URL，不是“任意聊天文本都会执行”。Rust 官方明确警告 `cmd.exe` 使用非标准参数解析，恶意参数可能执行 shell 命令。[Rust Command::arg 文档](https://doc.rust-lang.org/std/process/struct.Command.html#method.arg)

建议：使用 Windows 原生 URL 打开 API 或经过验证的跨平台打开库，移除这条 `cmd /C` 路径，并验证正常多参数 OAuth URL。

### F03 · P1 · ask / permission / plan 应答失败后不能重新操作

位置：[submit_ask](../../src/app/interaction.rs#L78)、[respond_plan / respond_permission](../../src/app/interaction.rs#L155)、[CommandAck 错误处理](../../src/app/mod.rs#L2454)。

发送之前就清除面板，把 interaction/call id 写入 `suppressed_interactions`；权限还会在会话内记为已解决。连接错误、拒绝或 10 秒超时后只显示 toast，没有恢复面板与抑制状态。后端仍可能在等待同一交互，本客户端却不能再次作答；重新获取同一 pending 交互也会受本地 suppression 影响。

`audit_failed_ask_response_must_remain_retryable` 使用无连接 stub，确认确定未送达时面板仍不恢复。permission/plan 的同类问题由共同收口路径与各自预先删除状态确认。

建议：保留在途应答及面板快照；确定失败时恢复，超时等结果未知时先查询 receipt 和权威 pending 状态，再提供重试。避免自动重发已经被后端执行的应答。

### F04 · P1 · 当前工作区与发布固定后端不兼容

位置：[固定依赖](../../release/dependencies.json#L5)、[CI 检出与测试](../../.github/workflows/release.yml#L62)、[沙箱设置读写](../../src/app/settings.rs#L523)。

本地新代码使用 `ConfigDto.sandbox`、`ConfigPatch.sandbox`，发布脚本仍固定后端 `80ddb3c785a0af4dfe217169119c1940bb53f7eb`。审计读取该提交的 `qaqh-config-api/src/lib.rs`，其中没有这些字段。本地依赖较新工作树可编译，但把当前改动按现有固定依赖发布，会遇到字段不存在的编译错误。

这是源码契约不匹配的静态结论，未启动 GitHub CI；不表示已有 2.0.0 标签必然失败。

建议：将当前功能依赖的后端完整提交与锁文件、发布清单同步，并在固定输入上验证后再发版。不要用当前本地测试通过代替发布输入验证。

### F05 · P1（远端能力）· TUI 拒绝后端真实局域网入口

位置：[RemoteTarget::new](../../src/runtime.rs#L180)、[后端 LAN/TLS 装配](../../../qaqh-backend/crates/qaqh-daemon/src/server.rs#L183)。

TUI 只接受 HTTP，显式拒绝 HTTPS；当前后端非 loopback bind 自动启用 TLS，discovery 的 LAN 地址为 HTTPS。因此直接连接正常的局域网 daemon 不成立，照错误提示把地址改为 HTTP 也不能与 TLS listener 通信。证书已加入系统信任库的 HTTPS 入口同样被 TUI 提前拒绝。

`audit_remote_tls_endpoint_must_be_accepted` 证实入口拒绝。对额外搭建的 HTTP 代理或其他明文目标，客户端又会允许非回环 Bearer 明文传输；这属于该替代部署的传输风险，不代表默认后端正用明文发送。

建议：支持正常 HTTPS 信任链，并由客户端层补齐自签证书指纹验证；SSH 转发到本机 HTTP 可作为明确的局部用法，不宜把所有远端连接引导为明文。

## 输入与状态逻辑

### F06 · P2 · 发送失败丢原输入，其他命令拒绝还能覆盖现有草稿

位置：[send_message](../../src/app/transcript_ops.rs#L81)、[拒绝与错误处理](../../src/app/mod.rs#L2420)。

`composer.take()` 提前取走文字和附件；结果回包不带原始输入。确定发送失败后只清理 pin、显示错误，无法恢复原文与附件。另一个分支在**所有**有 session_id 的命令被拒绝时，把错误文案直接写入 `composer.input`，连取消等无关命令都能覆盖用户新草稿。

两条复现：`audit_failed_send_must_retain_user_text`、`audit_rejected_control_must_not_overwrite_chat_draft`。后者把“用户正在编辑的草稿”变成了“取消 被拒绝: not_driver read only”。

建议：保存按 command_id 关联的待发送输入/附件，提供可见的失败草稿；错误反馈使用独立状态。恢复时也不要覆盖用户发送后继续输入的新文字。网络结果未知时需区分“恢复编辑”与“自动重发”。

### F07 · P2 · 设置保存回包抹掉之后的新编辑

位置：[save_settings](../../src/app/settings_ops.rs#L160)、[Settings 按键](../../src/app/overlay_ops.rs#L428)、[ConfigWrite](../../src/app/mod.rs#L2567)。

`settings_saving` 只阻止第二次保存，未阻止编辑或关闭重开面板。第一笔保存成功回读后，对当前所有 Settings overlay 执行 `st.draft = Default::default()`。例如提交 2048 后继续改成 4096，回包会清掉 4096；关闭旧页、新开一页编辑也可能被旧回包清空。

复现：`audit_settings_ack_must_preserve_edits_after_save`，结果 `Some(4096)` 变为 `None`。

建议：回包携带请求身份与提交快照，仅清除仍等于已提交值的字段；或明确锁定保存期间的编辑，并保证关闭重开后的草稿不被旧请求影响。

### F08 · P2 · exec 输出分片会覆盖普通文本

位置：[apply_bash_progress](../../src/app/timeline_model.rs#L85)、[生产 ToolProgress 调用](../../src/app/timeline_model.rs#L859)。

代码把“当前缓冲最后没有换行”误当成“上一片以 CR 结束”，下一片普通文字会清掉当前行。输入两片 `hel`、`lo\n`，结果是 `lo\n`，而不是 `hello\n`。适用于 exec/bash/shell/pwsh/powershell。

复现：`audit_exec_progress_fragments_must_append_without_cr`。最终工具结果可能补回完整正文，但运行中的输出与仅靠 progress 的回放已失真。已有测试只验证整行追加和 CR 进度，缺少普通半行分片。

建议：显式保留 CR/ANSI 解析状态，不从“缺少换行”推断覆盖；补齐随机切片、跨片 CRLF/ANSI 与一次性输入的一致性验证。

## 消息 UI 的完整性与冗余

### F09 · P2 · 回合失败原因没有显示

位置：[TurnFinished](../../src/app/mod.rs#L2148)、[fullscreen_sources](../../src/ui/v2/adapter.rs#L191)。

对话终态的 `error` 被 `..` 忽略；timeline 虽保存 `Turn.failure`，全屏适配器也没有把回合失败转成可见消息。提供“额度耗尽”等失败、且没有助手正文时，主 transcript 只留下用户问题。状态停止不等于向用户解释了失败。

复现：`audit_turn_failure_must_be_visible`，失败回合渲染只有“测试问题”，没有错误码或理由。`/history` 导出路径可能显示 failure，不能替代主界面的失败反馈。

建议：按 turn_id 显示一条持久错误/中止状态，去重两个事件面的同一终态，同时保留可操作的重试入口。

### F10 · P2 · offload 预览壳看起来像完整正文

位置：[Turn.offloaded](../../src/app/timeline_model.rs#L380)、[全屏适配](../../src/ui/v2/adapter.rs#L191)、[后端 offload](../../../qaqh-backend/crates/qaqh-runtime/src/timeline.rs#L1407)。

后端可以把块正文缩成 512 字符的预览，并清理部分工具输出。前端保留 `offloaded`，导出时会提示，但主 transcript 完全不使用此标志，没有“仅预览/查看全文”提示。复制正文也只能拿到当前保留文本，用户可能把残缺内容当作完整答案。

复现：`audit_offloaded_turn_must_not_look_complete`。触发条件是收到 offloaded 快照；若某条后端读取路径已从 sidecar 补全正文，该路径不受这项截断影响。

建议：显示预览状态，并在后端具备全文读取能力时按需补齐；“复制全文”必须区分完整内容与预览。

### F11 · P2 · progress 截断标记没有传给视图

位置：[from_tool](../../src/ui/v2/adapter.rs#L646)、[render_tool_body](../../src/ui/v2/transcript.rs#L1327)。

模型维护 `ToolCard.progress_truncated`，视图的 `truncated` 却只检查 typed body/outcome，漏掉 progress 截断。运行中丢了旧日志时仍没有可见提示；当前 reducer 还忽略实时 `stream` 和 `bytes_total`，对应指标不能在这条流上实时更新。

复现：`audit_progress_truncation_must_be_visible`，进度明确标为截断，界面只有剩余尾部。即使补上传播，空正文/摘要去重导致的提前 return 也需要保留截断提示。

建议：明确区分正文、进度、折叠三种信息损失，并保证提示不依赖正文非空。

### F12 · P2 · 折叠工具卡按逻辑行截取，实际可占多屏

位置：[TOOL_BODY_PREVIEW](../../src/ui/v2/transcript.rs#L28)、[选行与折行](../../src/ui/v2/transcript.rs#L1382)。

“三行预览”先按原始换行挑三行，再把每行按列宽折开。单行 JSON、长日志或压缩文本会绕过视觉高度限制。审计用 6000 个普通字符，在 80 列渲染出包含用户行在内的 **81 行**，工具卡尚未展开。

复现：`audit_collapsed_tool_preview_must_bound_visual_rows`。这是消息区冗杂与渲染开销的共同来源。

建议：折叠状态按视觉行/字符预算截取，保留头部、失败证据和展开入口；不要先完整物化巨量行再只取三行。

### F13 · P2 · pager 临时文件会暴露给同机其他用户

位置：[write_temp_file](../../src/app/pager.rs#L52)、[真实系统临时目录调用](../../src/terminal/agent/mod.rs#L1024)。

文件使用独占创建，但没有限制权限；常见 umask 022 下实际为 0644。生产路径写在系统临时目录，内容含思考/会话正文，其他本地用户可能在 pager 打开期间读取；崩溃或恢复终端失败还可能留下文件。

复现：`audit_pager_temp_file_must_be_private`，实测权限 644。测试使用独立临时目录和假正文，未读取其他用户文件。

建议：Unix 创建时直接指定 0600，使用私有临时目录/可靠临时文件管理，保证异常路径清理；Windows 使用对应用户私有目录与权限。独占创建解决覆盖风险，但不解决保密性。

## 另有三处需要收敛的静态风险

以下由代码路径确认风险条件，未用真实 daemon 做端到端复现，优先级暂定 P2：

1. **订阅任务竞态**：[set_tracked_session_ids](../../src/runtime.rs#L470) 每次 diff 独立 spawn，激活内部先 await HTTP，再创建流。快速 A→B 时，撤销 A 可能在 A 尚未建流时完成，迟到的 A 激活随后又创建流；当前 wanted 已只有 B，后续相同集合不再补一次撤销。`activate_with_attach_retry` 还会继续重试且忽略传入 generation。应由可取消的协调任务按最新 wanted 管理订阅，而不是让旧任务无条件落地。
2. **切目标失败没有真正恢复旧连接**：[reconnect_to](../../src/runtime.rs#L397) 失败只还原 target；[rebuild_inner](../../src/runtime.rs#L520) 已调用旧 client.close()。新建失败时保留的是已关闭的旧 client，界面仍可能停留 ready。随后 [RemoteReconnect handler](../../src/app/remote_ops.rs#L338) 在成功、失败两种情况下都发恢复命令。应先验证新连接，失败保持旧连接，或明确进入 Lost 并提供重连。
3. **凭据来源切回 BYOK 没有还原路由**：[Credential cycle](../../src/app/settings.rs#L877) 去账号档会写官方 baseUrl/wire，切回 api_key 的 `_ => {}` 不恢复原 BYOK 值。仅“切过去看一下再切回”后保存也会改变原端点。应保留各来源路由草稿或提供明确的路由变更提示。

## 设置与后端逐项核对

以当前本地后端的 `ConfigDto`、`ConfigPatch::validate`、`qaqh-config::apply_patch` 与运行时消费点为准，没有把历史注释当作接口事实。

| 页面字段/入口 | 当前后端状态 | 审计结论 |
| --- | --- | --- |
| credential、账号登录 | 支持三种 credential，siwc/dsaccount 服务仍在 | 有效；Token 粘贴与 Windows 打开 URL 问题见 F01/F02 |
| baseUrl、wire、model、apiKey | 支持 | 有效；`openai`/`anthropic` 仍是合法别名，不是失效字段 |
| maxTokens、contextLength、reasoningEffort、autoCompactThreshold | 支持；ultra、阈值 0 合法 | 有效；contextLength 只读全局配置，无法反映会话 Profile 覆盖值 |
| sandbox.enabled | 当前本地后端支持 | 有效；发布固定后端不含该字段，见 F04 |
| permissionLevel 1–4 | 当前 ConfigPatch 支持；TUI 走 config.save | 四档写路径正确，不能因为旧独立写口只收 1–3 就判页面失效 |
| Profile | 支持 profile.apply | 这是全局 Profile；后端另有 session.set_profile，TUI 没有接入入口 |
| exec.defaultShell、sessionIdleUnloadSecs | 支持，0 秒禁用卸载 | 有效 |
| 子代理 model/baseUrl/apiKey/maxTokens/timeoutSecs/defaultTools | 配置读写与子代理消费点仍在 | 没有发现废弃字段；不能仅在 runtime crate 内搜不到 defaultTools 就认为无效，实际消费在 qaqh-subagent |
| complianceEnabled、tokenizerPath | 支持 | tokenizer 重启生效提示正确；空值实际是启发式估算，页面“跟随系统”不够准确 |
| TUI performance_mode | 本地配置独立保存 | 分离正确；不混入 daemon patch |

尚未完全接入的后端能力包括：子代理 maxDepth、messageInFlightPerPair、messageOutboundPerSender 的设置入口，会话级 Profile，以及 MCP/LSP 配置的查看能力。MCP/LSP 在当前 ConfigPatch 本身没有写口，不能把缺少编辑入口单纯归咎于 TUI。

另有过时注释：设置页仍描述“wire 写侧只接受旧拼写”，当前后端已经同时接受 canonical 与别名；README 仍声称存在 `scripts/static-gates.sh`，实际目录没有该脚本，不能把这些门禁视为已经执行。

## 性能与 UI 收敛建议

已存在有效优化：块行 Arc 共享、窗口输出、四会话渲染缓存 LRU、普通更新差分绘制、性能模式关闭装饰动效。以下仍是可从代码确认的热点，未给出实际 CPU 百分比：

1. **流式长正文反复全量处理**：[render_assistant](../../src/ui/v2/transcript.rs#L560) 每个变化块都重新 sanitize、解析 Markdown、折行；[code.rs](../../src/ui/v2/markdown/code.rs#L39) 每次重建高亮状态并处理整段代码。固定小片追加时，累计工作量接近平方级。性能模式目前只减动画，并未绕开这条路径。
2. **队列无界且没有每轮处理预算**：[主循环](../../src/terminal/agent/mod.rs#L66) 两级 unbounded channel，[try_recv 循环](../../src/terminal/agent/mod.rs#L291) 持续抽到空。持续高产出时可能积压内存，或延迟重绘和按键反馈。应给可靠事件保序、给 replaceable 状态合并，并给处理批次/绘制设置预算；不应粗暴丢 timeline 事件。
3. **全历史元数据与待办反复扫描**：[fullscreen_sources](../../src/ui/v2/adapter.rs#L191) 随模型变化扫描历史块；[current_todo](../../src/ui/v2/adapter.rs#L518) 每次 timeline.version 变化从头折叠历史 todo 参数。普通文本增量也会触发待办扫描与 JSON 解析，增长会话中成本持续上升。
4. **模型内存没有字节预算**：四会话 LRU 限制的是渲染缓存，已经加载的 SessionState/timeline 不受此限。历史分页、多个标签和长思考正文仍可持续增长。需要可见的分页/逐出机制，不能静默删回合。
5. **空闲仍有定时唤醒**：输入线程 poll 10ms，维护 Tick 200ms；当前不能宣传严格“零空闲 CPU”。不建议在没有采样前为追求零唤醒直接改动终端读锁行为。

消息区冗余的判断：已有成功查询分组、摘要去重、live reasoning 避免双份显示等设计，应保留。主要可改进的是：

- 优先修 F09–F12，让失败、预览、截断可信，再简化视觉。
- 待办在历史工具卡与 composer 上方固定面板都展示属于“历史 + 当前状态”两种用途；但固定面板从历史参数重放，Workspace 从 daemon dashboard 读取，分页缺少早期 todo_write 时两者可不一致。建议固定面板改用 dashboard，同一当前状态源，历史卡保留变更摘要。
- 运行中工作状态在侧栏、thinking 行、status 行同时出现。可让侧栏保留多会话摘要，thinking 行保留当前阶段/工具，status 只保留连接、错误与用量，减少重复文字。
- 折叠工具成功输出按视觉行预算展示；错误与截断提示不能随精简消失。长 JSON/大 diff 的全文按需展开。
- 帮助、快捷键、目录/模型等信息按空间让位的现有策略总体合理，适合进一步做显式“紧凑显示”偏好。属于产品取舍，不能算作已经实测的视觉缺陷。

## 复现方式与处理顺序

```bash
python3 docs/audit-2026-10-23/run_repros.py
```

脚本临时在 `src/app/mod.rs` 注册审计模块，运行离线、单 job 的定向测试后在 finally 中移除声明，并保留运行期间其他源码编辑。当前预期退出 101；这些测试是“正确行为应成立”的审计断言，未修复前失败是证据。默认 `cargo test` 不包含它们。建议在没有其他编译/格式化任务同时访问该模块时运行。

建议处理顺序：F01/F02 输入与执行边界 → F03/F06/F07 失败恢复与草稿保护 → F04/F05 发布与远端契约 → F08–F13 输出准确性及保密性 → 订阅协调、流式增量布局与队列预算 → 界面紧凑化。
