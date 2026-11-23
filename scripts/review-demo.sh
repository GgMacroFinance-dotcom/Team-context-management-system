#!/usr/bin/env bash
# 搭一个演示用的审核页：造一个小上下文仓库、发两个令牌、起 tcms serve、让 agent 提 4 条提议。
# 然后可以自己在浏览器里点，或者用 scripts/review-walk.js 自动点一遍。
#
#   TCMS_DATABASE_URL=postgres://... scripts/review-demo.sh [目录] [端口]
#
# 用 schema tcms_demo（每次先用 psql 删掉重建），目录默认 ./tcms-demo（每次清空），端口默认 7351。
# Ctrl-C 停服务。演示用的令牌写在目录里的 reviewer.txt、agent.txt。
set -euo pipefail

: "${TCMS_DATABASE_URL:?要先设 TCMS_DATABASE_URL}"
DIR=${1:-./tcms-demo}
PORT=${2:-7351}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TCMS=${TCMS_BIN:-$ROOT/target/debug/tcms}
[ -x "$TCMS" ] || (cd "$ROOT" && cargo build -q -p tcms-cli)

rm -rf "$DIR"
mkdir -p "$DIR/repo"
DIR=$(cd "$DIR" && pwd)
export TCMS_SCHEMA=tcms_demo TCMS_REPO=$DIR/repo TCMS_ACTOR=demo TCMS_GIT_EMAIL=review@example.com

# 上下文仓库：一条规则、两条事实（一条过了复核日期）、一条团队的流程（也过了复核日期）。
cd "$DIR/repo"
git init -q
git config user.name demo
git config user.email demo@example.com
mkdir -p org role-macro team-research
printf 'teams:\n  research:\n    roles: [macro]\n' > teams.yaml
printf '## 数字要有来源\n<!-- ctx id=ctx_01JDX4M6Q8AAAAAAAAAAAAAAAA kind=rule -->\n所有数字注明来源。\n' > org/规矩.md
printf '# 数据源\n\n## 宏观数据从哪里取\n<!-- ctx id=ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC kind=fact tags="数据, 口径" -->\nCPI 用国家统计局的数。\n\n## PMI 的口径\n<!-- ctx id=ctx_01JDX4M6Q8BBBBBBBBBBBBBBBB kind=fact review_by=2026-10-01 owner=alice -->\nPMI 指官方制造业 PMI。\n' > role-macro/数据源.md
printf '## 周报用表格\n<!-- ctx id=ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD kind=procedure review_by=2026-11-01 -->\n周报用表格，日期写全。\n' > team-research/周报.md
git add -A
git commit -qm init
cd "$DIR"

command -v psql >/dev/null || { echo "要用 psql 删掉旧的 schema tcms_demo"; exit 1; }
psql "$TCMS_DATABASE_URL" -qc "DROP SCHEMA IF EXISTS tcms_demo CASCADE" 2>/dev/null
"$TCMS" reindex >/dev/null
secret() { python3 -c 'import json,sys; print(json.load(sys.stdin)["secret"])'; }
"$TCMS" token add --name "I.P. 宏观" --role macro --json | secret > agent.txt
"$TCMS" token add --reviewer --name "王审核" --json | secret > reviewer.txt

"$TCMS" serve --listen "127.0.0.1:$PORT" > serve.log 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null' EXIT INT TERM
for _ in $(seq 50); do
  curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break
  sleep 0.2
done

# agent 经 HTTP 的 MCP 提 4 条：改一条、新建一条和现有的相近、新建一条、归档一条。
propose() {
  curl -sf "http://127.0.0.1:$PORT/mcp" \
    -H "Authorization: Bearer $(cat agent.txt)" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_propose\",\"arguments\":$1}}" \
    | python3 -c 'import json,sys; r=json.load(sys.stdin)["result"]; print(r["structuredContent"]["proposal_id"], r["structuredContent"]["title"])'
}
propose '{"action":"update","item_id":"ctx_01JDX4M6Q8CCCCCCCCCCCCCCCC","draft":{"body":"CPI、PPI 都用国家统计局的数，周报里注明。"},"reason":"PPI 也是统计局的","evidence":["2026-11 周报第 3 段"]}'
propose '{"action":"create","draft":{"title":"宏观数据该从哪里取","body":"统一走 Finance Macro Data API，接口里有统计局和 FRED。","kind":"fact","tags":["数据"]},"reason":"新接口上线了"}'
propose '{"action":"create","draft":{"title":"CPI 发布日","body":"每月 9 日到 10 日之间，统计局官网公布。\n\n- 遇节假日顺延"},"reason":"大家老问","evidence":["https://www.stats.gov.cn"]}'
propose '{"action":"archive","item_id":"ctx_01JDX4M6Q8DDDDDDDDDDDDDDDD","reason":"周报改成图表了"}'

echo
echo "审核页：http://127.0.0.1:$PORT/review"
echo "审核的人的令牌（登录用）：$(cat reviewer.txt)"
echo "自动点一遍：node scripts/review-walk.js http://127.0.0.1:$PORT $DIR/reviewer.txt $DIR/agent.txt"
echo "Ctrl-C 停服务"
wait $SERVER
