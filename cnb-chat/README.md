# cnb-chat —— A/B（及总工程师）协同通道的监听器与发言口

**为什么存在**：本仓的协同通道是 CNB 的 issue / PR 评论，但**没有任何推送机制**——
对方发言不会主动到达我们（agent）这边。没有监听器时，唯一的办法是每隔一会儿手动
`cnb issues list-comments`，既慢又容易漏。这个工具把「轮询 → 增量 → 落盘 + 上屏」
固化下来，让通道变得可用。

**设计约束**（照本仓既有纪律）：

1. **不接触凭据**——数据面全部走已认证的 `cnb` CLI（`-v` 拿原始 JSON），
   token 始终留在 cnb CLI 自己的存储里，本工具不读、不写、不落日志。
2. **增量而非重放**——每线程一个游标（`last_comment_id` + `updated_at`），
   游标落盘；同一事件只投递一次，进程重启不重放。
3. **失败不致命**——单轮 API 异常只告警并继续（监听循环不退出）；
   告警走 stderr，stdout 保持可解析。
4. **只用标准库**——Python 3.11+，无第三方依赖。

## 用法

```bash
cd <repo-root>

# 单轮增量（首次会打全量历史，之后只打新增）
python3 cnb-chat/cnb_chat.py poll --once

# 常驻监听：20s 一轮，只报最近 1 小时的事件，静默时打心跳
python3 cnb-chat/cnb_chat.py watch --interval 20 --since=-1h --verbose-idle

# 只关心某几个线程（注意引号，`#` 会被 shell 吃掉）
python3 cnb-chat/cnb_chat.py watch --threads '#22,PR#21'

# 发言
python3 cnb-chat/cnb_chat.py send --issue 22 --body-file /tmp/reply.md
python3 cnb-chat/cnb_chat.py send --pr 21 --body "…" --mention AnyBuddy

# 回看收件箱（append-only，也可直接 grep state/inbox.jsonl）
python3 cnb-chat/cnb_chat.py inbox --tail 20
python3 cnb-chat/cnb_chat.py whoami          # 当前 cnb 身份
python3 cnb-chat/cnb_chat.py selftest        # 离线自测（不触网）
```

`--since` 接受相对（`-30m` / `-2h` / `-1d`）与绝对（`2026-09-20` / ISO8601）。
`--threads` / `--since` **只影响投递**：游标照常前进，被滤掉的事件不会在下一轮重放。

## 自测

`selftest` 是**不触网**的纯逻辑用例（33 项；复核：`python3 cnb-chat/cnb_chat.py selftest | grep -cE '^  (ok|FAIL)'`
——这个数字手写会漂，改用例时请一并改这里）：增量 id 全序比较、投递过滤、`parse_since`、
渲染截断标注、游标往返与损坏退化，以及**读失败路径**（把最底层的 `cnb_json` 打桩成瞬时
失败，跑真实的 `fetch_*` → `_paged` → `poll_once` 全链路）。存在理由：本工具的失败模式
大多是**静默漏消息**而非报错，必须有可证伪的用例钉住边界。

已做的变异验证（把实现改坏，看用例是否变红）：

| 变异 | 结果 |
|---|---|
| 增量改回字典序比较（雪花 id 位数不同就错） | 2 项红 |
| `--threads` 改成子串包含匹配（`#2` 误命中 `#22`） | 2 项红 |
| 去掉长正文的截断标注（静默截断） | 1 项红 |
| `_new_comments` 回退成 `int()` 严格解析（增量与游标口径不一致） | 2 项红 |
| `_paged` 读失败时返回 `[]`（＝把「没读到」压成「真的没有」） | 7 项红 |

> 第一版用例在「`--threads` 子串匹配」上**没变红**（用例与实现同弱），补了前缀混淆
> 用例后才抓到——记在这里，因为「不可证伪的回归锁」正是本仓台账反复点名的问题。

**覆盖缺口（如实标注）**：`send` 的「只吸收自己的、别人的照常投递」这条路径
**不在 selftest 覆盖内**（它需要两个进程 + 真实网络）。它由一次并发联调验证：
回滚游标后同时跑两个 `poll`，只有一个拿到事件、无重复 seq。
读失败路径（单进程、可打桩）**已覆盖**——见上表最后两条变异。

## 事件面

| kind | 含义 |
|---|---|
| `issue_new` / `pr_new` | 新 issue / 新 PR（首轮会把存量全报一遍，属预期） |
| `issue_comment` / `pr_comment` | 评论 |
| `pr_review` | PR 评审（`list-pull-reviews`，与普通评论是两个面） |
| `issue_comment_edited` | 线程有改动但没有新评论（编辑或删除） |
| `issue_state` / `pr_state` | 状态变更；`pr_state` 同时带 `mergeable_state`（`merged`/`closed`/`push_closed` 等） |

事件同时落两处：**stdout**（人看，也可被 harness 的 `process check` 增量读取）与
`state/inbox.jsonl`（append-only JSONL，一条事件一行）。

## 状态文件

`state/`（已 gitignore）——**不要提交**：

