# 在 DragonOS 上运行 OCManager：条件、步骤与坑点

本文是**可执行的操作手册**：从镜像准备到 16 容器全栈启动、端到端验收，
并记录已经踩过的坑（含内核缺陷与部署口径）。验收结果见文末“参考验收”。

- 适用代码基线：分支 `ocmanager-porting-2026-10`，内核 ≥ `#309`
  （依赖 `85cf41cf`、`eb8f7582`、`cf2ed782` 三个修复，见 §9.1）。
- 目标形态：guest 内 docker 跑 **16 个容器**（5 infra + 4 app + 7 TMS），
  bridge 网络 + 服务名 DNS，与 compose 的端口/配置保持一致。

---

## 1. 前提条件

| 项 | 要求 | 说明 |
|---|---|---|
| 宿主 | Linux x86_64 + KVM | QEMU 需要 `-cpu host` + `x2apic` |
| 内存 | guest **12G** | 4G/6G 曾出现缺页/内存破坏类挂死，不要低于 12G |
| 磁盘 | 16G 镜像（`bin/disk-image-x86_64.img`） | 容器镜像约 9G，剩余空间要留够 |
| 工具链 | Rust（`make kernel`）、`nix develop`（可选） | `nix run .#yolo-x86_64` 可一键编译+启动 |
| 其它 | docker（宿主，用于构建/打包镜像）、`zstd`、`losetup` | guest 内**没有** curl/wget/zstd |

内核编译（worktree 根目录）：`make kernel`（约 2 分钟）。
**不要**在 nix 环境外跑 `make -C kernel fmt`：会整体重排无关文件并触发 clippy 噪声。

---

## 2. 一次性准备：镜像 → 打包 → 注入磁盘

### 2.1 构建 TMS / 前端镜像（宿主执行）

在 OCManager 仓库下用各自 Dockerfile 构建，Go 服务需要指定基础镜像与镜像源：

```sh
docker build -f <svc>/Dockerfile \
  --build-arg GO_IMAGE=golang:1.25-alpine \
  --build-arg RUNTIME_IMAGE=alpine:3.19 \
  --build-arg GOPROXY=https://goproxy.cn,direct \
  --build-arg APK_MIRROR=dl-cdn.alpinelinux.org \
  -t oc-tms-<svc>:dev <svc>
```

需要构建的 7 个 TMS 服务：`gateway api asset-manager heartbeat task-manager
file-svc data-proxy`；前端单独用 `Dockerfile.frontend`
（`NODE_IMAGE=node:18-alpine`、`NGINX_IMAGE=nginx:1.25-alpine`）→ `oc-frontend:dev`。
`footstone / msg-etl / sysdiagnose` 镜像走 seed 磁盘（下一节）。

打包并传入 guest（宿主起 HTTP 服务，guest 用容器内 wget 拉取）：

```sh
docker save oc-tms-{gateway,api,asset-manager,heartbeat,task-manager,file-svc,data-proxy}:dev \
            oc-frontend:dev | gzip -1 > /tmp/ocm-serve/ocm-extra-images.tar.gz
cd /tmp/ocm-serve && python3 -m http.server 8000 --bind 0.0.0.0
# guest 内（见 §5.3）：docker load < 拉下来的 tar.gz
```

### 2.2 向磁盘注入 seed（**每次重建盘都必须重做**）

```sh
LOOP=$(sudo losetup -fP --show bin/disk-image-x86_64.img); sudo mount ${LOOP}p1 /mnt/ocm-inject
sudo mkdir -p /mnt/ocm-inject/var/lib/docker && sudo chmod 0710 /mnt/ocm-inject/var/lib/docker
zstd -dc /path/to/var-lib-docker-seed.tar.zst | sudo tar -x -C /mnt/ocm-inject/var/lib/docker \
     --numeric-owner --xattrs --xattrs-include='*' 2>/dev/null
# 迁移 SQL（footstone/mysql、msg_etl/clickhouse、sysdiagnose/mysql）
sudo mkdir -p /mnt/ocm-inject/migrations/{footstone,msg_etl,sysdiagnose}
sudo cp manager/backend/migrations/footstone/sql/mysql/v1.0.0/*.sql        /mnt/ocm-inject/migrations/footstone/
sudo cp manager/backend/migrations/msg-etl/sql/clickhouse/v1.0.{0,1}/*.sql /mnt/ocm-inject/migrations/msg_etl/
sudo cp manager/backend/migrations/sysdiagnose/sql/mysql/v1.0.0.0/*.sql    /mnt/ocm-inject/migrations/sysdiagnose/
# 部署脚本与配置
sudo cp docs/ocmanager-porting/full-stack-up.sh /mnt/ocm-inject/fs.sh
sudo cp tms/deploy/configs/*-trpc_go.yaml       /mnt/ocm-inject/tms-configs/
sudo cp manager/deployments/compose/nginx.docker.conf /mnt/ocm-inject/
sudo sync && sudo umount /mnt/ocm-inject && sudo losetup -d $LOOP
```

