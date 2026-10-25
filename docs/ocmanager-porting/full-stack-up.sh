#!/bin/sh
# OCManager 全栈（16 容器）在 DragonOS 上的启动脚本 v2：
# bridge 网络 ocm-net + 服务名 DNS；基础设施就绪后再启动依赖服务。
stty -tostop
dragon-docker-start
NAMES="mysql redis zookeeper kafka clickhouse footstone msg-etl sysdiagnose frontend gateway api asset-manager heartbeat task-manager file-svc data-proxy alpha"
docker rm -f $NAMES >/dev/null 2>&1
docker network rm ocm-net >/dev/null 2>&1
docker network create ocm-net >/dev/null 2>&1; echo NET=$?
N="--network ocm-net"
TOK=devtoken
HMAC=devsecret0123456789abcdef01234567
CA_PASS=devpass
mkdir -p /data/ocm/ca /data/ocm/asset-manager /data/ocm/task-manager /data/ocm/files

docker run -d --name mysql $N --network-alias mysql --entrypoint bash mysql:8.0.35 -c 'mysqld --initialize-insecure --user=root; exec mysqld --user=root' >/dev/null; echo MDB=$?
docker run -d --name redis $N --network-alias redis --network-alias ocm-redis redis:7.2.3-alpine >/dev/null; echo RDS=$?
docker run -d --name zookeeper $N --network-alias zookeeper -e ALLOW_ANONYMOUS_LOGIN=yes bitnamilegacy/zookeeper:3.8.3 >/dev/null; echo ZK=$?
docker run -d --name kafka $N --network-alias kafka --network-alias ocm-kafka \
 -e KAFKA_CFG_NODE_ID=1 -e KAFKA_CFG_PROCESS_ROLES=controller,broker \
 -e KAFKA_CFG_CONTROLLER_QUORUM_VOTERS=1@kafka:9093 \
 -e KAFKA_CFG_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 \
 -e KAFKA_CFG_ADVERTISED_LISTENERS=PLAINTEXT://kafka:9092 \
 -e KAFKA_CFG_LISTENER_SECURITY_PROTOCOL_MAP=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT \
 -e KAFKA_CFG_CONTROLLER_LISTENER_NAMES=CONTROLLER -e ALLOW_PLAINTEXT_LISTENER=yes \
 bitnamilegacy/kafka:3.5.1 >/dev/null; echo KFK=$?
docker run -d --name clickhouse $N --network-alias clickhouse clickhouse/clickhouse-server:23.8.8.20 >/dev/null; echo CH=$?

# ---- 就绪等待（最多 300s，每 5s 重试）----
i=0
while [ $i -lt 60 ]; do
  docker exec mysql mysqladmin -uroot --skip-password ping >/dev/null 2>&1 && break
  i=$((i+1)); sleep 5
done; echo MDB_READY=$i
i=0
while [ $i -lt 60 ]; do
  docker exec redis redis-cli ping >/dev/null 2>&1 && break
  i=$((i+1)); sleep 5
done; echo RDS_READY=$i
i=0
while [ $i -lt 60 ]; do
  docker exec clickhouse clickhouse-client -q 'select 1' >/dev/null 2>&1 && break
  i=$((i+1)); sleep 5
done; echo CH_READY=$i
i=0
while [ $i -lt 60 ]; do
  docker exec kafka kafka-topics.sh --bootstrap-server kafka:9092 --list >/dev/null 2>&1 && break
  i=$((i+1)); sleep 5
done; echo KFK_READY=$i