- `cursor.json`：`self`（身份）、`repo`、`seq`、`issues{}`、`pulls{}` 的游标；
- `inbox.jsonl`：全部已投递事件。

游标损坏时按空游标处理并告警（会重放一次全量，不会静默丢事件）。
`poll --baseline` 只标定游标、不产出事件——用于「不想看历史，只关心之后」。

## MCP 适配器（已实现，**未注册**）

`cnb_chat.py mcp` 是一个 stdio MCP server，暴露三个工具：`chat_poll` / `chat_send` /
`chat_inbox`。已实测 `initialize` / `tools/list` / `tools/call` / 未知方法报错四条链路。

**当前故意不启用**：本机 `~/.config/qaqh/config.toml` 是 `[mcp] enabled = false`，
注册需要同时改 `enabled` 与 `[mcp.servers.<name>]`（stdio + `command`/`args`），
并让 daemon 重新加载配置——那会动到运行中的 harness 本身，收益（少一次
`exec` 调用）远小于风险。**在脚本形态够用之前不动它**；等确认 harness 侧的
MCP 生命周期（idle 回收、并发上限 16、审批）都稳了再注册。

## 已知边界

- **多进程安全靠文件锁**：`state_lock()`（`flock`）把「读游标 → 扫描 → 投递 → 写游标」
  整段串行化，且**每轮重新读盘**而非内存常驻。没有它，常驻 `watch` 与一次性
  `send` / `poll` 会各持一份内存游标，导致重复投递与 seq 冲突（实测撞到过）。
  非 POSIX 平台无 `fcntl`，降级为无锁并告警。
- **`send` 的语义**：发出后立即「吸收」自己那条——**写进 `state/inbox.jsonl` 但不打屏**，
  避免下一轮把它当新消息回放；同一时刻别人发的新消息**照常投递**，不会被吞。
- **轮询而非推送**：CNB 侧没有可用的仓库动态接口（`cnb event get-events` 对各日期
  格式均返回 404，实测），所以只能轮询。默认 20s 一轮 = 每轮 2~3 个列表调用，
  对平台配额无压力；**不要把它设成秒级**。
- **评论编辑的检测是线程级的**：只有「线程 updated_at 变了但没有新评论」时才会报
  `*_comment_edited`，不区分是哪一条被编辑、也不给 diff；且**与同轮的 `*_state` 互斥**
  （否则「关一个 issue」会同时产出两条，其中一条是误导）。要看内容直接读 inbox 前后两条。
  另：**PR 评论的编辑/删除是静默的**（与 issue 面不对称）—— PR 面只推进 `updated_at`、
  不产事件，要看内容同样直读 inbox 前后两条。
- **「删一条 + 加一条」且计数与 `updated_at` 都不变时，新评论会静默漏**（2026-09-20 实测复现）：
  游标 `last_comment_id` 用 `max(...)` **单调推进、从不回退**，而 `dirty` 只看
  `comment_count` 与 `updated_at`。若对面删掉一条、又加一条使计数恰好不变、且平台**没有**
  推进 `updated_at`，评论面**根本不会重拉** → 新评论不会被看到，**且无告警**。
  是否触发取决于平台在删除评论时是否推进 `updated_at`（**未实测**）。
  修法（未做，另立 **W-12**）：给 `dirty` 加一条「每 N 轮兜底重拉一次评论面」。
- **读失败不推进游标**：`fetch_*` 把「本轮没读到」与「真的为空」严格区分（前者返回
  `None`）；读失败时**不写**计数/游标/`updated_at`，于是下一轮 `dirty` 仍为真、自动重拉。
  代价是读失败期间事件**延后**投递（而非静默丢失）。
- **非数字 id 的排序边界**：id 的全序键把数字 id 排在非数字 id 之前（真实 CNB id 是
  雪花数字，非数字只出现在合成数据里）。因此游标一旦是数字 id，非数字 id 就不再算「新」。
  增量判定与游标推进**共用同一个键**，否则会出现「每轮判新、却永远进不了游标」的重复投递。
- **`issues list-issues` 不接受 `--state all`**（实测 400），实现里改成 open/closed
  各拉一次再合并；PR 接口则接受 `--state all`。两面都走 `_paged()` 翻页（`totalPages`）。
- **`--threads` 是字面量相等，不做解析**：`--threads '#22, 22'` 里的 `22` 永不匹配，
  `--threads 21` 也匹配不到 `PR#21`。请写完整形如 `#22` / `PR#21`。
- **首次运行会打全量历史**（存量 issue/PR/评论/评审）。想跳过用 `--baseline` 先标定。
- 事件顺序按「扫描顺序」而非严格时间序（跨线程并发时可能交错）；单线程内严格有序。

## 与 agent 的配合方式

监听器本身不会「叫醒」agent，所以实际用法是：

1. 后台起 `watch`（harness 里用 `exec` + `background_after_secs`）；
2. 需要看有没有新消息时，读该后台进程的输出（`process check`）——**增量返回**，
   等价于「看一眼聊天窗口」；
3. 要回话就用 `send`。

这样一次 `check` 就能拿到所有新消息，不必为每条消息跑一次 `cnb` 命令。
