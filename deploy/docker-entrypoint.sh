#!/bin/sh
# 镜像的入口（Dockerfile）。先准备好上下文仓库；运行 serve 时再同步索引、起服务，别的子命令原样交给 tcms。
set -eu

repo=${TCMS_REPO:-/var/lib/tcms/context}
# 挂进来的是空目录（新的命名卷）时建一个空仓库，第一次 serve 之前发令牌、导入也能用。
# 人往里加内容用 tcms import，或者在审核页批准 agent 的提议。
if [ ! -e "$repo/.git" ]; then
  if [ -n "$(ls -A "$repo" 2>/dev/null)" ]; then
    echo "上下文仓库 $repo 里有文件但不是 git 仓库；先在主机上 git init 并提交，或者换一个空目录" >&2
    exit 1
  fi
  git init -q "$repo"
  echo "上下文仓库 $repo 是空的，已 git init" >&2
fi

if [ "${1:-serve}" = "serve" ]; then
  [ $# -gt 0 ] && shift
  # 人在仓库里直接改过（或换了仓库）时，启动时同步一次；服务起来以后批准的提议自己更新索引。
  tcms reindex
  exec tcms serve "$@"
fi

exec tcms "$@"
