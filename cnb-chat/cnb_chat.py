#!/usr/bin/env python3
"""cnb-chat — CNB 协同通道的监听器 / 发言口（工程师 A ↔ B ↔ 总工程师）。

设计要点
--------
* 数据面：已认证的 `cnb` CLI（加 `-v` 拿原始 JSON）。本工具**不接触 token**、
  不把任何凭据写盘——鉴权全部委托给 cnb CLI 自己的存储。
* 事件面：issue（新建 / 评论 / 评论编辑 / 状态变更）+ PR（新建 / 评论 / 评审 /
  状态与合并态变更）。用**每线程游标**（last_comment_id + updated_at）做增量，
  不做全量重放。
* 两个出口：stdout（给人看，也可被 harness 的 `process check` 增量读取）
  + `state/inbox.jsonl`（append-only，可用 `inbox` 子命令回看）。
* 幂等：同一事件只发一次；游标落盘，重启不重放（`--baseline` 可强制标定）。

用法
----
    cnb_chat.py poll --once                  # 单轮增量，打印新事件
    cnb_chat.py watch --interval 20          # 常驻监听
    cnb_chat.py send --issue 22 --body "…"   # 发言（issue）
    cnb_chat.py send --pr 21 --body-file x   # 发言（PR）
    cnb_chat.py inbox --tail 20              # 回看收件箱
    cnb_chat.py whoami                       # 当前 cnb 身份
    cnb_chat.py mcp                          # stdio MCP server（未注册，见 README）

退出码：0 正常；1 参数/环境错误；2 cnb CLI 不可用（监听循环不退出，只告警）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path

try:                                      # POSIX 下用于跨进程串行化（见 state_lock）
    import fcntl
except ImportError:                       # Windows：降级为无锁
    fcntl = None

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
STATE_DIR = Path(os.environ.get("CNB_CHAT_STATE", SCRIPT_DIR / "state"))
CURSOR_PATH = STATE_DIR / "cursor.json"
INBOX_PATH = STATE_DIR / "inbox.jsonl"

DEFAULT_REPO = "QAQ-Harness/qaqh-tui-app"
PAGE_SIZE = 100
BODY_LINE_CAP = 40          # stdout 里正文最多打多少行
BODY_CHAR_CAP = 6000
FETCH_PAUSE = 0.15          # 连续请求之间的礼貌间隔（秒）
CNB_TIMEOUT = 60


# ───────────────────────────── 基础设施 ─────────────────────────────

def warn(msg: str) -> None:
    """告警只走 stderr：stdout 必须保持可解析（供 --json / MCP 使用）。"""
    print(f"[cnb-chat] {msg}", file=sys.stderr, flush=True)


def cnb_json(args: list[str]) -> dict | None:
    """跑 `cnb <args> -v` 并把原始响应解析成 dict；失败返回 None（不抛）。"""
    try:
        proc = subprocess.run(
            ["cnb", *args, "-v"],
            capture_output=True, text=True, timeout=CNB_TIMEOUT,
        )
    except FileNotFoundError:
        warn("找不到 cnb CLI（PATH 里没有）")
        return None
    except subprocess.TimeoutExpired:
        warn(f"cnb 超时（>{CNB_TIMEOUT}s）: {' '.join(args)}")
        return None

    out = (proc.stdout or "").strip()
    if not out.startswith("{"):
        detail = (out or proc.stderr or "").strip().splitlines()
        warn(f"cnb 非 JSON 输出（exit={proc.returncode}）: {detail[0] if detail else '(空)'}")
        return None
    try:
        payload = json.loads(out)
    except json.JSONDecodeError as exc:
        warn(f"cnb JSON 解析失败: {exc}")
        return None
    if isinstance(payload, dict) and payload.get("status", 200) >= 400:
        warn(f"cnb 返回 {payload.get('status')}: {json.dumps(payload.get('data'), ensure_ascii=False)[:200]}")
        return None
    return payload


def repo_slug() -> str:
    if os.environ.get("CNB_REPO_SLUG"):
        return os.environ["CNB_REPO_SLUG"]
    try:
        url = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "remote", "get-url", "origin"],
            capture_output=True, text=True, timeout=10,
        ).stdout.strip()
        if url:
            tail = url.removesuffix(".git").rstrip("/")
            parts = tail.split("/")
            if len(parts) >= 2:
                return "/".join(parts[-2:])
    except (OSError, subprocess.SubprocessError):
        pass
    return DEFAULT_REPO


def whoami() -> str | None:
    payload = cnb_json(["users", "get-user-info"])
    if not payload:
        return None
    return (payload.get("data") or {}).get("username")


def load_cursor() -> dict:
    if CURSOR_PATH.exists():
        try:
            return json.loads(CURSOR_PATH.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            warn(f"游标损坏，按空游标处理: {exc}")
    return {"self": None, "repo": None, "seq": 0, "issues": {}, "pulls": {}}


def save_cursor(cur: dict) -> None:
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    tmp = CURSOR_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(cur, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(CURSOR_PATH)


def append_inbox(event: dict) -> None:
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    with INBOX_PATH.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(event, ensure_ascii=False) + "\n")


@contextmanager
def state_lock():
    """跨进程串行化「读游标 → 扫描 → 投递 → 写游标」。

    没有它，常驻 `watch` 与一次性命令（`send` / `poll`）共享同一 state 目录时
    会各自持一份内存游标：重复投递、seq 冲突都会出现（实测撞到过）。
    非 POSIX 平台（无 fcntl）降级为无锁并告警。
    """
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    if fcntl is None:
        warn("当前平台无 fcntl，跳过状态锁（多进程并发时不安全）")
        yield
        return
    with (STATE_DIR / "lock").open("w") as fh:
        fcntl.flock(fh, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fh, fcntl.LOCK_UN)


def load_state(repo: str | None) -> dict:
    """读游标并补齐 repo/self（**调用方必须已持锁**）。"""
    cur = load_cursor()
    cur["repo"] = repo or cur.get("repo") or repo_slug()
    if not cur.get("self"):
        cur["self"] = whoami()
    return cur


def local_time(iso: str) -> str:
    try:
        dt = datetime.fromisoformat(iso.replace("Z", "+00:00"))
        return dt.astimezone().strftime("%m-%d %H:%M")
    except (ValueError, AttributeError):
        return iso or "?"


# ───────────────────────────── 数据面 ─────────────────────────────

def _paged(argv: list[str]) -> list[dict] | None:
    """翻页拉全（`totalPages`）。**任一夜读失败即返回 None**——调用方据此不得推进游标。

    返回 `[]` 与返回 `None` 是两件不同的事：前者是「真的没有」，后者是「本轮没读到」。
    把两者压成同一个值，正是「瞬时失败 → 游标照推 → 静默漏事件」的入口。
    """
    out: list[dict] = []
    page = 1
    while True:
        payload = cnb_json([*argv, "--page", str(page), "--page-size", str(PAGE_SIZE)])
        if payload is None:
            return None
        batch = payload.get("data") or []
        out.extend(batch)
        if page >= int(payload.get("totalPages") or 1) or not batch:
            return out
        page += 1
        time.sleep(FETCH_PAUSE)


def fetch_issues(repo: str) -> list[dict] | None:
    """issues 接口不接受 `--state all`（实测 400），只能 open/closed 各拉一次再合并。"""
    merged: dict[str, dict] = {}
    for state in ("open", "closed"):
        batch = _paged(["issues", "list-issues", "--repo", repo,
                        "--state", state, "--order-by", "-updated_at"])
        if batch is None:
            return None
        for item in batch:
            merged[str(item.get("number"))] = item
        time.sleep(FETCH_PAUSE)
    return list(merged.values())


def fetch_issue_comments(repo: str, number: str) -> list[dict] | None:
    return _paged(["issues", "list-issue-comments", "--repo", repo,
                   "--number", str(number), "--sort", "created"])


def fetch_pulls(repo: str) -> list[dict] | None:
    return _paged(["pulls", "list-pulls", "--repo", repo,
                   "--state", "all", "--order-by", "-updated_at"])


def fetch_pull_comments(repo: str, number: str) -> list[dict] | None:
    return _paged(["pulls", "list-pull-comments", "--repo", repo, "--number", str(number)])


def fetch_pull_reviews(repo: str, number: str) -> list[dict] | None:
    """评审（review）与普通评论是两个面；失败返回 None（调用方不得据此推进游标）。"""
    return _paged(["pulls", "list-pull-reviews", "--repo", repo, "--number", str(number)])


# ───────────────────────────── 事件抽取 ─────────────────────────────

def _author(node) -> str:
    if isinstance(node, dict):
        return node.get("username") or node.get("nickname") or "?"
    return str(node or "?")


def _comment_id(node: dict) -> str:
    return str(node.get("id") or "")


def _new_comments(comments: list[dict], last_id: str) -> list[dict]:
    """按 `_id_key` 全序取增量——**与游标推进同一个键**。

    口径不一致（这里 `int()` 严格解析、游标那边 `_id_key()` 容错）会让非数字 id
    每轮都被判为新、却永远进不了游标 → 每轮重复投递。
    """
    floor = _id_key(last_id)
    return [c for c in comments if _id_key(_comment_id(c)) > floor]


def _make_event(cur: dict, kind: str, thread: str, *, author: str, body: str = "",
                url: str = "", ts: str = "", cid: str = "") -> dict:
    cur["seq"] = int(cur.get("seq", 0)) + 1
    return {
        "seq": cur["seq"],
        "ts": ts or datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "kind": kind,
        "thread": thread,
        "author": author,
        "self": author == cur.get("self"),
        "cid": cid,
        "url": url,
        "body": body,
    }


def poll_once(cur: dict, repo: str, *, baseline: bool = False) -> list[dict]:
    """单轮增量扫描。就地更新 cur；返回本轮新事件（baseline=True 时只标定不产出）。"""
    events: list[dict] = []
    base = f"https://cnb.cool/{repo}/-/"

    # 读失败（None）＝本轮这一面不投递、**不写任何游标**，下一轮自动重试；
    # 与「真的为空」严格区分（否则瞬时失败会静默吞掉事件）。
    for iss in fetch_issues(repo) or []:
        number = str(iss.get("number"))
        prev = cur["issues"].get(number)
        first_seen = prev is None
        prev = prev or {}
        if first_seen and not baseline:
            events.append(_make_event(
                cur, "issue_new", f"#{number}", author=_author(iss.get("author")),
                body=iss.get("title") or "", url=f"{base}issues/{number}",
                ts=iss.get("created_at") or "",
            ))

        comment_count = iss.get("comment_count")
        updated_at = iss.get("updated_at") or ""
        state = iss.get("state")
        state_changed = bool(not first_seen and prev.get("state") and prev["state"] != state)
        dirty = first_seen or prev.get("comments") != comment_count or prev.get("updated_at") != updated_at
        if dirty:
            comments = fetch_issue_comments(repo, number)
            if comments is None:
                # 读失败：不写 comments / last_comment_id / updated_at
                # ⇒ 下一轮 `dirty` 仍为真，自动重拉。
                pass
            else:
                fresh = _new_comments(comments, prev.get("last_comment_id", ""))
                if fresh:
                    if not baseline:
                        for c in fresh:
                            events.append(_make_event(
                                cur, "issue_comment", f"#{number}",
                                author=_author(c.get("author")), body=c.get("body") or "",
                                url=f"{base}issues/{number}", ts=c.get("created_at") or "",
                                cid=_comment_id(c),
                            ))
                    prev["last_comment_id"] = max((_comment_id(c) for c in comments), key=_id_key, default="")
                elif not first_seen and not baseline and prev.get("updated_at") != updated_at \
                        and not state_changed:
                    # 没有新评论但 updated_at 跳了：可能是编辑/删除，也可能是标签/处理人/标题
                    # 等元数据变更——本接口分不开，所以文案放宽；并与同轮的 issue_state 互斥，
                    # 否则「关一个 issue」会产出两条，其中一条是误导。
                    events.append(_make_event(
                        cur, "issue_comment_edited", f"#{number}", author="(未知)",
                        body="线程有改动但没有新评论（编辑/删除，或标签/处理人等元数据变更）",
                        url=f"{base}issues/{number}", ts=updated_at,
                    ))
                prev["comments"] = comment_count
                prev["updated_at"] = updated_at

        if state_changed and not baseline:
            events.append(_make_event(
                cur, "issue_state", f"#{number}", author="(平台)",
                body=f"{prev['state']} → {state}", url=f"{base}issues/{number}", ts=updated_at,
            ))
        prev["state"] = state
        cur["issues"][number] = prev

    for pr in fetch_pulls(repo) or []:
        number = str(pr.get("number"))
        prev = cur["pulls"].get(number)
        first_seen = prev is None
        prev = prev or {}
        title = pr.get("title") or ""
        if first_seen and not baseline:
            events.append(_make_event(
                cur, "pr_new", f"PR#{number}", author=_author(pr.get("author")),
                body=title, url=f"{base}pulls/{number}", ts=pr.get("created_at") or "",
            ))

        comment_count = pr.get("comment_count")
        review_count = pr.get("review_count")
        updated_at = pr.get("updated_at") or ""

        comments_ok = True
        if first_seen or prev.get("comments") != comment_count or prev.get("updated_at") != updated_at:
            comments = fetch_pull_comments(repo, number)
            if comments is None:
                # 读失败：不写 comments/last_comment_id，也不推进 updated_at（见下方 comments_ok）。
                comments_ok = False
            else:
                fresh = _new_comments(comments, prev.get("last_comment_id", ""))
                if fresh:
                    if not baseline:
                        for c in fresh:
                            events.append(_make_event(
                                cur, "pr_comment", f"PR#{number}",
                                author=_author(c.get("author")), body=c.get("body") or "",
                                url=f"{base}pulls/{number}", ts=c.get("created_at") or "",
                                cid=_comment_id(c),
                            ))
                    prev["last_comment_id"] = max((_comment_id(c) for c in comments), key=_id_key, default="")
                prev["comments"] = comment_count

        if first_seen or prev.get("reviews") != review_count:
            reviews = fetch_pull_reviews(repo, number)
            if reviews is None:
                # 读失败：**绝不**写 reviews / last_review_id——旧实现会把 last_review_id
                # 重置成 "" 并关上闸门，历史评审再也不会重放（评审 #23 阻断项 1）。
                pass
            else:
                floor = _id_key(prev.get("last_review_id") or "")
                if reviews and not baseline:
                    for r in reviews:
                        if _id_key(_comment_id(r)) <= floor:
                            continue
                        events.append(_make_event(
                            cur, "pr_review", f"PR#{number}", author=_author(r.get("author")),
                            body=json.dumps({k: v for k, v in r.items()
                                             if k in ("state", "body", "verdict")}, ensure_ascii=False),
                            url=f"{base}pulls/{number}", ts=r.get("created_at") or "",
                            cid=_comment_id(r),
                        ))
                prev["reviews"] = review_count
                prev["last_review_id"] = max((_comment_id(r) for r in reviews), key=_id_key, default="")

        state = pr.get("state")
        mergeable = pr.get("mergeable_state") or ""
        if not first_seen and prev.get("state") and (prev["state"] != state or prev.get("mergeable_state") != mergeable):
            if not baseline:
                events.append(_make_event(
                    cur, "pr_state", f"PR#{number}", author="(平台)",
                    body=f"{prev['state']}/{prev.get('mergeable_state') or '-'} → {state}/{mergeable or '-'}"
                         f"｜{title}",
                    url=f"{base}pulls/{number}", ts=updated_at,
                ))
        prev["state"] = state
        prev["mergeable_state"] = mergeable
        if comments_ok:
            prev["updated_at"] = updated_at
        cur["pulls"][number] = prev

    return events


def _id_key(value: str):
    """id 的全序键：数字 id 按数值排；非数字 id 退到字典序且**恒小于任何数字 id**。

    增量判定 / 游标推进 / 评审 floor 三处必须同用这一个键（见 `_new_comments`）。
    已知边界：游标一旦是数字 id，非数字 id 就不再算「新」——真实 CNB id 是雪花数字，
    该分支只在合成数据里出现；两条行为都由 selftest 钉住。
    """
    text = str(value or "")
    try:
        return (1, int(text), "")
    except (TypeError, ValueError):
        return (0, 0, text)


# ───────────────────────────── 渲染 ─────────────────────────────

ICON = {
    "issue_new": "🆕", "issue_comment": "💬", "issue_comment_edited": "✏️", "issue_state": "🔁",
    "pr_new": "🚀", "pr_comment": "💬", "pr_review": "🔍", "pr_state": "🔀",
}


def render(event: dict, *, color: bool = True) -> str:
    who = event.get("author") or "?"
    tag = f"{who}(我)" if event.get("self") else who
    head = (f"[{local_time(event.get('ts', ''))}] {ICON.get(event['kind'], '·')} "
            f"{event['thread']} {event['kind']} @{tag} #{event.get('seq')}")
    body = (event.get("body") or "").rstrip()
    if not body:
        return head
    lines = body.splitlines()
    shown = lines[:BODY_LINE_CAP]
    if len(lines) > BODY_LINE_CAP:
        shown.append(f"…（正文共 {len(lines)} 行，完整内容见 inbox.jsonl #{event.get('seq')}）")
    text = "\n".join("    " + ln for ln in shown)
    if len(text) > BODY_CHAR_CAP:
        text = text[:BODY_CHAR_CAP] + f"\n    …（截断，完整见 inbox.jsonl #{event.get('seq')}）"
    return f"{head}\n{text}"


def emit(events: list[dict], *, as_json: bool) -> None:
    for ev in events:
        append_inbox(ev)
        print(json.dumps(ev, ensure_ascii=False) if as_json else render(ev), flush=True)


# ───────────────────── 投递过滤器（游标照常前进，只筛“要不要告诉你”）─────────────────────

def parse_since(spec: str) -> datetime | None:
    """接受 `-30m` / `-2h` / `-1d`（相对）或 `2026-09-20` / ISO8601（绝对）。"""
    spec = (spec or "").strip()
    if not spec:
        return None
    rel = re.fullmatch(r"-(\d+)([smhd])", spec)
    if rel:
        scale = {"s": 1, "m": 60, "h": 3600, "d": 86400}[rel.group(2)]
        return datetime.now(timezone.utc) - timedelta(seconds=int(rel.group(1)) * scale)
    try:
        if re.fullmatch(r"\d{4}-\d{2}-\d{2}", spec):
            return datetime.fromisoformat(spec).replace(tzinfo=timezone.utc)
        return datetime.fromisoformat(spec.replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        warn(f"--since 无法解析，忽略: {spec!r}")
        return None


def parse_ts(ts: str) -> datetime:
    try:
        return datetime.fromisoformat((ts or "").replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        return datetime.fromtimestamp(0, timezone.utc)


def filter_events(events: list[dict], threads: str | None, since: str | None) -> list[dict]:
    """**只影响投递**：游标已在 poll_once 里前进，被滤掉的事件不会在下轮重放。"""
    if threads:
        want = {t.strip() for t in threads.split(",") if t.strip()}
        events = [e for e in events if e["thread"] in want]
    cutoff = parse_since(since) if since else None
    if cutoff:
        events = [e for e in events if parse_ts(e["ts"]) >= cutoff]
    return events


# ───────────────────────────── 子命令 ─────────────────────────────

def cmd_poll(args) -> int:
    if args.interval < 0:
        warn("--interval 不得为负")
        return 2
    if args.cmd == "watch" and args.interval < 1:
        # 旧行为：watch --interval 0 会静默退化成「只跑一轮」，与子命令名不符。
        warn("watch 的 --interval 必须 ≥ 1（0 会静默退化成只跑一轮）")
        return 2
    if args.interval and not getattr(args, "once", False):
        return watch_loop(args)
    with state_lock():
        cur = load_state(args.repo)
        events = filter_events(poll_once(cur, cur["repo"], baseline=args.baseline),
                               args.threads, args.since)
        if not args.baseline:
            emit(events, as_json=args.json)
        save_cursor(cur)
    if not args.baseline and not events:
        warn("本轮无新事件")
    return 0


def watch_loop(args) -> int:
    with state_lock():
        repo = load_state(args.repo)["repo"]
    warn(f"开始监听 {repo}（间隔 {args.interval}s，Ctrl-C 退出）")
    idle = 0
    while True:
        events: list[dict] = []
        try:
            # 每轮重新读游标（而非内存常驻）：常驻 watch 与一次性命令并存时，
            # 只有重新读盘才能看到对方已经吸收过的位置。
            with state_lock():
                cur = load_state(args.repo)
                events = filter_events(poll_once(cur, cur["repo"]), args.threads, args.since)
                if events:
                    emit(events, as_json=args.json)
                save_cursor(cur)
            if events:
                idle = 0
            else:
                idle += 1
                if args.verbose_idle and idle % 10 == 1:
                    warn(f"静默中（已 {idle} 轮无新事件）")
        except KeyboardInterrupt:
            warn("收到中断，退出")
            return 0
        except Exception as exc:                      # 单轮异常不得杀死监听
            warn(f"本轮扫描异常（继续）: {type(exc).__name__}: {exc}")
        try:
            time.sleep(args.interval)
        except KeyboardInterrupt:
            return 0


def cmd_send(args) -> int:
    body = args.body if args.body is not None else Path(args.body_file).read_text(encoding="utf-8")
    if args.mention:
        body = f"@{args.mention} {body}"

    with state_lock():
        cur = load_state(args.repo)
        repo = cur["repo"]
        if args.issue:
            cmd = ["issues", "post-issue-comment", "--repo", repo, "--number", str(args.issue)]
            thread = f"#{args.issue}"
        else:
            cmd = ["pulls", "post-pull-comment", "--repo", repo, "--number", str(args.pr)]
            thread = f"PR#{args.pr}"

        if cnb_json([*cmd, "--body", body]) is None:
            warn("发言失败（见上方 cnb 报错）")
            return 1

        # 「吸收」自己的这条评论，避免下一轮被回放——但**只吸收自己的**：
        # 若这期间别人刚好发了言，那些事件必须照常投递，否则会被静默吞掉。
        others: list[dict] = []
        try:
            events = poll_once(cur, repo)
            mine = [e for e in events if e.get("self")]
            others = [e for e in events if not e.get("self")]
            for ev in mine:
                append_inbox(ev)      # 记进 transcript，但不打屏（那是我自己刚发的）
            save_cursor(cur)
        except Exception as exc:
            warn(f"吸收时扫描异常（不影响已发出的评论）: {exc}")
        warn(f"已发往 {thread}；吸收自己的这条评论以免被回放")
        if others:
            warn(f"发言期间收到 {len(others)} 条别人的新消息，一并投递")
            emit(others, as_json=False)

    print(f"OK {thread} ({len(body)} 字符)", flush=True)
    return 0


def cmd_inbox(args) -> int:
    if args.tail < 0:
        # `lines[-(-3):]` = `lines[3:]`：负值会静默变成「去掉前 3 条」的反义行为。
        warn("--tail 不得为负（0=全部）")
        return 2
    if not INBOX_PATH.exists():
        warn("收件箱为空（还没跑过 poll）")
        return 0
    lines = INBOX_PATH.read_text(encoding="utf-8").splitlines()
    tail = lines[-args.tail:] if args.tail else lines
    for line in tail:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        print(json.dumps(ev, ensure_ascii=False) if args.json else render(ev))
    return 0


def cmd_whoami(args) -> int:
    name = whoami()
    if not name:
        return 2
    print(name)
    return 0


def cmd_selftest(args) -> int:
    """离线自测：**不触网**，只验纯逻辑（增量/过滤/渲染/游标往返）。

    存在的理由：本工具的失败模式大多是「静默漏消息」而不是报错，
    所以需要有可证伪的用例钉住这些边界。
    """
    failures: list[str] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        print(f"  {'ok  ' if ok else 'FAIL'} {name}{('  — ' + detail) if detail and not ok else ''}")
        if not ok:
            failures.append(name)

    # 1) 增量：只取游标之后的新评论（雪花 id 数值比较）。
    #    刻意用**不同位数**的 id：字典序比较会把 "9" 误判为比 "10" 新。
    comments = [{"id": "9"}, {"id": "10"}, {"id": "100"}]
    check("增量：只取游标之后", [c["id"] for c in _new_comments(comments, "10")] == ["100"])
    check("增量：位数不同不得按字典序（9 不得混入）",
          [c["id"] for c in _new_comments(comments, "100")] == [])
    check("增量：空游标取全部", len(_new_comments(comments, "")) == 3)
    # 非数字 id：**增量判定与游标推进必须用同一个键**。旧实现里 `_new_comments` 用
    # `int()` 严格解析（解不开就「视为新」）、游标推进用 `_id_key()` 容错（非数字恒最小），
    # 于是非数字 id「每轮判新、却永远进不了游标」→ 每轮重复投递。原 selftest 把这个
    # 错误当期望锁住了（`== 1`），所以变异测试也不会变红——本条已改正。
    check("增量：非数字 id 在空游标下必须投递（不得静默漏）",
          len(_new_comments([{"id": "x"}], "")) == 1)
    check("增量：游标为数字时非数字 id 不得每轮重投",
          _new_comments([{"id": "x"}], "300") == [])
    mixed = [{"id": "zzz"}, {"id": "100"}]
    check("增量：游标为数字时混合集不得重投", _new_comments(mixed, "100") == [])
    check("id 比较：数字 id 恒大于非数字 id（同一全序键）", _id_key("1") > _id_key("zzz"))
    check("游标推进：取的就是增量判定那个键",
          max((_comment_id(c) for c in mixed), key=_id_key, default="") == "100")
    check("id 比较：数值而非字典序", _id_key("99") < _id_key("100"))
    check("id 比较：空值退化为最小", _id_key("") < _id_key("1"))

    # 2) 投递过滤：threads 精确匹配（不误伤其它线程）
    evs = [{"seq": 1, "thread": "#22", "ts": "2026-09-19T16:00:00Z", "body": "a"},
           {"seq": 2, "thread": "PR#21", "ts": "2026-09-19T17:00:00Z", "body": "b"}]
    check("--threads 单值", [e["seq"] for e in filter_events(evs, "#22", None)] == [1])
    check("--threads 多值", [e["seq"] for e in filter_events(evs, "#22,PR#21", None)] == [1, 2])
    check("--threads 不匹配则为空", filter_events(evs, "#999", None) == [])
    # 前缀混淆：子串匹配（`#2` 命中 `#22`）与精确匹配必须区分得开——这条用例
    # 就是为了钉住实现不得退化成 `in` 包含匹配。
    check("--threads 精确匹配，不得被前缀命中", filter_events(evs, "#2", None) == [])
    check("--threads 精确匹配，不得被 PR 前缀命中", filter_events(evs, "PR#2", None) == [])
    check("--since 绝对时间截断", [e["seq"] for e in filter_events(evs, None, "2026-09-19T16:30:00Z")] == [2])
    check("--since 无过滤则全留", len(filter_events(evs, None, None)) == 2)

    # 3) parse_since：相对/绝对/非法
    check("parse_since 相对 -2h", parse_since("-2h") is not None)
    check("parse_since 绝对日期", parse_since("2026-09-19") is not None)
    check("parse_since 非法不抛", parse_since("不是时间") is None)

    # 4) 渲染：长正文必须带截断标注（不得静默截断）
    long_ev = _make_event({"seq": 0, "self": "QAQTam"}, "issue_comment", "#22",
                          author="AnyBuddy", body="\n".join(f"行{i}" for i in range(60)))
    out = render(long_ev)
    check("渲染：超长正文标注截断", "完整内容见 inbox.jsonl" in out)
    check("渲染：标注自己的发言", "(我)" in render(_make_event({"seq": 0, "self": "QAQTam"},
                                                          "issue_comment", "#22", author="QAQTam")))

    # 5) 游标往返（用临时目录，不碰真实 state）
    import tempfile
    global STATE_DIR, CURSOR_PATH, INBOX_PATH
    saved = (STATE_DIR, CURSOR_PATH, INBOX_PATH)
    try:
        with tempfile.TemporaryDirectory() as tmp:
            STATE_DIR = Path(tmp)
            CURSOR_PATH = STATE_DIR / "cursor.json"
            INBOX_PATH = STATE_DIR / "inbox.jsonl"
            cur = {"self": "QAQTam", "repo": "o/r", "seq": 7, "issues": {"22": {"comments": 3}}, "pulls": {}}
            save_cursor(cur)
            check("游标往返一致", load_cursor() == cur)
            append_inbox({"seq": 8, "body": "x"})
            append_inbox({"seq": 9, "body": "y"})
            check("收件箱 append 不覆盖", len(INBOX_PATH.read_text().splitlines()) == 2)
            CURSOR_PATH.write_text("{ 坏 JSON")
            check("游标损坏时退化为空游标", load_cursor().get("seq", 0) == 0)
    finally:
        STATE_DIR, CURSOR_PATH, INBOX_PATH = saved

    # 6) 读失败不得推进游标（把**最底层的 `cnb_json`** 打桩成瞬时失败，让真实的
    #    `fetch_*` → `_paged` → `poll_once` 全链路被覆盖——可证伪）
    saved_json = cnb_json

    def _iss(comment_count=5, updated_at="2026-09-19T18:00:00Z"):
        return {"number": "22", "comment_count": comment_count, "updated_at": updated_at,
                "state": "open", "title": "t", "author": {"username": "QAQTam"},
                "created_at": "2026-09-19T10:00:00Z"}

    def _pull(comment_count=2, review_count=2, updated_at="2026-09-19T10:00:00Z"):
        return {"number": "23", "comment_count": comment_count, "review_count": review_count,
                "updated_at": updated_at, "state": "open", "mergeable_state": "mergeable",
                "title": "t", "author": {"username": "AnyBuddy"},
                "created_at": "2026-09-19T10:00:00Z"}

    def _c(cid, body="x", ts="2026-09-19T18:00:00Z"):
        return {"id": cid, "author": {"username": "AnyBuddy"}, "body": body, "created_at": ts}

    down = {"issue_comments": True, "pull_comments": True, "reviews": True}  # 模拟「本轮没读到」
    # 两个面分开测：同时活着会让 `issue_new`/`pr_new`/`pr_comment` 混进断言。
    live = {"issues": True, "pulls": False}

    def _stub(argv: list[str]) -> dict | None:
        if "list-issues" in argv:
            return {"data": [_iss(comment_count=5)] if live["issues"] else [], "totalPages": 1}
        if "list-pulls" in argv:
            return {"data": [_pull()] if live["pulls"] else [], "totalPages": 1}
        if "list-issue-comments" in argv:
            if down["issue_comments"]:
                return None                              # 瞬时失败
            return {"data": [_c("100"), _c("200", "补投")], "totalPages": 1}
        if "list-pull-comments" in argv:
            if down["pull_comments"]:
                return None
            return {"data": [_c("500")], "totalPages": 1}
        if "list-pull-reviews" in argv:
            if down["reviews"]:
                return None
            return {"data": [{"id": "1000", "state": "approved", "body": "ok",
                              "author": {"username": "AnyBuddy"},
                              "created_at": "2026-09-19T18:00:00Z"}], "totalPages": 1}
        return None

    try:
        globals()["cnb_json"] = _stub

        # 6a) issue 评论面读失败：游标逐字段不变，且不得产出误导性的 issue_comment_edited
        cur = {"self": "QAQTam", "repo": "o/r", "seq": 0,
               "issues": {"22": {"comments": 4, "updated_at": "2026-09-19T10:00:00Z",
                                 "state": "open", "last_comment_id": "100"}},
               "pulls": {}}
        ev = poll_once(cur, "o/r")
        check("读失败：issue 游标不得推进",
              cur["issues"]["22"]["comments"] == 4
              and cur["issues"]["22"]["updated_at"] == "2026-09-19T10:00:00Z"
              and cur["issues"]["22"]["last_comment_id"] == "100")
        check("读失败：不得产出误导性的 issue_comment_edited", [e["kind"] for e in ev] == [])

        down["issue_comments"] = False                   # 下一轮恢复
        ev = poll_once(cur, "o/r")
        check("读失败恢复后：漏掉的那条必须补投",
              [e["kind"] for e in ev] == ["issue_comment"] and ev[0]["cid"] == "200")
        check("读失败恢复后：游标推进到最新", cur["issues"]["22"]["last_comment_id"] == "200")

        # 6b) 评审面读失败：`last_review_id` 绝不能被重置（否则历史评审永不重放）
        live["issues"] = False
        live["pulls"] = True
        cur2 = {"self": "QAQTam", "repo": "o/r", "seq": 0, "issues": {},
                "pulls": {"23": {"comments": 2, "reviews": 1, "last_comment_id": "500",
                                 "last_review_id": "999", "state": "open",
                                 "mergeable_state": "mergeable",
                                 "updated_at": "2026-09-19T10:00:00Z"}}}
        ev = poll_once(cur2, "o/r")
        check("读失败：last_review_id 不得被重置", cur2["pulls"]["23"]["last_review_id"] == "999")
        check("读失败：reviews 计数不得推进", cur2["pulls"]["23"]["reviews"] == 1)
        check("读失败：评审面不得产出事件", [e["kind"] for e in ev] == [])

        down["reviews"] = False
        ev = poll_once(cur2, "o/r")
        check("读失败恢复后：漏掉的评审必须补投",
              [e["kind"] for e in ev] == ["pr_review"] and ev[0]["cid"] == "1000")
    finally:
        globals()["cnb_json"] = saved_json

    print(f"\n{len(failures)} 项失败" if failures else "\n全部通过")
    return 1 if failures else 0


# ───────────────────────────── MCP 适配器 ─────────────────────────────

MCP_TOOLS = [
    {
        "name": "chat_poll",
        "description": "增量拉取 CNB 协同通道的新消息（issue/PR 评论、评审、状态变更）。",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "chat_send",
        "description": "在 issue 或 PR 下发言。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": {"type": "string", "enum": ["issue", "pr"]},
                "number": {"type": "integer"},
                "body": {"type": "string"},
                "mention": {"type": "string"},
            },
            "required": ["target", "number", "body"],
        },
    },
    {
        "name": "chat_inbox",
        "description": "回看最近的协同通道事件。",
        "inputSchema": {
            "type": "object",
            "properties": {"tail": {"type": "integer", "default": 20}},
        },
    },
]


def mcp_call(name: str, arguments: dict) -> dict:
    if name == "chat_poll":
        # 与 cmd_poll/watch_loop 同样持锁：MCP 客户端与常驻 watch 并存时，
        # 无锁路径正是 state_lock docstring 点名的「重复投递 / seq 冲突」。
        with state_lock():
            cur = load_cursor()
            repo = cur.get("repo") or repo_slug()
            cur["repo"] = repo
            if not cur.get("self"):
                cur["self"] = whoami()
            events = poll_once(cur, repo)
            save_cursor(cur)
        text = "\n\n".join(render(e, color=False) for e in events) or "(无新消息)"
    elif name == "chat_send":
        ns = argparse.Namespace(
            repo=None, issue=arguments.get("number") if arguments.get("target") == "issue" else None,
            pr=arguments.get("number") if arguments.get("target") == "pr" else None,
            body=arguments.get("body"), body_file=None, mention=arguments.get("mention"),
        )
        code = cmd_send(ns)
        text = "已发送" if code == 0 else "发送失败，见 stderr"
    elif name == "chat_inbox":
        ns = argparse.Namespace(tail=int(arguments.get("tail", 20)), json=False)
        cmd_inbox(ns)
        text = "(见 stdout)"
    else:
        return {"content": [{"type": "text", "text": f"未知工具: {name}"}], "isError": True}
    return {"content": [{"type": "text", "text": text}], "isError": False}


def cmd_mcp(args) -> int:
    warn("MCP stdio server 已启动（注意：本机 [mcp] enabled = false，尚未注册到 harness）")
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
        except json.JSONDecodeError:
            continue
        method = req.get("method")
        rid = req.get("id")
        if method in ("notifications/initialized", "notifications/cancelled"):
            continue
        if method == "initialize":
            result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "cnb-chat", "version": "0.1.0"}}
        elif method == "tools/list":
            result = {"tools": MCP_TOOLS}
        elif method == "tools/call":
            params = req.get("params") or {}
            result = mcp_call(params.get("name", ""), params.get("arguments") or {})
        elif method == "ping":
            result = {}
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": rid,
                              "error": {"code": -32601, "message": f"未实现: {method}"}}), flush=True)
            continue
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}, ensure_ascii=False), flush=True)
    return 0


# ───────────────────────────── CLI ─────────────────────────────

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="cnb_chat.py", description="CNB 协同通道监听器/发言口")
    # `--repo` 挂在各子命令上（而不是顶层）：argparse 的子解析器会把默认值写回
    # 命名空间，顶层与子命令各定义一次会互相覆盖。
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--repo", help=f"仓库 slug（默认取 git remote，兜底 {DEFAULT_REPO}）")
    sub = p.add_subparsers(dest="cmd", required=True)

    poll = sub.add_parser("poll", parents=[common], help="单轮增量扫描（--interval 则常驻）")
    poll.add_argument("--once", action="store_true", help="只跑一轮（默认行为，显式化用）")
    poll.add_argument("--interval", type=int, default=0, help="常驻监听间隔秒数，0=只跑一轮")
    poll.add_argument("--baseline", action="store_true", help="只标定游标，不产出事件")
    poll.add_argument("--json", action="store_true", help="以 JSONL 输出事件")
    poll.add_argument("--threads", help="只投递这些线程，逗号分隔（如 '#22,PR#21'）")
    poll.add_argument("--since", help="只投递该时刻后的事件：-30m/-2h/-1d 或 2026-09-20 / ISO8601")
    poll.add_argument("--verbose-idle", action="store_true", help="静默时也打心跳到 stderr")
    poll.set_defaults(func=cmd_poll)

    watch = sub.add_parser("watch", parents=[common], help="常驻监听（poll --interval 的别名）")
    watch.add_argument("--interval", type=int, default=20)
    watch.add_argument("--json", action="store_true")
    watch.add_argument("--threads", help="只投递这些线程，逗号分隔")
    watch.add_argument("--since", help="只投递该时刻后的事件（-2h / 2026-09-20 / ISO8601）")
    watch.add_argument("--verbose-idle", action="store_true")
    watch.set_defaults(func=cmd_poll)

    send = sub.add_parser("send", parents=[common], help="发言")
    target = send.add_mutually_exclusive_group(required=True)
    target.add_argument("--issue", help="issue 编号")
    target.add_argument("--pr", help="PR 编号")
    body = send.add_mutually_exclusive_group(required=True)
    body.add_argument("--body", help="正文")
    body.add_argument("--body-file", help="从文件读正文（推荐，避开 shell 转义）")
    send.add_argument("--mention", help="在正文前加 @用户")
    send.set_defaults(func=cmd_send)

    inbox = sub.add_parser("inbox", parents=[common], help="回看收件箱")
    inbox.add_argument("--tail", type=int, default=20, help="只看最近 N 条，0=全部")
    inbox.add_argument("--json", action="store_true")
    inbox.set_defaults(func=cmd_inbox)

    sub.add_parser("whoami", parents=[common], help="当前 cnb 身份").set_defaults(func=cmd_whoami)
    sub.add_parser("selftest", parents=[common], help="离线自测（不触网）").set_defaults(func=cmd_selftest)
    sub.add_parser("mcp", parents=[common], help="以 stdio MCP server 运行").set_defaults(func=cmd_mcp)
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