盘内最终需要存在：`/fs.sh`、`/tms-configs/*.yaml`、`/nginx.docker.conf`、
`/migrations/{footstone,msg_etl,sysdiagnose}/`、`/var/lib/docker`（预置镜像）。

净盘重建（可选）：`sudo rm -f bin/disk-image-x86_64.img && ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage`。

---

## 3. 启动 DragonOS

```sh
cd tools
DRAGONOS_QEMU_MEMORY=12G ROOT_PATH=/home/sparkzky/DragonOS-ocmanager \
  bash run-qemu.sh --bios=legacy --display=nographic
```

- 串口输出写到 `serial_opt.txt`（在仓库根或 `tools/../`），**读它要 `sudo`**。
- 等到 `Please press Enter to activate this console.` 后，**先按一次回车**；
  激活后的**第一条命令经常被吞**，需要重发一次。
- QEMU 偶发 `failed`（残留 vmstate/端口）：重试一次即可。
  杀进程用：`for p in $(pgrep -f 'qemu-system-x86_64 --nographic'); do sudo kill -9 $p; done`
  （不要用带匹配串的 `pkill -f`，会杀掉自己）。
- 关机：guest 内 `sync; busybox poweroff -f`，QEMU 以 0 退出。

---

## 4. guest 内启动 docker

```sh
dragon-docker-start        # 约 60~90s，出现 29.1.3 overlayfs 即成功
```

- 控制台**长行会丢字符**（约 >400 字符）：长命令写成脚本，用 HTTP 传入后再 `sh`。
- 每次 `docker exec` 都会打印
  `WARNING: --criu ignored (criu binary from $PATH is used); do not use`，属噪声。

---

## 5. 部署 16 容器全栈

### 5.1 直接跑预置脚本

```sh
sh /root/fs.sh          # = docs/ocmanager-porting/full-stack-up.sh（串行版，含就绪轮询）
```

期望输出（顺序固定）：`NET=0` → 5 个 infra 创建结果 → `*_READY=*` →
建库/迁移结果 → 各服务创建结果 → `docker ps` 列表 → **`UPALL_END`**。
单次约 10 分钟；期间串口会持续输出，`UPALL_END` 之后串口停更属正常。

### 5.2 容器清单与端口（内部端口，bridge 网络 `ocm-net`）

| 容器 | 镜像 | 关键端口 | 备注 |
|---|---|---|---|
| mysql | `mysql:8.0.35` | 3306 | `mysqld --initialize-insecure --user=root` |
| redis | `redis:7.2.3-alpine` | 6379 | 别名 `ocm-redis` |
| zookeeper | `bitnamilegacy/zookeeper:3.8.3` | 2181 | `ALLOW_ANONYMOUS_LOGIN=yes` |
| kafka | `bitnamilegacy/kafka:3.5.1` | 9092/9093 | KRaft，别名 `ocm-kafka` |
| clickhouse | `clickhouse/clickhouse-server:23.8.8.20` | 9000/8123 | |
| footstone | `oc-footstone:dev` | HTTP 20000 / trpc 17778 / health 20001 | `JWT_SECRET=devsecret` |
| msg-etl | `oc-msg-etl:dev` | HTTP 8080 / trpc 7778 / health 8081 | 必须注入 `JWT_SECRET` |
| sysdiagnose | `oc-sysdiagnose:dev` | 8080 / 7778 / 8081 | |
| asset-manager | `oc-tms-asset-manager:dev` | trpc 50044 | 自动生成 CA 到 `/data/ocm/ca` |
| gateway | `oc-tms-gateway:dev` | 50041 | 只读挂载 CA |
| api | `oc-tms-api:dev` | 8080（http_no_protocol） | 见 §7 的签名要求 |
| heartbeat | `oc-tms-heartbeat:dev` | — | |
| task-manager | `oc-tms-task-manager:dev` | 50042 | |
| file-svc | `oc-tms-file-svc:dev` | 50045 | |
| data-proxy | `oc-tms-data-proxy:dev` | 50047(admin) | |
| frontend | `oc-frontend:dev` | 80 | 反代 footstone/msg-etl/sysdiagnose |

