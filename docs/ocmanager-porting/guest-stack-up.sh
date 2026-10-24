#!/bin/sh
stty -tostop
dragon-docker-start
# 幂等：清理上次运行残留的容器（数据都在容器内，删掉即干净重建）
docker rm -f mdb rds ch kfk foot etl >/dev/null 2>&1
docker run -d --name mdb --network host --entrypoint bash mysql:8.0.35 -c 'mysqld --initialize-insecure --user=root; exec mysqld --user=root' >/dev/null 2>&1; echo MDB=$?
docker run -d --name rds --network host redis:7.2.3-alpine >/dev/null 2>&1; echo RDS=$?
docker run -d --name ch --network host clickhouse/clickhouse-server:23.8.8.20 >/dev/null 2>&1; echo CH=$?
docker run -d --name kfk --network host \
 -e KAFKA_CFG_NODE_ID=1 -e KAFKA_CFG_PROCESS_ROLES=controller,broker \
 -e KAFKA_CFG_CONTROLLER_QUORUM_VOTERS=1@localhost:9093 \
 -e KAFKA_CFG_LISTENERS=PLAINTEXT://:9092,CONTROLLER://:9093 \
 -e KAFKA_CFG_ADVERTISED_LISTENERS=PLAINTEXT://localhost:9092 \
 -e KAFKA_CFG_LISTENER_SECURITY_PROTOCOL_MAP=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT \
 -e KAFKA_CFG_CONTROLLER_LISTENER_NAMES=CONTROLLER -e ALLOW_PLAINTEXT_LISTENER=yes \
 bitnamilegacy/kafka:3.5.1 >/dev/null 2>&1; echo KFK=$?
sleep 150
docker exec rds redis-cli ping; echo V_RDS=$?
docker exec ch clickhouse-client -q 'select 1'; echo V_CH=$?
docker exec ch clickhouse-client -q 'CREATE DATABASE IF NOT EXISTS msg_etl'; echo CK1=$?
docker exec ch clickhouse-client -q 'CREATE DATABASE IF NOT EXISTS footstone'; echo CK2=$?
docker exec mdb mysql -uroot --skip-password -e "create database if not exists footstone; create database if not exists msg_etl; CREATE USER IF NOT EXISTS 'root'@'%' IDENTIFIED BY ''; GRANT ALL ON *.* TO 'root'@'%' WITH GRANT OPTION; FLUSH PRIVILEGES;"; echo MDB_DB=$?
docker exec kfk kafka-topics.sh --bootstrap-server localhost:9092 --list >/dev/null 2>&1; echo KFK_OK=$?
# 应用 footstone(MySQL) 与 msg-etl(ClickHouse) 迁移（对应 compose 的
# docker-entrypoint-initdb.d；本环境用 --initialize-insecure 手动建库，需显式执行）
for f in /migrations/footstone/*.sql; do docker exec -i mdb mysql -uroot --skip-password footstone < "$f" 2>&1 | tail -1; echo "MIG_FOOT_$f=$?"; done
for f in /migrations/msg_etl/*.sql; do docker exec -i ch clickhouse-client --multiquery < "$f" 2>&1 | tail -1; echo "MIG_CK_$f=$?"; done
docker run -d --name etl --network host \
 -e SERVICE=msg-etl -e KAFKA_HOST=127.0.0.1 -e KAFKA_PORT=9092 \
 -e CLICKHOUSE_HOST=127.0.0.1 -e CLICKHOUSE_PORT=9000 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PWD= \
 -e FOOTSTONE=ip://127.0.0.1:17778 \
 -e MSG_ETL_HTTP_PORT=8080 -e MSG_ETL_TRPC_PORT=7778 -e MSG_ETL_HEALTH_PORT=8081 \
 -e TMS=http://127.0.0.1:8080 \
 oc-msg-etl:dev >/dev/null 2>&1; echo ETL=$?
docker run -d --name foot --network host \
 -e SERVICE=footstone -e MYSQL_HOST=127.0.0.1 -e MYSQL_PORT=3306 -e MYSQL_USER=root -e MYSQL_PWD= \
 -e REDIS_HOST=127.0.0.1 -e REDIS_PORT=6379 -e REDIS_USER=default -e REDIS_PWD= \
 -e KAFKA_HOST=127.0.0.1 -e KAFKA_PORT=9092 \
 -e CLICKHOUSE_HOST=127.0.0.1 -e CLICKHOUSE_PORT=9000 -e CLICKHOUSE_USER=default -e CLICKHOUSE_PWD= \
 -e JWT_SECRET=devsecret -e FOOTSTONE=ip://127.0.0.1:17778 -e MSG_ETL=ip://127.0.0.1:7778 \
 -e FOOTSTONE_HTTP_PORT=21000 -e FOOTSTONE_TRPC_PORT=17778 -e FOOTSTONE_WS_PORT=21001 -e FOOTSTONE_HEALTH_PORT=21002 \
 oc-footstone:dev >/dev/null 2>&1; echo FOOT=$?
sleep 30
docker ps --format '{{.Names}} {{.Status}}'
docker logs --tail 15 foot > /root/foot.log 2>&1
echo UP_END
