# qaqh-tui-app NPC 运行环境：Ubuntu 26.04 LTS + Rust 1.98.1 + gcc-15 + cmake + OpenSSL dev
# tui-app 为跨平台终端应用（crossterm + reqwest/native-tls），Linux 云端可全量编译与测试。
# 构建后推送 CNB 制品库，供 issue.comment@npc / pull_request.comment@npc 事件引用。
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

# 基础工具链 + OpenSSL 头文件（reqwest native-tls 在 Linux 的链接依赖）+ NPC 运行时依赖
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        build-essential \
        gcc-15 \
        g++-15 \
        cmake \
        libssl-dev \
        pkg-config \
        git \
        git-lfs \
        curl \
        jq \
        ripgrep \
        file \
        unzip \
        xz-utils \
        tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && git lfs install

# Node 22（cnb-cli / skills 运行时）
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && node -v && npm -v

# Rust 1.98.1 固定工具链
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain 1.98.1 \
    && rustup component add clippy rustfmt \
    && cargo --version && rustc --version

# NPC 运行时：CNB CLI + Skills 框架 + 官方 cnb-skill
RUN npm install -g @cnbcool/cnb-cli skills \
    && npx skills add https://cnb.cool/cnb/skills/cnb-skill.git -g -y \
    && cnb --version || true

ENV TZ=Asia/Shanghai
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 冒烟断言：环境不对即 fail 构建（含 openssl-sys 链接前置条件的 pkg-config 验证）
RUN cargo --version | grep -q 1.98.1 \
    && gcc-15 --version | head -1 \
    && cmake --version | head -1 \
    && pkg-config --exists openssl \
    && node -v \
    && bash --version | head -1 \
    && echo "[qaqh-tui-npc-env] smoke check passed"

CMD ["/bin/bash"]
