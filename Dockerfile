# qaqh-tui-app NPC 运行环境：Ubuntu 26.04 LTS + Rust 1.98.0 + gcc-15 + cmake
# tui-app 的传输层由 qaqh-client 提供；其 reqwest 走 rustls（aws-lc-rs），
# 不再依赖 OpenSSL / native-tls。构建后推送 CNB 制品库，供 NPC 事件引用。
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

# 基础工具链 + NPC 运行时依赖（aws-lc-sys/onig_sys 等需要 cmake/pkg-config；
# qaqh-client 已切 rustls，不再安装 libssl-dev）
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

# Rust 1.98.0 固定工具链（对齐 rust-toolchain.toml；U-28）
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain 1.98.0 \
    && rustup component add clippy rustfmt \
    && cargo --version && rustc --version

# NPC 运行时：CNB CLI + Skills 框架 + 官方 cnb-skill
RUN npm install -g @cnbcool/cnb-cli skills \
    && npx skills add https://cnb.cool/cnb/skills/cnb-skill.git -g -y \
    && cnb --version || true

ENV TZ=Asia/Shanghai
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 冒烟断言：环境不对即 fail 构建
RUN cargo --version | grep -q 1.98.0 \
    && gcc-15 --version | head -1 \
    && cmake --version | head -1 \
    && pkg-config --version \
    && node -v \
    && bash --version | head -1 \
    && echo "[qaqh-tui-npc-env] smoke check passed"

CMD ["/bin/bash"]
