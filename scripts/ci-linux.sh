#!/bin/bash
# Linux CI：准备兄弟仓库依赖，然后跑 test + clippy + fmt。
#
# ── 为什么需要这个脚本 ────────────────────────────────────────────────
# 本仓的 Cargo.toml 有两组**指向兄弟仓库的相对 path 依赖**：
#
#   qaqh-client / qaqh-config-api  →  ../qaqh-backend/crates/...
#   ratatui 一族（[patch.crates-io]） →  ../ratatui/...
#
# 本机布局是 `~/Projects/{qaqh-tui-app,qaqh-backend,ratatui}` 同级，所以能解析。
# 但 CNB 的仓库根是 `/workspace`，`../ratatui` 解析成 `/ratatui` —— **不存在**。
# 于是 cargo 在依赖解析阶段就死：
#
#   error: failed to load source for dependency `ratatui`
#     unable to update /ratatui/ratatui
#     failed to read `/ratatui/ratatui/Cargo.toml`
#     No such file or directory (os error 2)
#
# 这自 2026-09-15（T-01 迁移引入 path 依赖）起就让 `main` 的 CI 一直是红的。
#
# ── 版本策略（2026-09-20 · W-05 / U-30 已决）──────────────────────────
# 两个 rev **继续钉死**：跨仓 gate 必须可归因 —— 红了要能立刻分清「是本仓改动，
# 还是后端/上游漂移」。代价是钉住的 rev 会落后，所以本脚本额外**报告落后多少**
# （`report_drift`：只报告、不阻塞 —— 后端改造期间让 CI 因漂移变红只会烧掉
# 稀缺额度，且那条红不指向本仓的任何改动）。
# 刷新时机：**后端阶段性收口后由人确认**再改 rev；不要改成跟随 main。
# 改动这里必须同步 `Cargo.toml` 里 [patch.crates-io] 的注释，以及本机
# `.cargo/config.toml` 的 paths 覆盖（指向 `../qaqh-backend-anchor`，见 .gitignore）。
#
# 后端侧对应 `tui-anchor-2026-09-23-v2.0.0-rc`（annotated tag，不可移动）。**注意 rev 必须是
# 完整 40 位 SHA**：下面的 prepare() 用 `git rev-parse HEAD` 与它做字符串比对，
# 写 tag 名会判不等。换锚点时后端会新开 tag（如 `-r2`），**不要移动旧 tag**。
# ratatui 用的是**上游未发版的 main**（ratatui#2743 修复尚未发版）。
#
# ── 严格模式 ─────────────────────────────────────────────────────────
# `STRICT_DEPS=1`（CI 里由 .cnb.yml 设置）：兄弟仓库目录已存在时**也要校验 rev**，
# 不符就地修正、修不了则失败。这挡住「半成品残留目录让 CI 用错版本」。
# 未设置时（本机开发）：目录已存在就只告警、**不动开发者的工作树**。
#
# ── 可测性 ───────────────────────────────────────────────────────────
# 解析与 prepare() 都是纯函数，可被 `scripts/tests/ci-linux-parse-test.sh` source
# 后直接断言（不联网）。主流程只在脚本被直接执行时运行。

set -euo pipefail

# 与 Cargo.toml 的 path 依赖对应。改动请同步 Cargo.toml 的注释。
QAQH_BACKEND_REPO="${QAQH_BACKEND_REPO:-https://cnb.cool/QAQ-Harness/qaqh-backend.git}"
QAQH_BACKEND_REV="${QAQH_BACKEND_REV:-1e78b7ce98df2875f036dc8aec1e2371779d0f58}"
RATATUI_REPO="${RATATUI_REPO:-https://github.com/ratatui/ratatui.git}"
RATATUI_REV="${RATATUI_REV:-e02e2a622eda6e4cae105df48a48f641cdba0303}"

