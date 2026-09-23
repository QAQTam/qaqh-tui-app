# QAQH TUI v2 v1/v2 Parity Matrix

> 状态：**V2-M6.2 已完成；M6.3 进行中：Workspace/resize/高频 resize/会话切换/自动重连已通过，permission/ask/plan 肯定+否定路径已通过，完整终端矩阵待补**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：
> [`2026-09-20-v2视觉token-spec.md`](2026-09-20-v2视觉token-spec.md) ·
> [`2026-09-20-v2终端提交协议-spec.md`](2026-09-20-v2终端提交协议-spec.md) ·
> [`2026-09-20-v2-agent-view-wireframe-spec.md`](2026-09-20-v2-agent-view-wireframe-spec.md)

---

## 0. 目的

v2 可以在视觉与终端模型上重构，但不能静默降低 QAQH 的核心能力。
本矩阵定义：

- 哪些能力必须与 v1 等价；
- 哪些能力有意改变交互方式；
- 哪些能力允许延后；
- 每个能力的验收与回滚口径。

---

## 1. 结论

| 类别 | 规则 |
|---|---|
| Must parity | 功能必须可用，行为可不同，但结果与数据不得丢失 |
| Intentionally changed | 交互入口/呈现方式改变，需在帮助与 release note 说明 |
| Deferred | 允许 v2 首版缺失，但必须有明确入口或提示 |
| Removed | 不迁移，需机主确认 |

---

## 2. 核心流程

| ID | 流程 | v1 行为 | v2 目标 | 类别 | 验收 |
|---|---|---|---|---|---|
| P-01 | 启动 | alternate screen 全屏 | inline viewport + scrollback | Intentionally changed | 退出后历史保留 |
| P-02 | 新建会话 | Ctrl+N | Ctrl+N / Workspace | Must parity | 创建成功、可发送 |
| P-03 | 切换会话 | tab bar / Alt+数字 | Workspace / 命令面板 | Intentionally changed | PTY 切换 + purge/replay 通过 |
| P-04 | 发送消息 | composer Enter | composer Enter | Must parity | 用户 prompt 提交一次 |
| P-05 | 流式回复 | 全屏重绘 | inline live viewport | Intentionally changed | 不写 scrollback，seal 后提交 |
| P-06 | 工具调用 | 工具卡 | ToolBlock / ToolGroup | Intentionally changed | 最终态提交，失败内联 |
| P-07 | 思考 | ActivityBar + Ctrl+T | ThinkingLine + Ctrl+T | Intentionally changed | 实时可见，seal 后聚合 |
| P-08 | 权限确认 | modal | alternate-screen modal | Intentionally changed | fake provider + 真实 daemon PTY：modal 可见 + 批准通过；**拒绝路径已补**（`MODE=permission-deny`，判据=后端 timeline `state=failed` + `[DENIED]`） |
| P-09 | ask_user | modal | 阻塞式单题分页 Modal | Intentionally changed | fake provider + 真实 daemon PTY：modal 可见 + 1-based 应答通过；**跳过路径已补**（`MODE=ask-dismiss`，判据=回合 `cancelled` + 账本 `decision=dismissed`） |
| P-10 | plan review | modal | alternate-screen modal | Intentionally changed | **PTY 已补**（`MODE=plan`：modal 可见 + `a` 批准通过）；**拒绝路径已补**（`MODE=plan-reject`：`r`+理由+Enter，判据=账本 `decision=rejected` 哈希）；钩子不物化 timeline 块，转写渲染待后端（见 M6.3 报告 §5） |
| P-11 | 子代理观测 | Ctrl+↑/↓ 视图栈 | Workspace 子代理视图 | Intentionally changed | 只读观测、返回父会话路由通过 |
| P-12 | workspace/todo | 右侧栏 | Workspace View | Intentionally changed | F4/`/workspace` 路由与 CJK 渲染通过 |
| P-13 | 导出 | `/export` | `/export` | Must parity | Markdown 内容完整 |
| P-14 | 加载更早 | PgUp + banner | Workspace / 命令入口 | Intentionally changed | 高水位过滤通过；旧页不倒灌 scrollback |
| P-15 | 手动重连 | Ctrl+R | Ctrl+R | Must parity | v2 auto + manual Ctrl+R PTY 均通过 |
| P-16 | 退出 | Ctrl+C×2 / Ctrl+Q | 同 v1 | Must parity | PTY 退出码 0，恢复终端 |
| P-17 | v1 回退 | 默认入口 | `--v1` 显式覆盖 | Must parity | 单元测试锁定 CLI > env > 默认优先级 |

---

## 3. 数据与契约

| ID | 能力 | v1 | v2 | 验收 |
|---|---|---|---|---|
| D-01 | qaqh.Ringing v1 | 使用 | 不变 | 不改协议 |
| D-02 | timeline 唯一真源 | 使用 | 不变 | 不本地猜测历史 |
| D-03 | display 投影 | 使用 | 不变 | 缺失时 H16 回退 |
| D-04 | progress bytes/stream | 使用 | 不变 | 运行卡仍显示 |
| D-05 | B1 丢弃可见 | 使用 | 不变 | 折叠/截断有计数 |
| D-06 | offload | 预览壳 | 预览壳 + 重放标注 | 不伪造历史 |
| D-07 | 后端锚点 | `50d3dc1dcfef` | `5ec1900d6c937b6`（2026-09-23 刷新） | 视觉切片不改后端 |

