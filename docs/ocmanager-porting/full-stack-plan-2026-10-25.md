# OCManager 全量容器移植计划（2026-10-25）

## 目标

把 OCManager 的**全部**容器（不只是核心三服务）跑在 DragonOS 上，并做端到端验收；
验收通过后才算移植完成。

## 容器清单（16）

| 层 | 容器 | 镜像 | 端口（容器内） | 备注 |
|---|---|---|---|---|
| infra | ocm-mysql | mysql:8.0.35 | 3306 | 已有 |
| infra | ocm-redis | redis:7.2.3-alpine | 6379 | 已有 |
| infra | ocm-zookeeper | bitnamilegacy/zookeeper:3.8.3 | 2181 | 已有（seed） |
| infra | ocm-kafka | bitnamilegacy/kafka:3.5.1 | 9092/9093 | 已有 |
| infra | ocm-clickhouse | clickhouse/clickhouse-server:23.8.8.20 | 9000/8123 | 已有 |
| app | oc-footstone | oc-footstone:dev | 21000/17778/21001 | 已有 |
| app | oc-msg-etl | oc-msg-etl:dev | 8080/7778/8081 | 已有 |
| app | oc-sysdiagnose | oc-sysdiagnose:dev | 21006/21007/21008 | 已有 |
| app | oc-frontend | oc-frontend:dev | 80 | **本次构建** |
| tms | ocm-gateway | oc-tms-gateway:dev | 8989/50041 | **本次构建** |
| tms | ocm-api | oc-tms-api:dev | 8080 | **本次构建** |
| tms | ocm-asset-manager | oc-tms-asset-manager:dev | 50044 | **本次构建**（CGO/musl） |
| tms | ocm-heartbeat | oc-tms-heartbeat:dev | 50043 | **本次构建** |
| tms | ocm-task-manager | oc-tms-task-manager:dev | 50042 | **本次构建** |
| tms | ocm-file-svc | oc-tms-file-svc:dev | 50045 | **本次构建** |
| tms | ocm-data-proxy | oc-tms-data-proxy:dev | 50046/50047 | **本次构建** |

> `zookeeper` 是否启用取决于 Kafka 模式：本环境 kafka 用 KRaft（无 zk），
> zookeeper 容器仍按其镜像启动并保持运行，以满足“全部容器可运行”。

## 构建方式（宿主，2026-10-25）

- 6 个无 CGO 的 TMS 服务：`docker build -f <svc>/Dockerfile`，
  `GO_IMAGE=golang:1.25-alpine`、`RUNTIME_IMAGE=alpine:3.19`、
  `GOPROXY=https://goproxy.cn,direct`、`APK_MIRROR=dl-cdn.alpinelinux.org`；
- `asset-manager` 需要 CGO + musl（mattn/go-sqlite3），同样用上述 Dockerfile 构建；
- 前端：`Dockerfile.frontend`，`NODE_IMAGE=node:18-alpine`、
  `NGINX_IMAGE=nginx:1.25-alpine`。

镜像通过 `docker save | zstd` 打包后注入 guest 磁盘，启动后 `docker load`。

## 网络模型

TMS 配置里用服务名互访（如 `ip://task-manager:50042`、`ip://gateway:50041`），
因此需要**容器 DNS 或等价物**：

1. 首选：guest 内 `docker network create` + 容器名解析（compose 用 bridge + name）；
   需先在 guest 验证 bridge/DNS（可能缺 netfilter/NAT）。
2. 回退：全部 `--network host`，把配置中的服务名改写为 `127.0.0.1`（各服务端口互不冲突：
   8989/50041/8080/50044/50043/50042/50045/50046/50047），并用环境变量覆盖端口。

## 凭据与配置

- `TASK_MANAGER_AUTH_TOKEN`、`TMS_API_GW_HMAC_SECRET`（api 严格模式）、
  `OCM_CA_KEY_PASSPHRASE`（asset-manager 生成 CA，gateway 只读挂载）、`REDIS_PWD`；
