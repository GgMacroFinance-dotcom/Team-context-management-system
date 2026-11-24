# 用 Docker 部署

> 镜像里只有 `tcms`、git 和 ssh 客户端，用 `Dockerfile` 在仓库根目录构建。
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

1. 上下文仓库是空目录时先 `git init`，分支叫 `main`。
2. 运行 `serve`（默认命令）时先 `tcms reindex`，把仓库的 HEAD 同步进 PostgreSQL，再起服务。
3. 别的子命令原样交给 `tcms`，比如 `docker run ... tcms token add ...`。

数据库连接串只从环境变量读：

| 变量 | 说明 |
| --- | --- |
| `TCMS_DATABASE_URL` | 必填，PostgreSQL 连接串。用普通账号（拥有 schema 就够），数据库的行级安全才起作用；超级用户和带 `BYPASSRLS` 的账号不受它管，启动时会提醒（`docs/architecture.md` 第 5 节） |
| `TCMS_SCHEMA` | 用哪个 schema，默认 `tcms`。schema 已经由管理员建好时，账号不需要在库里建 schema 的权限 |
| `TCMS_REPO` | 上下文仓库，镜像里默认 `/var/lib/tcms/context` |
| `TCMS_LISTEN` | 监听地址，镜像里默认 `0.0.0.0:7341`。健康检查跟着它的端口走 |
| `TCMS_ALLOWED_HOSTS` | 除了本机以外还接受的 `Host`，逗号隔开。别的容器用服务名连时写服务名，比如 `tcms`；对外提供时写域名 |
| `TCMS_GIT_EMAIL` | 审核页批准提议时提交者的邮箱，名字是登录的人 |
| `TCMS_REMOTE` | 可选，上下文仓库的 git 远端：地址，或者仓库里配好的远端名。见“推到自己的 git 远端” |
| `TCMS_REMOTE_BRANCH` | 远端的分支，默认和本地同名（新卷是 `main`） |
| `TCMS_GIT_TOKEN` | https 远端的令牌。只放在这里，不要写进地址 |
| `TCMS_GIT_USERNAME` | 和令牌一起用的用户名，默认 `x-access-token`（GitHub）；Gitea、GitLab 写自己的用户名 |
| `TCMS_SYNC_SECS` | 多久和远端同步一次，默认 60 秒；0 是只在批准、改单条时同步 |

## 单独部署

以下都在 `deploy/` 目录里执行。

数据库里有两个账号（2026-11-24 起）：PostgreSQL 镜像建的 `tcms` 是超级用户，只拿来管库、备份；tcms 服务用普通账号 `tcms_app` 连，它只拥有 schema `tcms`。超级用户不受数据库的行级安全管（`docs/architecture.md` 第 5 节），换成普通账号，agent 读条目、变更时数据库才会按令牌的范围再拦一道。空数据卷第一次启动时，`postgres-init/01-tcms-app.sh` 建这个账号和 schema。和 I.P. 一起部署时也是这样分的。

1. **密钥**：复制 `env.example`，用 `openssl rand -hex 24` 生成两个不一样的密码，填进 `POSTGRES_PASSWORD`（管理账号）和 `TCMS_DB_PASSWORD`（tcms 服务的账号）。`.env` 已被 git 忽略。

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

人直接在仓库里改了、提交以后，运行 `docker compose run --rm tcms reindex`，或者重启 tcms（启动时会同步一次）。新写的标题还没有 id 时不进索引，先运行 `docker compose run --rm tcms assign-ids`。推到下一节的远端时不用，服务拉下来时自动补。

`teams.yaml`（团队有哪些岗位）现在只能这样直接提交进仓库（或者推到下一节的远端），导入和提议都不管它。

## 推到自己的 git 远端

配了远端以后，批准、审核页改单条、导入、补 id 都是提交之前先拉、提交以后推；服务每 60 秒再同步一次，人推到远端的改动就这样进来、进索引。规则见 `spec.md` 6.5。

1. 在 GitHub、Gitea 上建一个仓库，空的或者已经放了上下文都行。
2. 发一个只对这个仓库有读写权限的令牌：GitHub 用 fine-grained token，权限给 Contents 读写；Gitea 用 Access Token，给 repository 读写。在 `.env` 里写：

   ```bash
   TCMS_REMOTE=https://github.com/<组织>/<仓库>.git
   TCMS_GIT_TOKEN=<令牌>
   # Gitea、GitLab 还要写用户名：
   # TCMS_GIT_USERNAME=<用户名>
   ```

   令牌不要写进地址（`https://<令牌>@…`），日志里会把地址里的用户名、密码换成 `***`，但放在 `TCMS_GIT_TOKEN` 里它才不会出现在 git 的命令行参数和仓库配置里。

   用 ssh 也行：远端写 `git@github.com:<组织>/<仓库>.git`，把部署密钥和 `known_hosts` 挂进容器，在 `docker-compose.override.yml` 里给 tcms 加 `GIT_SSH_COMMAND: ssh -i /run/secrets/deploy_key -o UserKnownHostsFile=/run/secrets/known_hosts -o BatchMode=yes`。私钥文件的属主要是 uid 10002、权限 600。

3. 重新起 tcms：`docker compose up -d`。日志里有一行“远端 …：每 60 秒同步一次”，启动后马上同步一次：
   - 卷里的仓库是空的、远端有内容：把远端的内容拉进来，建好索引。
   - 卷里有提交、远端是空的：推上去。
   - 两边都有提交但历史不相干（各自 `git init` 的）：git 不合并不相干的历史，日志里一直报错，批准照样在本地提交但推不上去。先定以哪边为准，换一个空的远端，或者清空卷从远端拉。
