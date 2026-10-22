# TUI Release 流程

## 目标和产物

只构建 `cargo build --locked --release --bin qaqh-tui --target <target>`。
四个原生 GitHub runner 分别输出 Windows x64 ZIP、Linux x64 tar.gz、macOS Intel
tar.gz、macOS ARM64 tar.gz。最终发布四包加一份 `SHA256SUMS.txt`。
包内仅 TUI 二进制、MIT/Apache 许可证、BUILD-INFO.json；不包含 daemon。

Linux 使用 Ubuntu 22.04 GNU 工具链，不是静态 musl 包；macOS deployment target 为 13.0。
未配置代码签名、公证、安装器或自动更新器。

## 固定依赖

`dependencies.json` 固定 Rust 版本、backend 客户端库仓库提交、ratatui 提交。
CI 目录布局：

```text
$GITHUB_WORKSPACE/
  tui/           # 本仓，按 refs/tags/2.0.0 检出
  qaqh-backend/   # 仅作为 path crate 编译来源，不执行 backend workspace build
  ratatui/       # 本仓 Cargo.toml 的 path patch 来源
```

保留本地 path 依赖以使用权威客户端类型。CI 必须能读取这两个固定提交；backend
如果是私有仓，给本仓配置 Actions secret **RELEASE_DEPENDENCY_TOKEN**，使用仅有
`QAQTam/qaqh-backend` Contents: read 权限的 fine-grained PAT（或等效 GitHub App token）。
公开仓可不设置，回退本仓 GITHUB_TOKEN。token 仅用于检出，不写产物，checkout 不持久化凭据。
默认 GITHUB_TOKEN 不能读取其他私有仓库，不能因为 token 自动回退就假设访问成功。

升级依赖时先更新锁文件/验证，再同步此 JSON 的完整 SHA；不要让 CI 跟随分支最新提交。

## 发布

版本、发布说明和工作流必须**先提交，再创建标签**，否则标签提交里没有 CI。
标签使用裸版本 `2.0.0`，不带 `v`，并必须等于 Cargo.toml 版本。

```bash
# 本轮已准备本地发布提交和标签时，只需执行下面的 push。
# 未准备过时，先审查/提交改动，再 git tag -a 2.0.0 -m 'qaqh-tui 2.0.0'。
git push origin main
git push origin 2.0.0
```

推送标签自动触发 `.github/workflows/release.yml`：

1. 检出标签，检查 tag/version、Python 打包脚本测试、Rust 格式及 TUI 单元测试。
2. 四个原生 runner 使用固定 Rust 和依赖提交，仅构建 TUI，运行 `--version` 冒烟探针。
3. Python 打包并计算 SHA256，上传短期 Actions artifacts。
4. 全部成功后验证四包齐全及校验和，创建 draft Release、上传资产，最后转为正式发布。

构建失败不会创建新公开 Release。上传中断可能留下 draft，可重跑恢复。
也可在 Actions → **Release TUI** → Run workflow，输入一个已存在的版本标签重跑。
手动运行仍按指定 tag 检出源码，不会悄悄打当前 main。若已发布同名 Release，重跑会
使用 `--clobber` 替换同名资产；不要移动已公开标签，后续修复使用新版本。

工作流仅 publish job 有 Contents: write；在本仓 Actions 设置允许此工作流的 token
写 release。手动分发工作流需要工作流文件先存在于默认分支。

## 本地低成本验证（不编译 release）

需要 Python 3.11+：

```bash
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 scripts/release.py check-tag --tag 2.0.0
TMPDIR="$PWD/target" cargo test --offline --locked -j 1 -- --test-threads=1 --quiet
cargo fmt -- --check
git diff --check
```

需要在某台机器手动构建打包时（这是显式 release 编译，不是上述测试的一部分）：

```bash
python3 scripts/release.py build --target x86_64-unknown-linux-gnu --tag 2.0.0 --out dist
```

脚本会核对依赖 checkout SHA 与干净状态；本地 backend 文档脏改也会阻止正式打包，
这是为避免把“固定提交”和“本机修改”混为一谈。CI 从干净提交检出，不受此限制。

## 尚需 GitHub 端确认

当前开发环境曾无法 DNS 解析 github.com，因此本地不能替代远端验证：依赖 SHA 已推送
且可读、GitHub runner 标签可用、固定 Rust 版本可安装、四平台实际编译/运行、Release
权限与上传都需要以 Actions 的真实结果为准。不要把本地打包单测通过写成四平台构建成功。