- 配置文件来自 `tms/deploy/configs/*-trpc_go.yaml`，按网络模型渲染后挂载；
- CA 目录：asset-manager 写、gateway 读（`/data/ocm/ca`）。

## 端到端验收项

1. 16 个容器全部 `Up`，无重启循环；
2. 每个服务端口可连、健康端点/管理端口可用（TMS 各服务有 trpc admin/health 端口）；
3. 三条业务链路：
   - 前端（nginx）可访问，静态页返回 200，`/api` 反代到 footstone；
   - footstone：登录 JWT → `GetCurrentUser`/`ListProjects`（已有）；
   - TMS：api → gateway → asset-manager/task-manager/file-svc 的至少一条真实调用；
4. Kafka/Redis/ClickHouse/MySQL 数据面在服务启动后仍正常（produce/consume 或查询）；
5. 关机后 `Filesystem state: clean`（或 QEMU exit 0）。

## 进度（2026-10-25 深夜，#290）

- [x] 镜像可用性盘点；TMS 7 镜像与 frontend 镜像构建
- [x] guest 内加载镜像（8/8）
- [x] 网络模型验证：**bridge + 容器名 DNS**（`getent hosts <svc>` 可用），
      因此配置里的服务名零改动
- [x] 16 容器启动：`full-stack-up.sh` 串行版一次跑通，串行就绪探活约 10 分钟
- [x] 端到端验收（见 `status-overview.md` §3.50 的表）：
      前端 / 三个健康端点 / JWT 登录 / GetCurrentUser / ListProjects /
      msg-etl 指标 / sysdiagnose 命令列表 / api→task-manager /
      api→asset-manager 写入 + 唯一约束 / 120 次并发 exec 压测
- [ ] kafka(JVM) 偶发起不来 → msg-etl 连带退出（`status-overview.md` §3.50 残留 1）
- [ ] asset-manager 第二次 SQLite 写入报 `operation not supported`（残留 2）

## 验收脚本口径（务必按此写，否则会得到「空响应」的假失败）

> 完整操作手册（含镜像准备、启动、坑点与排查）见
> `docs/ocmanager-porting/running-ocmanager-on-dragonos.md`。

1. **变更类 api 端点需要网关 HMAC 签名**。即使容器里
   `TMS_API_GW_HMAC_STRICT=false`，只要 `TMS_API_GW_HMAC_SECRET` 非空，
   `POST /api/v1/{assets/add,batch_add,remove,ops_tasks/publish,cancel,
   artifacts/upload,remove,agents/upgrade,restart}` 仍会被校验：

   ```
   canonical = METHOD "\n" PATH "\n" X-User-Name "\n" X-User-Role "\n"
               TIMESTAMP "\n" NONCE "\n" SHA256_HEX(body)
   X-Signature = hex(HMAC-SHA256(TMS_API_GW_HMAC_SECRET, canonical))
   ```

   另需 `X-User-Name`（必填）、`X-User-Role`、`X-Signature-Timestamp`（Unix 秒，
   偏差 >300s 拒绝）、`X-Signature-Nonce`（10 分钟内不可重复）。
   **时间戳必须用 guest 时钟**：guest 是 UTC，宿主时区与之相差 8 小时。
2. footstone 登录：`User/IdentifyAccount`，字段是 **`loginname`**（不是 account），
   密码传 `hex(SHA512("Admin123456@"))`；服务端再做 `SHA512(password + salt)`。
3. `ListProjects` 的分页在 pb 里是嵌套的 **`page_request`**：
   `{"page_request":{"page_size":10,"page_number":1}}`。
4. 容器镜像内**没有 curl**；用 `docker exec clickhouse wget` 发起请求。
   wget 对非 2xx 会丢弃响应体并以退出码 6 结束，取证必须加
   `--content-on-error`（否则「空响应」其实是 401/400）。
5. `docker exec` 每次都会打印
   `WARNING: --criu ignored (criu binary from $PATH is used); do not use`，属噪声。
6. 容器退出后其 DNS 记录立即消失，`getent hosts <name>` 失败时要先
   `docker ps -a` 确认容器是否还在运行。