# 从 rust-toolchain.toml 解析 `channel`。
#
# 成功 → stdout 打印 channel，返回 0；失败（文件缺失 / 没有 channel 行 / 值为空）
# → 返回非零，**不打印任何东西**。调用方必须处理失败，不要假装有 fallback：
# 上一版用 `grep | head | sed` 直接赋值，`grep` 无匹配时在 `set -e` 下会**静默
# 中止整个脚本**，fallback 分支成了死代码（PR #19 复审指出）。
#
# 解析规则：只认行首（允许前导空白）的 `channel =`；引号可有可无；**忽略行尾注释**。
# 不能用贪婪的 `s/.*"([^"]+)".*/\1/` —— 它会取到**最后一个**引号里的内容，
# 于是 `channel = "1.98.0" # 备用 "v1.98.1"` 会解析成 `v1.98.1`，
# 组件就会装到另一套 toolchain 上，把「误诊」重新引入。
resolve_toolchain() {
    local file="$1" out
    [ -f "$file" ] || return 1
    out=$(sed -nE \
        's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"?([^"#[:space:]]+)"?.*$/\1/p' \
        "$file" | head -1) || true
    [ -n "$out" ] || return 1
    printf '%s\n' "$out"
}

# 确保 $dir 处有 $repo 的 $rev。
# 目录已存在时**始终校验 rev**（这一点与上一版不同：上一版对任何已有目录直接
# return 0，于是 clone 成功但 checkout 失败留下的半成品目录会让 CI 用错版本）。
prepare() {
    local dir="$1" repo="$2" rev="$3" name="$4" got
    if [ -e "$dir" ]; then
        if ! got=$(git -C "$dir" rev-parse HEAD 2>/dev/null); then
            echo "  [$name] 目录已存在但**不是 git 仓库**: $dir" >&2
            [ "${STRICT_DEPS:-0}" = "1" ] || { echo "  [$name] 非严格模式，跳过" >&2; return 0; }
            return 1
        fi
        if [ "$got" = "$rev" ]; then
            echo "  [$name] 已存在且 rev 相符: $got"
            return 0
        fi
        echo "  [$name] ⚠ 已存在但 rev 不符：期望 $rev，实得 $got" >&2
        if [ "${STRICT_DEPS:-0}" != "1" ]; then
            echo "  [$name] 非严格模式（本机开发）：不动你的工作树，继续" >&2
            return 0
        fi
        echo "  [$name] 严格模式：就地 fetch 并 checkout $rev"
        git -C "$dir" fetch --quiet origin || true
        git -C "$dir" checkout --quiet "$rev"
        got=$(git -C "$dir" rev-parse HEAD)
        if [ "$got" != "$rev" ]; then
            echo "  [$name] ✗ 就地修正失败：实得 $got" >&2
            return 1
        fi
        echo "  [$name] ✓ 已修正为 $got"
        return 0
    fi

    echo "  [$name] 拉取 $repo @ $rev"
    git clone --filter=blob:none --no-checkout "$repo" "$dir"
    git -C "$dir" checkout --quiet "$rev"
    got=$(git -C "$dir" rev-parse HEAD)
    if [ "$got" != "$rev" ]; then
        echo "  [$name] ✗ rev 不符：期望 $rev，实得 $got" >&2
        return 1
    fi
    echo "  [$name] ✓ $got"
}

# $dir 的 HEAD 落后 $ref 多少个提交；不可用时返回非零且**不打印任何东西**。
# 纯函数（只读本地 git 对象，不发网络请求），可被测试直接断言。
count_behind() {
    local dir="$1" ref="$2" got n
    got=$(git -C "$dir" rev-parse HEAD 2>/dev/null) || return 1
    n=$(git -C "$dir" rev-list --count "$got..$ref" 2>/dev/null) || return 1
    printf '%s\n' "$n"
}

# U-30：报告钉住的 rev 落后远端默认分支多少（**只报告，不阻塞**）。
#
# 只在 STRICT_DEPS=1（CI）下调用：本机开发时兄弟仓库是开发者自己的工作树，
# 不该在这里发网络请求 —— 与 prepare() 的「非严格模式不动你的工作树」同一条纪律。
report_drift() {
    local dir="$1" name="$2" behind
    [ -d "$dir/.git" ] || return 0
    if ! git -C "$dir" fetch --quiet origin main 2>/dev/null; then
        echo "  [$name] 漂移检查跳过（fetch origin main 失败，可能离线）"
        return 0
    fi
    if behind=$(count_behind "$dir" FETCH_HEAD); then
        if [ "$behind" = "0" ]; then
            echo "  [$name] 与 origin/main 一致"
        else
            echo "  [$name] ⚠ 钉住的 rev 落后 origin/main **$behind** 个提交（U-30：只报告，不阻塞）"
        fi
    else
        echo "  [$name] 漂移检查跳过（无法计算落后提交数）"
    fi
}