docker exec clickhouse clickhouse-client -q 'CREATE DATABASE IF NOT EXISTS msg_etl'; echo CK1=$?
docker exec clickhouse clickhouse-client -q 'CREATE DATABASE IF NOT EXISTS footstone'; echo CK2=$?
docker exec mysql mysql -uroot --skip-password -e "create database if not exists footstone; create database if not exists msg_etl; create database if not exists sysdiagnose; CREATE USER IF NOT EXISTS 'root'@'%' IDENTIFIED BY ''; GRANT ALL ON *.* TO 'root'@'%' WITH GRANT OPTION; FLUSH PRIVILEGES;" 2>/dev/null; echo MDB_DB=$?
for f in /migrations/footstone/*.sql; do docker exec -i mysql mysql -uroot --skip-password footstone < "$f" 2>&1 | tail -1; echo "MIG_FOOT_$f=$?"; done
for f in /migrations/msg_etl/*.sql; do docker exec -i clickhouse clickhouse-client --multiquery < "$f" 2>&1 | tail -1; echo "MIG_CK_$f=$?"; done
for f in /migrations/sysdiagnose/*.sql; do docker exec -i mysql mysql -uroot --skip-password sysdiagnose < "$f" 2>&1 | tail -1; echo "MIG_SD_$f=$?"; done

docker run -d --name asset-manager $N --network-alias asset-manager --network-alias ocm-asset-manager \
 -e OCM_CA_KEY_PASSPHRASE=$CA_PASS -e OCM_GATEWAY_SERVER_HOST= \
 -v /tms-configs/asset-manager-trpc_go.yaml:/app/trpc_go.yaml \
 -v /data/ocm/ca:/data/ocm/ca -v /data/ocm/asset-manager:/data/ocm/asset-manager \
 oc-tms-asset-manager:dev >/dev/null; echo ASM=$?
i=0
while [ $i -lt 30 ]; do
  [ -s /data/ocm/ca/ca.crt ] && [ -s /data/ocm/ca/ca.key ] && break
  i=$((i+1)); sleep 2
done; echo CA_READY=$i
docker run -d --name gateway $N --network-alias gateway --network-alias ocm-gateway \
 -e TASK_MANAGER_AUTH_TOKEN=$TOK -v /tms-configs/gateway-trpc_go.yaml:/app/trpc_go.yaml \
 -v /data/ocm/ca:/data/ocm/ca:ro oc-tms-gateway:dev >/dev/null; echo GW=$?
docker run -d --name api $N --network-alias api --network-alias ocm-api \
 -e TASK_MANAGER_AUTH_TOKEN=$TOK -e TMS_API_GW_HMAC_SECRET=$HMAC -e TMS_API_GW_HMAC_STRICT=false \
 -v /tms-configs/api-trpc_go.yaml:/app/trpc_go.yaml oc-tms-api:dev >/dev/null; echo API=$?
docker run -d --name heartbeat $N --network-alias heartbeat -e REDIS_PWD= \
 -v /tms-configs/heartbeat-trpc_go.yaml:/app/trpc_go.yaml oc-tms-heartbeat:dev >/dev/null; echo HB=$?
docker run -d --name task-manager $N --network-alias task-manager -e TASK_MANAGER_AUTH_TOKEN=$TOK \
 -v /tms-configs/task-manager-trpc_go.yaml:/app/trpc_go.yaml \
 -v /data/ocm/task-manager:/data/ocm/task-manager oc-tms-task-manager:dev >/dev/null; echo TM=$?
docker run -d --name file-svc $N --network-alias file-svc \
 -v /tms-configs/file-svc-trpc_go.yaml:/app/trpc_go.yaml \
 -v /data/ocm/files:/data/ocm/files oc-tms-file-svc:dev >/dev/null; echo FS=$?
docker run -d --name data-proxy $N --network-alias data-proxy -e REDIS_PWD= -e DATAPROXY_ADMIN_ADDR=:50047 \
 -v /tms-configs/data-proxy-trpc_go.yaml:/app/trpc_go.yaml oc-tms-data-proxy:dev >/dev/null; echo DP=$?

docker run -d --name footstone $N --network-alias footstone \
 -e SERVICE=footstone -e MYSQL_HOST=mysql -e MYSQL_PORT=3306 -e MYSQL_USER=root -e MYSQL_PWD= \
 -e REDIS_HOST=redis -e REDIS_PORT=6379 -e REDIS_USER=default -e REDIS_PWD= \
 -e KAFKA_HOST=kafka -e KAFKA_PORT=9092 \
 -e CLICKHOUSE_HOST=clickhouse -e CLICKHOUSE_PORT=9000 -e CLICKHOUSE_HTTP_PORT=8123 \
 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PWD= \
 -e JWT_SECRET=devsecret -e FOOTSTONE=ip://footstone:17778 -e MSG_ETL=ip://msg-etl:7778 \
 -e FOOTSTONE_HTTP_PORT=20000 -e FOOTSTONE_TRPC_PORT=17778 -e FOOTSTONE_WS_PORT=20002 -e FOOTSTONE_HEALTH_PORT=20001 \
 oc-footstone:dev >/dev/null; echo FOOT=$?
docker run -d --name msg-etl $N --network-alias msg-etl \
 -e SERVICE=msg-etl -e JWT_SECRET=devsecret -e KAFKA_HOST=kafka -e KAFKA_PORT=9092 \
 -e CLICKHOUSE_HOST=clickhouse -e CLICKHOUSE_PORT=9000 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PWD= \
 -e FOOTSTONE=ip://footstone:17778 \
 -e MSG_ETL_HTTP_PORT=8080 -e MSG_ETL_TRPC_PORT=7778 -e MSG_ETL_HEALTH_PORT=8081 \
 -e TMS=http://ocm-api:8080 oc-msg-etl:dev >/dev/null; echo ETL=$?
docker run -d --name sysdiagnose $N --network-alias sysdiagnose \
 -e SERVICE=sysdiagnose -e JWT_SECRET=devsecret \
 -e MYSQL_HOST=mysql -e MYSQL_PORT=3306 -e MYSQL_USER=root -e MYSQL_PWD= \
 -e REDIS_HOST=redis -e REDIS_PORT=6379 -e REDIS_USER=default -e REDIS_PWD= \
 -e CLICKHOUSE_HOST=clickhouse -e CLICKHOUSE_PORT=9000 -e CLICKHOUSE_HTTP_PORT=8123 \
 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PWD= \
 -e FOOTSTONE=ip://footstone:17778 -e TMS=http://ocm-api:8080 -e TMS_USER_ID=18 \
 -e SYSDIAGNOSE_HTTP_PORT=8080 -e SYSDIAGNOSE_TRPC_PORT=7778 -e SYSDIAGNOSE_HEALTH_PORT=8081 \
 oc-sysdiagnose:dev >/dev/null; echo SD=$?
docker run -d --name frontend $N --network-alias frontend \
 -v /nginx.docker.conf:/etc/nginx/nginx.conf:ro oc-frontend:dev >/dev/null; echo FE=$?
sleep 45
docker ps --format '{{.Names}} {{.Status}}'
echo UPALL_END