服务间用**容器名/别名**互访（`ip://task-manager:50042` 等），因此必须用
`docker network create ocm-net` + `--network-alias`，不要改成 host 网络。

### 5.3 传入额外镜像/脚本（guest 无 wget）

guest 内用 clickhouse 镜像自带的 wget 从宿主 slirp 网关取文件：

```sh
docker run --rm --network host --entrypoint wget clickhouse/clickhouse-server:23.8.8.20 \
  -q -O - http://10.0.2.2:8000/<file> > /root/<file>
docker load < /root/ocm-extra-images.tar.gz     # 首次需要
```

---

## 6. 端到端验收

### 6.1 验收集（#309 实测全绿）

| 链路 | 请求 | 期望 |
|---|---|---|
| 容器 | `docker ps -q \| wc -l` | `16` |
| 并发 exec | 8 容器 × 10 轮 `docker exec … true` | 无挂死 |
| 前端 | `GET http://frontend/` | HTML |
| 健康 | `GET footstone:20001/health`、`msg-etl:8081/health`、`sysdiagnose:8081/health` | `{"status":"ok"}` |
| 登录 | `POST footstone:20000/trpc.ocmanager.footstone.User/IdentifyAccount` | `jwt_token` |
| 鉴权 | `User/GetCurrentUser` + `Authorization` | `is_super_admin:true` |
| 项目 | `Footstone/ListProjects` | 「默认」项目 |
| 指标 | `Metrics/ListMetricsData {"plugin_name":"os_monitor"}` | `success:true` |
| sysdiagnose | `Project/ListGlobalCmds` | 命令列表 |
| api→task-manager | `POST api:8080/api/v1/ops_tasks/list` | `code:OK` |
| api→asset-manager | `POST api:8080/api/v1/assets/add` | `code:OK`，可连续多次 |
| 持久化 | 同 uuid 再提交 | `UNIQUE constraint failed: assets.uuid` |

### 6.2 请求构造规矩（踩过的坑，照做即可）

1. **登录字段是 `loginname`**（不是 account），密码传
   `hex(SHA512(明文))`；服务端会再算 `SHA512(password + salt)`。
   默认管理员：`admin / Admin123456@`（seed 见 `v1.0.0_2_init_data.sql`）。
2. **`ListProjects` 分页是嵌套 pb**：
   `{"page_request":{"page_size":10,"page_number":1}}`（不是 `page`）。
3. **变更类 api 端点需要网关 HMAC 签名**。容器内设置了
   `TMS_API_GW_HMAC_SECRET`，因此即使 `TMS_API_GW_HMAC_STRICT=false`，
   `/api/v1/{assets/add,batch_add,remove,ops_tasks/publish,cancel,
   artifacts/upload,remove,agents/upgrade,restart}` 仍强制校验：

   ```
   canonical = METHOD "\n" PATH "\n" X-User-Name "\n" X-User-Role "\n"
               TIMESTAMP "\n" NONCE "\n" SHA256_HEX(body)
   X-Signature = hex(HMAC-SHA256(secret, canonical))
   ```

   需要同时带 `X-User-Name`、`X-User-Role`、`X-Signature-Timestamp`、
   `X-Signature-Nonce`（10 分钟内不可重复）。
   **时间戳必须用 guest 时钟**：guest 是 UTC，与宿主可能差 8 小时，
   先 `date +%s` 取一次再用。
4. **`wget` 对非 2xx 会丢弃响应体并以退出码 6 结束**：取证必须加
   `--content-on-error`，否则“空响应”其实是 401/400。
5. 请求头/字段里含 `{}`、`$()` 的脚本尽量写成文件再 `sh`，避免串口丢字符。

---

## 7. 坑点清单

### 7.1 已在当前基线修复（老内核会复现）

| 症状 | 根因 | 修复 |
|---|---|---|
| 并发 `docker exec` 期间整机挂死，双 CPU 分别在 `flush_tlb_multi` 等 ack / handler 自旋 | CSD `info` 锁的持有者不可归属；rq 自锁等待时不服务入站 shootdown | 无锁 CSD + rq 自锁周期服务入站（`85cf41cf`） |
| 容器内 `mmap` 后首次访问 panic 为 `map_pages() ... OverlayFS`，或上层报 `disk I/O error: operation not supported`；kafka/JVM 启动失败 | OverlayFS 没有页缓存，`map_pages` 默认实现是 panic | 实现 OverlayFS `map_pages`，按 backing inode 页缓存映射（`eb8f7582`） |
| 文件第一次成长之后写入永久报 `operation not supported`（sqlite `SQLITE_IOERR_WRITE`+errno 95、JVM/entrypoint 写文件） | 写入/预分配把新 extent 标 unwritten，而实现无“未写→已写”转换，`extent_right_spine_projection` 返回 ENOTSUP | 写路径直接 written；预分配按“是否属于有效数据”区分（`cf2ed782`） |