main() {
    local root parent tc_file tc
    root=$(git rev-parse --show-toplevel)
    parent=$(dirname "$root")

    echo "== 依赖准备 =="
    echo "仓库根: $root"
    echo "兄弟仓库父目录: $parent"
    echo "严格模式(STRICT_DEPS): ${STRICT_DEPS:-0}"

    prepare "$parent/qaqh-backend" "$QAQH_BACKEND_REPO" "$QAQH_BACKEND_REV" "qaqh-backend"
    prepare "$parent/ratatui" "$RATATUI_REPO" "$RATATUI_REV" "ratatui"

    # U-30：钉死的 rev 落后多少（只报告，不阻塞）。仅 CI —— 本机不动开发者工作树。
    if [ "${STRICT_DEPS:-0}" = "1" ]; then
        echo
        echo "== 依赖漂移（U-30：只报告，不阻塞）=="
        report_drift "$parent/qaqh-backend" "qaqh-backend"
        report_drift "$parent/ratatui" "ratatui"
    fi

    # libssl-dev / pkg-config：为可能仍需要系统 TLS 头文件的构建准备（幂等）。
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update -qq
        apt-get install -y --no-install-recommends pkg-config libssl-dev >/dev/null
    fi

    echo
    echo "== 工具链与组件 =="
    # U-28（2026-09-20）后镜像 tag 已对齐 `rust-toolchain.toml` 的 1.98.0，
    # 「两套 toolchain 并存、组件装错那套」的旧坑不复存在。下面仍显式
    # `--toolchain "$tc"`：组件必须装到**仓库钉的那套**上，而不是「当前活动」那套
    # —— 这条纪律与镜像版本无关，留着是为了不把正确性押在「镜像恰好装齐组件」上。
    if command -v rustup >/dev/null 2>&1; then
        # 用绝对路径：`[ -f rust-toolchain.toml ]` 依赖 cwd，而 $root 已在手。
        tc_file="$root/rust-toolchain.toml"
        if tc=$(resolve_toolchain "$tc_file"); then
            echo "  按 rust-toolchain.toml 使用 toolchain: $tc"
            rustup component add --toolchain "$tc" clippy rustfmt
        else
            # 显式失败，不静默中止、也不假装 fallback：解析不出 channel 时
            # 「猜一个」正是上一版出问题的地方。
            echo "✗ 无法从 $tc_file 解析出 channel（文件缺失 / 无 channel 行 / 值为空）" >&2
            echo "  请在 rust-toolchain.toml 写明 channel，或修正本脚本的解析。" >&2
            exit 1
        fi
        echo "  cargo 实际使用: $(rustup show active-toolchain 2>/dev/null || echo '(未知)')"
    fi

    echo
    echo "== cargo test =="
    cargo test --all-targets

    echo
    echo "== cargo clippy =="
    cargo clippy --all-targets -- -D warnings

    echo
    echo "== fmt（仅本仓）=="
    # 注意：**不能**裸跑 `cargo fmt --all` —— 它会连带格式化兄弟仓库里别人的工作树
    # （`../qaqh-backend` 在 HEAD 上本就不是 fmt-clean）。不带 `--all` 只检查本包。
    if ! cargo fmt --version >/dev/null 2>&1; then
        echo "✗ cargo fmt 不可用。当前活动 toolchain：$(rustup show active-toolchain 2>/dev/null || echo '(rustup 不可用)')" >&2
        echo "  若上面那行显示的不是 rust-toolchain.toml 里的版本，说明组件装错了 toolchain。" >&2
        exit 1
    fi
    cargo fmt --check

    echo
    echo "CI OK"
}

# 被 source 时（测试）只暴露函数，不跑主流程。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