4. 手动同步一次、看结果：

   ```bash
   docker compose exec tcms tcms sync
   docker compose logs tcms | grep 远端
   ```

   审核页顶上也有一行，写着上次同步是几分钟前、正常没有；出错、合并不了时换颜色，写明原因和怎么合并。

**人怎么改**：克隆远端，改 Markdown，提交，推到 `main`。最多等一个同步间隔，服务拉下来、更新索引，运行中的检索索引跟着换。新写的标题不用自己写 id：服务拉下来时补上，做成一个“补 id：远端新来的 N 条”的提交推回去，日志里也有一行。接着改之前先 `git pull`，拿到这些 id。

**合并不了时**（服务器批准了一条，同时有人在远端改了同一处）：
- 服务把服务器这边的提交推到远端的旁支 `tcms/main-unmerged`。日志、`tcms sync` 和审核页上批准时的回答都会写明哪些文件冲突。
- 在自己的克隆里合并它，推回 `main`，下次同步服务器就快进过去：

  ```bash
  git fetch origin tcms/main-unmerged
  git merge FETCH_HEAD        # 解决冲突，git add，git commit
  git push origin main
  ```

- 合并好之前，审核页顶上一直提示合并不了，批准、改单条会停下来，回答“合并不了”，提议还是待审核。

## 日常维护

**升级**：

```bash
git pull && docker compose up -d --build
```

**老的数据卷**（2026-11-24 以前建的，tcms 服务原来用超级用户 `tcms` 连，schema 里的表都归它）：升级以后先在 `.env` 里加上 `TCMS_DB_PASSWORD`，不加的话 `docker compose` 会报“在 .env 里设 TCMS_DB_PASSWORD”。初始化脚本只在空数据卷上自动执行，老的数据卷里没有 `tcms_app`，tcms 起不来，日志里是 `password authentication failed for user "tcms_app"`。手工执行一次同一个脚本，它建账号，把 schema 和里面的表、函数都交给 `tcms_app`，数据和令牌都不动，然后重启 tcms：

```bash
docker compose up -d
docker compose exec postgres sh /docker-entrypoint-initdb.d/01-tcms-app.sh
docker compose restart tcms
```

这个脚本可以重复执行。改了 `TCMS_DB_PASSWORD` 以后也执行一次，账号的密码跟着改。

**备份**：

- 数据库（提议、令牌、审计、索引）用 `pg_dump` 导：

  ```bash
  docker compose exec -T postgres pg_dump -U tcms -d tcms -n tcms -Fc > tcms-$(date +%F).dump
  ```

- 上下文仓库是真相。配了远端时，远端就是一份异地备份；没配时打一个 git bundle：

  ```bash
  docker compose exec -T tcms git -C /var/lib/tcms/context bundle create - --all > context-$(date +%F).bundle
  ```

  索引丢了可以从仓库重建：`tcms reindex --full`。

**还没有的**：远端的 webhook（推上去马上同步，现在最多等一个同步间隔）。

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

改用普通账号（2026-11-24，同一台机器）。镜像是在 `tcms:latest` 上换进本机编的 `tcms`（代码和 main 一样），没有从 `Dockerfile` 重新构建，这次没改 `Dockerfile`：

| 测了什么 | 结果 |
| --- | --- |
| 空数据卷第一次启动 | 初始化脚本建了 `tcms_app`（不是超级用户、没有 `BYPASSRLS`）和归它的 schema `tcms`；两个服务都 `healthy`，tcms 的启动日志里没有“超级用户”的提醒；表都归 `tcms_app`，`items`、`item_versions`、`changes` 开了行级安全 |
| 写上范围直接查 `items`（3 条，全公司、宏观岗位、法务岗位各 1 条） | 用 `tcms_app` 连，范围写全公司和宏观岗位时只出来 2 条；用超级用户 `tcms` 连，3 条都出来 |
| 老的数据卷：先用原来的 compose（超级用户连）起，导入 2 条、发 1 个令牌，再换成新的 compose | tcms 起不来，日志里是 `password authentication failed for user "tcms_app"`；执行上面三条命令以后 `healthy`，没有提醒，8 张表、2 个序列和函数 `visible_scope` 都归 `tcms_app`，条目和令牌都在，行级安全照样起作用 |
| 脚本再执行一次 | 只改了密码、授权和 schema 的属主，别的不动 |

推到远端（2026-11-23，同一台机器，镜像 324 MB，多了 ssh 客户端）。远端是本机起的一个带 Basic 认证的 git 服务（`git http-backend` 外面包一层，只认一个用户名和令牌），不是真的 GitHub、Gitea：

| 测了什么 | 结果 |
| --- | --- |
| 空卷、远端已经有 2 条 | 启动后第一次同步快进到远端的提交，`tcms search` 搜得到 |
| `tcms import`（另起一个容器） | 导入的提交推到了远端；服务下一次同步时索引跟着更新 |
| 人在自己的克隆里改了一条、推上去 | 3 秒（`TCMS_SYNC_SECS=3`）内服务快进过去，检索到新内容 |
| 服务器上的提交和人推的改动改了同一行 | 日志写一次“合并不了”和冲突的文件，远端多了 `tcms/main-unmerged`；人在克隆里合并它、推到 `main` 以后，服务快进过去，合并后的内容进了索引 |
| 没给令牌、令牌不对（在容器外用 `tcms` 跑的） | 马上失败（“terminal prompts disabled”“Authentication failed”），不卡住等密码；导入照样在本地提交 |

没测的：放在 HTTPS 反向代理后面让别的机器连 `/mcp`；从旧版本升级（还没有发过版本）；真的 GitHub、Gitea；ssh 远端。
