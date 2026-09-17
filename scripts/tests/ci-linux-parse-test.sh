#!/bin/bash
# ci-linux.sh 的解析与依赖校验用例（不联网）。
#
# 为什么需要它：`scripts/ci-linux.sh` 里那两处（channel 解析、prepare() 的 rev 校验）
# 都是**纯 shell 控制流**，且只在畸形输入下才现形——PR #19 复审指出上一版正是因此
# 一路带着三个 bug 存活。所以这里按复审要求给出**可证伪**的用例。
#
# 证伪标准：把 ci-linux.sh 里对应实现改回旧行为，对应用例必须变红：
#   - 解析改回 `grep | head | sed` 贪婪版 → 用例 2/3 红
#   - 解析失败改回「假装 fallback」   → 用例 1 红
#   - prepare() 对已有目录直接 return 0 → 用例 4/5 红
#
# 用法：bash scripts/tests/ci-linux-parse-test.sh

set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT="$HERE/../ci-linux.sh"
[ -f "$SCRIPT" ] || { echo "找不到 $SCRIPT" >&2; exit 1; }

# 只 source 函数，不跑主流程（脚本用 BASH_SOURCE 守卫）。
# shellcheck source=/dev/null
. "$SCRIPT"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

pass=0; fail=0
ok()   { printf '  ✓ %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  ✗ %s\n' "$1"; fail=$((fail+1)); }

expect_tc() {  # $1=用例名  $2=文件内容  $3=期望（"ERR" 表示应解析失败）
    local name="$1" content="$2" want="$3" f="$TMP/rt-$RANDOM.toml" got
    printf '%s\n' "$content" > "$f"
    if got=$(resolve_toolchain "$f"); then
        if [ "$want" = "ERR" ]; then bad "$name：期望解析失败，实得 '$got'"
        elif [ "$got" = "$want" ]; then ok "$name：解析出 $got"
        else bad "$name：期望 '$want'，实得 '$got'"; fi
    else
        if [ "$want" = "ERR" ]; then ok "$name：按预期解析失败（非零退出）"
        else bad "$name：期望 '$want'，但解析失败"; fi
    fi
}

echo "== resolve_toolchain =="
expect_tc "标准写法"            'channel = "1.98.0"'                          1.98.0
expect_tc "无空格"              'channel="1.98.0"'                             1.98.0
expect_tc "无引号"              'channel = 1.98.0'                             1.98.0
expect_tc "行尾注释含另一个版本" 'channel = "1.98.0" # 备用 "v1.98.1"'          1.98.0
expect_tc "注释掉的 channel"    '# channel = "9.9.9"'                          ERR
expect_tc "空值"                'channel = ""'                                 ERR
expect_tc "无 channel 行"       '[toolchain]
components = ["clippy"]'                                                        ERR

echo
echo "== prepare(): 已有目录也要校验 rev =="
mk_repo() {  # 造一个只有一次提交的真 git 仓库，返回其 sha
    local d="$1"
    git init -q "$d"
    git -C "$d" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
    git -C "$d" rev-parse HEAD
}

# 用例 4：目录存在且 rev 相符 → 成功
d="$TMP/match"; sha=$(mk_repo "$d")
if (STRICT_DEPS=1; prepare "$d" "unused" "$sha" "match" >/dev/null 2>&1); then
    ok "rev 相符：通过"
else bad "rev 相符：本应通过"; fi

# 用例 5：目录存在但 rev 不符 → 严格模式必须失败（旧行为会 return 0）
d="$TMP/mismatch"; mk_repo "$d" >/dev/null
if (STRICT_DEPS=1; prepare "$d" "unused" "0000000000000000000000000000000000000000" "mismatch" >/dev/null 2>&1); then
    bad "rev 不符：严格模式下本应失败，却通过了（旧行为回归）"
else ok "rev 不符：严格模式按预期失败"; fi

# 用例 6：目录存在但不是 git 仓库 → 严格模式必须失败
d="$TMP/notgit"; mkdir -p "$d"
if (STRICT_DEPS=1; prepare "$d" "unused" "deadbeef" "notgit" >/dev/null 2>&1); then
    bad "非 git 目录：严格模式下本应失败，却通过了（旧行为回归）"
else ok "非 git 目录：严格模式按预期失败"; fi

# 用例 7：非严格模式（本机开发）下 rev 不符只告警、不动工作树
d="$TMP/local"; sha=$(mk_repo "$d")
before=$(git -C "$d" rev-parse HEAD)
if prepare "$d" "unused" "0000000000000000000000000000000000000000" "local" >/dev/null 2>&1; then
    after=$(git -C "$d" rev-parse HEAD)
    if [ "$before" = "$after" ]; then ok "非严格模式：未改动开发者工作树"
    else bad "非严格模式：HEAD 被改动了（$before → $after）"; fi
else bad "非严格模式：本应通过（只告警）"; fi

# 用例 8：解析失败时，main 里 `if tc=$(resolve_toolchain ...)` 这个**模式**必须
# 走 else 分支。上一版的 bug 正是：`TC=$(grep ... | head | sed ...)` 在
# `set -euo pipefail` 下 grep 无匹配 → 命令替换失败 → **整脚本静默 exit 1**，
# else/fallback 永远是死代码。这里直接断言该模式可达。
d="$TMP/none.toml"; printf '[toolchain]\ncomponents = ["clippy"]\n' > "$d"
if tc=$(resolve_toolchain "$d"); then
    bad "解析失败时：本应走 else 分支，却进了 then（tc='$tc'）"
else
    ok "解析失败时：正确进入 else 分支（未被 set -e 静默中止）"
fi

echo
echo "通过 $pass / 失败 $fail"
[ "$fail" -eq 0 ] || exit 1
echo "解析与依赖校验用例全部通过"
