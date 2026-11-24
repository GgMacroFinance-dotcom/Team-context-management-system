#!/bin/sh
# 单独部署（deploy/docker-compose.yml）时，建 tcms 用的普通账号 tcms_app 和它拥有的 schema tcms。
# PostgreSQL 镜像第一次启动（数据卷还是空的）时自动执行这个文件，之后不再执行。
#
# 镜像建的 POSTGRES_USER（tcms）是超级用户，数据库的行级安全对超级用户不起作用（docs/architecture.md 第 5 节），
# 所以 tcms 服务不用它连，只拿它管库、备份。密码从环境变量 TCMS_DB_PASSWORD 读。
#
# 可以重复执行。老的数据卷（之前 tcms 服务用超级用户连，schema 里的东西都归它）手工执行一次，
# 把 schema 和里面的表、函数都交给 tcms_app，见 docs/deploy-docker.md“老的数据卷”：
#   docker compose exec postgres sh /docker-entrypoint-initdb.d/01-tcms-app.sh
set -eu

if [ -z "${TCMS_DB_PASSWORD:-}" ]; then
  echo "TCMS_DB_PASSWORD 没有设置" >&2
  exit 1
fi

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  -v db="$POSTGRES_DB" -v pw="$TCMS_DB_PASSWORD" <<'SQL'
-- 没有就建；已经有了就确保它是普通账号（不是超级用户、不能绕过行级安全），密码以环境变量为准。
SELECT 'CREATE ROLE tcms_app LOGIN'
 WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tcms_app') \gexec
ALTER ROLE tcms_app LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE PASSWORD :'pw';
GRANT CONNECT ON DATABASE :"db" TO tcms_app;

SELECT 'CREATE SCHEMA tcms AUTHORIZATION tcms_app'
 WHERE NOT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'tcms') \gexec
ALTER SCHEMA tcms OWNER TO tcms_app;

-- 老的数据卷：表（连带它们的自增序列、索引、行级安全策略）和函数原来归超级用户，交给 tcms_app。
SELECT format('ALTER TABLE tcms.%I OWNER TO tcms_app', c.relname)
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'tcms' AND c.relkind IN ('r', 'p') AND c.relowner <> 'tcms_app'::regrole \gexec
SELECT format('ALTER FUNCTION %s OWNER TO tcms_app', p.oid::regprocedure)
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE n.nspname = 'tcms' AND p.proowner <> 'tcms_app'::regrole \gexec
SQL