---

## 4. 视觉与交互

| ID | 项目 | v1 | v2 | 类别 |
|---|---|---|---|---|
| V-01 | 主题 | 硬编码 ANSI | 语义 token + 3 主题 | Intentionally changed |
| V-02 | 颜色降级 | 无 | truecolor / 256 / 16 / NO_COLOR | Must parity+ |
| V-03 | 边框 | 多处 `Borders::ALL` | accent rail + 轻边框 | Intentionally changed |
| V-04 | 多 tab | 常驻 | 单活动 + Workspace | Intentionally changed |
| V-05 | sidebar | 常驻可开关 | Workspace View | Intentionally changed |
| V-06 | status | 1 行密集 | 1 行分层 + shortcuts | Intentionally changed |
| V-07 | composer | 标题塞提示 | 轻边框 + 内联元信息 | Intentionally changed |
| V-08 | markdown | 基础 | 统一 md token | Intentionally changed |
| V-09 | 动画 | 全屏缓存槽位 | 仅 live viewport | Intentionally changed |
| V-10 | CJK | 已处理 | 继续保证 | Must parity |

---

## 5. 终端行为

| ID | 行为 | v1 | v2 | 验收 |
|---|---|---|---|---|
| T-01 | 滚动 | App 自绘 | 终端 scrollback | 原生 PgUp/PgDn 可用 |
| T-02 | 滚动条 | App 自绘 | 终端原生 | 不显示自绘滚动条 |
| T-03 | 选择/复制 | 鼠标捕获影响 | 默认不捕获 | 原生选择可用 |
| T-04 | resize | 全屏重算 | inline + 重放协议 | 80 次高频高度 resize PTY 通过 |
| T-05 | 主题切换 | 全屏重绘 | viewport 即时；历史可选重放 | 行为有提示 |
| T-06 | 重连 | 全量重绘 | ledger 幂等提交 | daemon kill/restart 后自动恢复 ready |
| T-07 | 退出 | 离开 alternate screen | 保留 scrollback | PTY 恢复 terminal；逐字节历史断言待补 |
| T-08 | tmux/SSH | 可用 | 兼容矩阵覆盖 | tmux/screen/SSH 环境分支通过；真实链路待测 |
| T-09 | `$PAGER` 挂起/恢复 | 全屏 restore/re-init | inline viewport 恢复 | 真实 daemon + PTY 通过 |

---

## 6. 性能与资源

| ID | 指标 | v1 基线 | v2 目标 |
|---|---|---|---|
| R-01 | 首帧 | 现有基线 | 不劣化 | 达标：v1 31.4ms / v2 lazy 6.6ms |
| R-02 | 流式 delta | 只重绘活动块 | 只重绘 live viewport | 基准：V2 sync 8µs/帧 |
| R-03 | 长会话 UI 驻留 | 虚拟化 + 估算 | 显著下降 | 达标：440 回合 v2 runtime steady 545KB，v1 cache 2297KB |
| R-04 | 历史滚动 | App 窗口 | 终端负责 |
| R-05 | resize 重放 | 无 | 已有 80 次 PTY 压力；定量基准待补 |
| R-06 | commit 开销 | 无 | 批量 + 背压 | 已有 v2 runtime 基准；无新增同步 1µs |

---

## 7. 迁移门禁

### Gate A：M1 原型通过

- inline viewport 可启动/退出；
- 已提交文本进入 scrollback；
- 重连不重复；
- resize 不崩、不错位到不可用；
- v1 仍可回退。

### Gate B：M2-M3 视觉通过

- token 覆盖 v2 UI；
- transcript 核心流程 parity；
- CJK、NO_COLOR、16 色快照通过；
- 性能不劣化。

### Gate C：M4-M5 体验通过

- composer/status/shortcuts 完整；
- Workspace/Modal 不污染 scrollback；
- 会话切换、权限、ask、plan、子代理可用。

### Gate D：M6-M7 切换

- 全门禁绿；
- parity matrix 全项有结论；
- 回滚路径验证；
- v1 滚动路径可删除。

---

## 8. 明确不迁移

| 项目 | 原因 | 替代 |
|---|---|---|
| v1 自绘滚动条 | 终端原生 scrollback | 终端滚动 |
| v1 历史高度估算 | 不再计算历史高度 | commit 协议 |
| v1 离屏淘汰/窗口 | scrollback 拥有历史 | 终端 |
| v1 常驻 tab bar | 单活动会话模型 | Workspace |
| v1 常驻 sidebar | 单列阅读优先 | Workspace |
| v1 卡片级展开 | W-02 已裁决删除 | 固定窗口 + export |
| Grok 品牌/logo/文案 | 品牌与许可边界 | QAQH 自有视觉 |

---

## 9. 验收清单

- [ ] P-01 ~ P-16 全部有测试或人工记录
- [ ] D-01 ~ D-07 全部满足
- [ ] V-01 ~ V-10 全部满足
- [ ] T-01 ~ T-08 全部满足
- [ ] R-01 ~ R-06 全部达标
- [ ] Gate A ~ Gate D 全部通过
- [ ] v1 回退路径验证
- [ ] 文档与 release note 更新
