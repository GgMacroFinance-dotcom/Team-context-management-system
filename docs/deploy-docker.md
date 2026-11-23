# 用 Docker 部署

> 镜像里只有 `tcms` 和 git，用 `Dockerfile` 在仓库根目录构建。
> 和 I.P. 一起部署时不用这里的 compose：I.P. 仓库的 `rust/docker-compose.yml` 里有 `tcms` 服务，和 I.P. 共用一个 PostgreSQL，步骤见 I.P. 的 `docs/ip/deploy-docker.md`“团队上下文”。
> 这里讲单独部署，用 `deploy/docker-compose.yml`（一个 PostgreSQL 加 tcms）。

## 镜像

```bash
docker build -t tcms .
```

- 容器里以 uid 10002 的 `tcms` 用户运行。
- 上下文仓库在 `/var/lib/tcms/context`，要挂卷。
- 默认监听 `0.0.0.0:7341`，提供 `/mcp`、审核页 `/review` 和健康检查 `/healthz`。

入口脚本 `deploy/docker-entrypoint.sh` 做三件事：

1. 上下文仓库是空目录时先 `git init`。
2. 运行 `serve`（默认命令）时先 `tcms reindex`，把仓库的 HEAD 同步进 PostgreSQL，再起服务。
3. 别的子命令原样交给 `tcms`，比如 `docker run ... tcms token add ...`。

数据库连接串只从环境变量读：

| 变量 | 说明 |
| --- | --- |
| `TCMS_DATABASE_URL` | 必填，PostgreSQL 连接串 |
| `TCMS_SCHEMA` | 用哪个 schema，默认 `tcms`。schema 已经由管理员建好时，账号不需要在库里建 schema 的权限 |
| `TCMS_REPO` | 上下文仓库，镜像里默认 `/var/lib/tcms/context` |
| `TCMS_LISTEN` | 监听地址，镜像里默认 `0.0.0.0:7341`。健康检查跟着它的端口走 |
| `TCMS_ALLOWED_HOSTS` | 除了本机以外还接受的 `Host`，逗号隔开。别的容器用服务名连时写服务名，比如 `tcms`；对外提供时写域名 |
| `TCMS_GIT_EMAIL` | 审核页批准提议时提交者的邮箱，名字是登录的人 |

## 单独部署

以下都在 `deploy/` 目录里执行。

1. **密钥**：复制 `env.example`，用 `openssl rand -hex 24` 生成 `POSTGRES_PASSWORD` 填进去。`.env` 已被 git 忽略。

   ```bash
   cp env.example .env
   ```

2. **构建、启动**：`docker compose ps` 里 tcms 显示 `healthy` 就起来了。

   ```bash
   docker compose up -d --build
   ```

3. **发令牌**：每个 agent 一个。明文只打印这一次，放进 agent 的环境变量。

   ```bash
   docker compose run --rm tcms token add --name "I.P. 宏观研报" --role macro
   ```

   岗位属于哪个团队，默认从上下文仓库的 `teams.yaml` 查。仓库还是空的时候，可以用 `--team research` 直接写上。

   审核的人用另一种令牌，在审核页 `http://127.0.0.1:7341/review` 登录：

   ```bash
   docker compose run --rm tcms token add --reviewer --name "王审核"
   ```

4. **放内容**：见下一节。

端口只开放给本机。在远程主机上看审核页，用 SSH 转发：

```bash
ssh -L 7341:127.0.0.1:7341 <主机>
```

要让别的机器上的 agent 连 `/mcp`，放在 HTTPS 反向代理后面，并在 `.env` 里写 `TCMS_ALLOWED_HOSTS=<域名>`。

## 往上下文仓库里放内容

有三种办法。

**导入一个 Markdown 目录**（比如 Notion、AFFiNE 导出的）。把目录挂进一次性的容器，`--into` 的第一层是范围（`org`、`team-<团队>`、`role-<岗位>`、`project-<项目>`）。导入会补好 id、做成一个提交，之后再同步一次索引：

```bash
docker compose run --rm -v "$PWD/notion-export:/import:ro" tcms import /import --into team-research
docker compose run --rm tcms reindex
```

运行中的服务收到通知后自己换上新的检索索引，不用重启。

**agent 提议、人批准**：agent 用 `context_propose` 提，人在审核页批准，批准后写进仓库、做成一个提交。

**换成自己的 git 仓库**：把 compose 里的 `context:/var/lib/tcms/context` 换成主机上的目录，比如 `./context:/var/lib/tcms/context`。这个目录要已经是 git 仓库（或者是空的），属主要是 uid 10002，否则批准时写不进去：

```bash
sudo chown -R 10002:10002 ./context
```

人直接在仓库里改了、提交以后，运行 `docker compose run --rm tcms reindex`，或者重启 tcms（启动时会同步一次）。

`teams.yaml`（团队有哪些岗位）现在只能这样直接提交进仓库，导入和提议都不管它。

## 日常维护

**升级**：

```bash
git pull && docker compose up -d --build
```

**备份**：

- 数据库（提议、令牌、审计、索引）用 `pg_dump` 导：

  ```bash
  docker compose exec -T postgres pg_dump -U tcms -d tcms -n tcms -Fc > tcms-$(date +%F).dump
  ```

- 上下文仓库是真相，打一个 git bundle：

  ```bash
  docker compose exec -T tcms git -C /var/lib/tcms/context bundle create - --all > context-$(date +%F).bundle
  ```

  索引丢了可以从仓库重建：`tcms reindex --full`。

**还没有的**：批准以后推送到远端（客户自己的 GitHub、Gitea），现在只提交在卷里的仓库。

## 实测结果

2026-11-23 在一台开发机上跑的：Docker 29.10.0，Compose v5.7.1，Linux 6.18。基础镜像从 Docker Hub 的镜像站拉。
这台机器只能经 HTTPS 代理出网，所以构建时临时多加了两行，让 cargo 和 apt 信任代理的证书、apt 走 https。仓库里的 `Dockerfile` 没有这两行。

| 测了什么 | 结果 |
| --- | --- |
| 构建 | 通过。镜像 315 MB，其中 `tcms` 28 MB，git 2.47.3 |
| 空卷第一次启动 | 入口脚本 `git init`，`tcms reindex` 报“仓库还没有提交”，服务照常起来，几秒后 `healthy` |
| 第一次 serve 之前发令牌 | 能发：入口脚本对所有子命令都先准备好仓库 |
| 挂进来一个有文件、不是 git 仓库的目录 | 启动失败，提示先 `git init` 并提交 |
| 挂主机上已有的仓库（属主 uid 10002） | `tcms reindex` 解析了 3 个文件、15 条 |
| `deploy/docker-compose.yml` 单独部署 | 两个服务都 `healthy`；发审核令牌、导入一个目录（7 条）、`reindex`、`pg_dump`、`git bundle` 都照上面的命令跑通 |
| 和 I.P. 一起部署（I.P. 仓库的 compose，`COMPOSE_PROFILES=tcms`） | 详见 I.P. 的 `docs/ip/deploy-docker.md`。这一步发现：`tcms_app` 没有在库里建 schema 的权限，旧代码的 `CREATE SCHEMA IF NOT EXISTS` 报 permission denied，tcms 起不来；改成先查再建，加了测试 |
| 别的 `Host` | 审核页 403；`Host: tcms`（写进了 `TCMS_ALLOWED_HOSTS`）能连 |
| `docker compose stop tcms` | 退出码 0，日志最后一行“已停止” |

没测的：放在 HTTPS 反向代理后面让别的机器连 `/mcp`；从旧版本升级（还没有发过版本）。