> 修复前的旧文件若尾部已有 unwritten extent 仍会失败：删除/重建即可。

### 7.2 部署口径

- `msg-etl` 不注入 `JWT_SECRET` 会去回查 `QueryJWTSecret`，而该 RPC 需管理员上下文 → 必然失败。
- `asset-manager` 的 CA 由它自己生成（`/data/ocm/ca`），gateway 只读挂载；
  首启要等 CA 文件出现再起 gateway。
- **容器退出后 DNS 记录立即消失**：`getent hosts <name>` 失败时先 `docker ps -a`
  确认容器是否还在运行，别误判为网络问题。
- kafka 用 KRaft（`KAFKA_CFG_PROCESS_ROLES=controller,broker`），
  advertised listener 必须是 `kafka:9092`，否则 zookeeper 依赖面会连不上。
- `docker restart <svc>` 只重建进程，不会重建数据卷；排查“状态相关”的问题时
  先清对应数据（如 `/data/ocm/asset-manager/assets.db*`）。

### 7.3 工具链 / 操作

- guest 无 `curl`/`wget`/`zstd`；有 `gzip`。取文件走宿主 HTTP + clickhouse 镜像里的 wget。
- 串口**读要 sudo**；控制台长命令丢字符；激活后第一条命令被吞。
- `make -C kernel fmt` 在 nix 环境外会整体重排无关文件 + clippy 101，不要提交这类噪声。
- 判断脚本是否跑完：看 `serial_opt.txt` 的 mtime 是否停止增长 + 关键字（如 `UPALL_END`）。

---

## 8. 故障排查手册

1. **确认是否挂死**：`ls -l --time-style=+%H:%M:%S serial_opt.txt` 与 `date` 对比，
   超过 3~5 分钟无增长基本可判挂死。
2. **gdb 看现场**（端口在 QEMU 命令行 `gdb tcp::<port>`）：
   ```sh
   timeout 40 gdb -q -batch bin/kernel/kernel.elf -ex "target remote :<port>" \
     -ex 'info threads' -ex 'p/x $rsp' -ex 'detach'
   ```
   关键静态量：`SPIN_LOCK_ADDR`、`SPIN_CONTENDED`（自旋锁取证）。
3. **写入类错误**：开启 `ada1c760` 的诊断后，串口会打印
   `another_ext4: ENOTSUP created at <file:line>` 与
   `syscall <name> returned EOPNOTSUPP`，直接定位；
   注意 `SystemError::to_posix_errno()` 返回**负数**，比较要用 `-95`。
4. **`pcache stall: wait_ready Loading exceeded Ns`**：说明有页长时间停在
   Loading；诊断会同时打印 `waiter_pid/owner_pid/entry/instance_id/page_phys`。
5. 容器起来但服务不健康：先 `docker logs --tail 50 <svc>`；
   依赖顺序是 mysql/redis/kafka/clickhouse → (asset-manager 生成 CA) → 其余服务。

---

## 9. 参考验收结果（内核 #309，12G，单次净启动）

| 项 | 结果 |
|---|---|
| 容器 | 16/16 `Up`（含 kafka、msg-etl） |
| 并发压测 | 8 容器 × 10 轮 → `STRESS_OK`，无挂死/spinout/panic |
| 前端 | `<!DOCTYPE html>` |
| 三个健康端点 | 全部 `{"status":"ok"}` |
| 登录 / 鉴权 | JWT 296 字符 / `GetCurrentUser` `is_super_admin:true` |
| 项目 / 指标 / sysdiagnose | 全部 `success:true` |
| api→task-manager | `code:OK` |
| api→asset-manager | `assets/add` ×3 全部 `code:OK`（id=1/2/3）；重复 uuid → `UNIQUE constraint failed` |
| 关机 | `busybox poweroff -f` → QEMU exit 0 |

## 10. 已知限制

- 修复前创建的、尾部已有 unwritten extent 的文件仍无法追加写入
  （无“未写→已写”转换）；重建文件即可。
- `pcache Loading` 长时间停留的现象仍会被记录（不影响当前验收项），需要时按 §8.4 取证。
- kafka(JVM) 历史上偶发启动失败，修复 `eb8f7582` 后本轮 16/16 稳定；仍建议长跑观察。
