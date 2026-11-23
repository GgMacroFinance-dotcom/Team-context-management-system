# syntax=docker/dockerfile:1
#
# 团队上下文系统（tcms）的镜像。用法见 docs/deploy-docker.md。在仓库根目录构建：
#
#   docker build -t tcms .
#
# 镜像里只有 tcms、git 和 ssh 客户端。上下文仓库在 /var/lib/tcms/context（要挂卷），第一次启动时是空的就 git init 一个。
# 容器默认运行 tcms serve：先 tcms reindex 把仓库的 HEAD 同步进 PostgreSQL，再在 0.0.0.0:7341 上提供 /mcp 和审核页。
# 别的子命令照常用，比如 docker run ... tcms token add --name "I.P. 宏观研报" --role macro。
# 数据库连接串只从环境变量 TCMS_DATABASE_URL 读，不写进镜像。

ARG RUST_VERSION=1.97
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS build
# 内存分配器 jemalloc（docs/architecture.md 第 9 节）构建时要用 make 编它的源码，这个镜像里没有 make。
RUN apt-get update \
 && apt-get install -y --no-install-recommends make \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# 依赖的下载和编译结果放在缓存里，只改源码时重新构建很快。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tcms-cli \
 && cp target/release/tcms /usr/local/bin/tcms

FROM debian:${DEBIAN_RELEASE}-slim

# git：入口脚本给空的上下文仓库 git init；配了远端（TCMS_REMOTE）时 tcms 用它拉取、推送；人进容器看历史、合并也用它。
# openssh-client：远端是 ssh 地址（git@…）时用。tini：转发信号、回收子进程。
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git openssh-client tini \
 && rm -rf /var/lib/apt/lists/*

# 不用 root 运行。上下文仓库的目录先建好交给 tcms 用户：挂上新的命名卷时，Docker 会连同属主一起初始化。
RUN useradd --create-home --home-dir /home/tcms --uid 10002 --shell /bin/bash tcms \
 && mkdir -p /var/lib/tcms/context \
 && chown -R tcms:tcms /var/lib/tcms
COPY --from=build /usr/local/bin/tcms /usr/local/bin/tcms
COPY deploy/docker-entrypoint.sh /usr/local/bin/tcms-entrypoint

USER tcms
WORKDIR /var/lib/tcms
ENV TCMS_REPO=/var/lib/tcms/context \
    TCMS_LISTEN=0.0.0.0:7341
VOLUME ["/var/lib/tcms/context"]
EXPOSE 7341

# 镜像里没有 curl，用 bash 自带的 /dev/tcp 请求 /healthz（不要令牌，回 ok）。端口取 TCMS_LISTEN 冒号后面那段。
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${TCMS_LISTEN##*:} && printf 'GET /healthz HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n' >&3 && grep -q '^ok' <&3"]

ENTRYPOINT ["tini", "--", "tcms-entrypoint"]
CMD ["serve"]
