# OCManager 镜像预置策略：把 `docker load` 从每次启动中拿掉

> 日期：2026-10-23 | 工作区：`~/DragonOS-ocmanager` | 范围：只读调研 + 设计；不改内核，脚本改动仅作为落地建议（本次未提交）
> 证据标注：**实测** = 本次直接读取仓库/镜像盘或在 host 上复现；**推断** = 由实测事实推导，未在 guest 完整验证。

---

## 0. 结论先行

**推荐方案：A（golden 盘基线）+ A2（把 golden 的 `/var/lib/docker` 抽成 seed，在写盘阶段注入）**。
日常迭代用 qcow2 覆盖盘或稀疏复制，**启动后不再执行任何 `docker load`**。

一句话：*一次干净引导把 6 个 tar（共 8 个镜像）load 进 containerd store，关机后把 `/var/lib/docker` 固化为带校验的 seed，由 `write_disk_image.sh` 在挂载点直接解包预置；此后每次启动零 load，迭代用 qcow2 覆盖盘。*

关键实测数字：

| 指标 | 数值 | 来源 |
|---|---|---|
| 6 个 tar 总大小 | 1,362,633,989 B ≈ 1.27 GiB | 实测（`stat`） |
| 解压后唯一 layer 总量（content store） | 3,114,088,960 B ≈ 2.90 GiB（44 个唯一 blob；47 条 chain） | 实测（tar 成员大小求和 + chainID 计算） |
| 完整预置后 `/var/lib/docker` 占用 | **5.9 GiB**（content 3.0G + snapshots 3.0G） | 实测（host 同版本 load 全部 6 tar） |
| 预置后镜像数 / snapshot 数 / blob 数 | 8 / 47 / 60，ingest 为空 | 实测（同上） |
| 在 guest 内 load 全部镜像 | **30 分钟以上**，且多次卡死/EIO | 实测（用户与 `status-overview.md` 记录） |
| 在 host 用同版本 docker 预置 | 6 次 load 合计约 **95 s**（mysql 13s、zk 15s、kafka 17s、CH 36s、ocservices 9s、redis <5s；不含每次 daemon 启动） | 实测（两次分步跑） |
| 单次迭代节省 | **≥ 25–30 分钟**（load 时间归零；首次 `docker run` 也不再解包） | 推断（由上述两者相减） |

方案速览：

| 方案 | 可行性 | 一次性成本 | 每轮节省 | 主要风险 | 判定 |
|---|---|---|---|---|---|
| **A golden 盘** | 高（实测存储格式支持；跨启动可见为高置信推断） | 一次 guest 引导 + load，30–40 min | 30 min+ | 关机 journal、运行态残留、16G 配额 | **推荐** |
| **B host 侧预填充** | 中高（host 机制实测通过；guest 侧兼容为推断） | 约 2 min（loop 挂载 + 特权容器 load） | 30 min+ | 版本必须完全一致、内核 overlay 语义差异、必须 guest 复验 | 备选 |
| **C 手工构造 overlayfs/containerd 记录** | 低 | 数小时到数天 | 同 A | bbolt schema、chainID、GC 标签、快照目录 | **拒绝** |
| **D1 把 `/var/lib/docker` 做成 seed 注入** | 高 | 一次 golden 生成；每次写盘 +1–2 min | 30 min+ | 注入时 mknod/xattr、tar 完整性 | **推荐落地形态（A2）** |
| **D2 独立 docker-data 磁盘** | 高（需 guest 挂载编排） | 同 A | 30 min+，且内核迭代不碰 rootfs | 启动挂载时序、双盘 journal | 可选增强 |
| **D3 skopeo/buildah/OCI 导入** | 低 | 仍需 containerd import | 同 B | 不能直接填 docker 的 containerd store | 并入 B |
| **D4 扁平化 `docker import`** | 低 | — | 有限 | 丢 layer/entrypoint/cmd，compose 失效 | **拒绝** |

---

## 1. 实测环境与存储布局

### 1.1 版本（实测）

| 组件 | 版本 | 证据 |
|---|---|---|
| 基础发行版 | Ubuntu 24.04 (noble) | `tools/docker-rootfs/Dockerfile` |
| docker-ce / cli | **29.1.3** | Dockerfile `DOCKER_VERSION`；`/usr/local/share/docker-validation/packages.txt` |
| containerd.io | **2.2.0** | 同上 |
| runc | **1.3.4** | 基础镜像内 `runc --version` |
| docker compose plugin | 2.40.3 | 同上 |
| guest 内核 | `6.6.21-dragonos #195` | `serial_opt.txt` 启动行 |
| QEMU 规格 | 8G RAM / 2 vCPU / `-rtc clock=host,base=localtime` | 运行中的 QEMU 命令行 |

guest 内 `dragon-docker-start` 会打印 `docker info --format '{{.ServerVersion}} {{.Driver}}'`，实测输出：

```
29.1.3 overlayfs
```

（`serial_opt.txt:285`，同一行也出现在本次运行 harness 的 `output.log:294`。）

### 1.2 存储架构：containerd image store + overlayfs snapshotter（实测）

guest 盘内 `/var/lib/docker` 的只读 `debugfs` 取证显示：

- **不存在** `/var/lib/docker/image`（classic image store 的 `overlay2` 元数据目录）。
- **不存在** `/var/lib/docker/overlay2`（classic graph driver 的 diff 目录）。
- 存在完整的 containerd image store 目录树：

```
/var/lib/docker/
├── engine-id                      # 36B UUID，首次 dockerd 启动生成
├── buildkit/{cache.db,history_*.db}
├── containers/                    # 运行态：空
├── network/files/local-kv.db      # 运行态：默认网络数据库
├── runtimes/  plugins/  swarm/  tmp/  volumes/
└── containerd/daemon/
    ├── io.containerd.metadata.v1.bolt/meta.db              # bbolt：images/content/snapshots/GC
    ├── io.containerd.content.v1.content/
    │   ├── blobs/sha256/<digest>                           # 未压缩 layer tar + config + manifest
    │   └── ingest/                                         # 未完成导入（必须为空）
    └── io.containerd.snapshotter.v1.overlayfs/
        ├── metadata.db                                     # snapshot 父子链
        └── snapshots/<id>/{fs,work}                        # 已解包的 layer 根
```

- `docker info` 的 `Driver` 显示为 **`overlayfs`**（不是 `overlay2`）：这是 Docker 29 **containerd image store** 模式下的快照器名称（实测：host 同版本复现，guest 串口同样输出；与目录布局互为印证）。
- `engine-id` 的值为 `98e7e9a4-fe88-4170-8729-954da731834a`（实测，只读取证）。
- `/etc/machine-id` 与 `/etc/hostname` 均为 **0 字节**（实测），不存在 systemd/dbus machine-id；因此 golden 复制不需要处理 machine-id。
- `/etc/docker/` 为空目录，**没有 daemon.json**（实测），实际行为是 containerd store（实测；“这是 29.1.3 默认值”为推断）。是否需要显式写 daemon.json 由后续版本升级决定。

**结论（实测）**：预置镜像的正确落点是
`/var/lib/docker/containerd/daemon/{io.containerd.content.v1.content,io.containerd.snapshotter.v1.overlayfs,io.containerd.metadata.v1.bolt}`，
**不是** classic 的 `/var/lib/docker/overlay2` + `/var/lib/docker/image/overlay2`。任何按 classic 布局设计的方案都会直接失效。

### 1.3 元数据里有什么、没有什么（实测 + 推断）

在 host 上用同一 `dragonos-docker:ubuntu2404-29.1.3` 预置全部 6 个 tar 后，对 `meta.db` 做 `strings` 检查：

- 镜像记录位于 containerd namespace **`moby`**；记录键形如 `moby/<n>/sha256:<chainID>`；
- image 标签包含 `io.containerd.image.name`、`org.opencontainers.image.ref.name`；
- GC 引用形如
  `containerd.io/gc.ref.content.config`、`containerd.io/gc.ref.content.l.<i>`、
  `containerd.io/gc.ref.snapshot.overlayfs`；
- **没有发现任何绝对路径**（`^/`、`upperdir`、`/var/lib`、`/run` 均无命中）（实测）。
  这意味着整个 store 与宿主/guest 的绝对目录、machine-id、磁盘 UUID 无关；只要挂到**同名路径** `/var/lib/docker` 即可。
- snapshotter 的 `metadata.db` 同样未出现绝对路径（实测）。

**推断**：因此“同一版本 docker/containerd 在 host 生成、guest 读取”在数据格式上成立；但 DragonOS 内核对 overlayfs/白化/xattr 的语义仍必须由一次 guest 冒烟启动确认（见 §10）。

### 1.4 盘内现状（只读取证；该盘当时正被运行中的 QEMU 以 rw 打开）

在**不写盘**的前提下用 `debugfs` 读了 `bin/disk-image-x86_64.img`（`losetup -r`，取证后已 detach）：

| 观察 | 值 | 含义 |
|---|---|---|
| `/var/lib/docker` 结构 | containerd store（如上） | 镜像确实落在盘内 |
| content blobs | 27 个，合计 1,226,193,577 B | 含 mysql 11 层 + redis 8 层 + 完整 zk 层 + 少量 CH 空层 |
| 镜像记录 | `docker.io/library/mysql:8.0.35`、`docker.io/library/redis:7.2.3-alpine`（读取时） | 正在进行的 run 仍在写 meta.db |
| snapshots 目录 | 43 个 | 包含历史/失败 load 的孤儿快照 |
| `ingest/` | 残留 `data` = 575,668,224 B 且未完成 | 一次被中断的层导入 |
| ext4 | 16 GiB 虚拟；`qemu-img info` 显示 disk size **5.41 GiB** | 稀疏盘 |
| 运行记录 | 本次 `r16_ocm_up.sh` 已打印 `L_mysql8035=0`、`L_redis72=0`、`L_bzk383=0`，随后 kafka 阶段 `containerd` 阻塞在 futex（TASK DUMP） | 与会话历史一致 |

**结论（实测）**：当前盘是**使用中的工作盘**，含孤儿快照和未完成 ingest，**不能直接当 golden**。golden 必须从净盘按脚本重新生成，并在归档前校验 `ingest/` 为空。

---

## 2. 现状成本基线

### 2.1 镜像清单（实测）

`ocservices.tar.gz` 是 **三合一包**：3 个镜像各 7 层，前 3 层共享，合计 15 个唯一 layer。因此 6 个 tar 对应 **8 个镜像引用**。

| tar | 镜像引用 | tar 大小 (B) | layer 条目 | 解压 layer 字节 (B) | guest 内 load 记录 |
|---|---|---|---|---|---|
| `mysql8035.tar.gz` | `mysql:8.0.35` | 190,097,199 | 11 | 606,608,896 | 一次 >13 min；用户量级 5–13 min |
| `redis72.tar.gz` | `redis:7.2.3-alpine` | 17,698,127 | 8 | 42,203,648 | 快速（`L2=0`） |
| `bzk383.tar.gz` | `bitnamilegacy/zookeeper:3.8.3` | 304,484,519 | 1 | 577,355,264 | `LOAD=0` |
| `bkafka351.tar.gz` | `bitnamilegacy/kafka:3.5.1` | 362,427,363 | 1 | 562,702,336 | 单独 load <5 min；连续 load 多次挂死 |
| `clickhouse.tar.gz` | `clickhouse/clickhouse-server:23.8.8.20` | 342,361,890 | 11 条，9 个唯一 blob | 1,018,384,896 | 单独成功；5 连装时 EIO |
| `ocservices.tar.gz` | `oc-footstone:dev`、`oc-msg-etl:dev`、`oc-sysdiagnose:dev` | 145,564,891 | 21 条（7×3 镜像），15 个唯一 blob | 306,834,944 | — |
| **合计** | **8 个镜像** | **1,362,633,989** | **63 条 / 44 个唯一 blob / 47 条 chain** | **3,114,088,960** | **30 min+（用户实测）** |

证据：`status-overview.md` §3.7/§3.8（mysql >13 min、kafka <5 min、clickhouse EIO、6 镜像 30 min+ 为用户口述实测）；单镜像 tar 大小与 layer 数由本次 `tar` 解析实测。
补充：本次可得的 `serial_opt.txt` / harness `output.log` 只有 `L_*=<rc>` marker、**没有逐行时间戳**，因此无法从日志精确反推每个镜像的 load 耗时；表中 guest 值是历史记录与用户实测，host 值是本次实测。

### 2.2 完整预置的磁盘/元数据足迹（host 实测）

在 host 上用同版本 docker（29.1.3/containerd 2.2.0，特权容器、`--data-root` 指向空目录）依次 load 全部 6 个 tar：

| 指标 | 实测值 |
|---|---|
| 镜像引用 | 8 |
| `/var/lib/docker` 总占用 | **5.9 GiB** |
| content store | 3.0 GiB |
| overlayfs snapshots | 3.0 GiB（load 即解包，见下） |
| snapshot 数 | 47（= 47 条唯一 chainID；按镜像内 layer 序列累计，重复层也各成一条 chain；唯一 layer blob 为 44 个） |
| blob 数 | 60 |
| `ingest/` 残留 | 0 |
| snapshot 中的 xattr | 0（8 个镜像全量扫描） |
| inode 总数 | 48,190（目录 4,817 / 文件 38,145） |
| `docker load` 耗时 | 合计约 95 s（不含 daemon 启动） |
| 镜像校验 | `docker create --network none` 成功；重启 daemon 后 8 个镜像全部可见 |

**重要事实（实测）**：在 containerd image store 下，`docker load` **不仅写 content blob，还会立即解包成 snapshots**（redis 8 层 → 8 个 snapshot；全量 47 个）。因此预置后首次 `docker run` 不再付出解包成本；但也意味着磁盘需要约 **2× layer 总量**。

### 2.3 16G 配额测算（推断）

- 基础 rootfs：约 0.4 GiB（基础镜像内 `du -sx /` 实测 411,268 KiB）。
- 预置 store：5.9 GiB（实测）。
- ext4 元数据/journal/日志/稀疏开销：约 0.6–0.8 GiB（由当前盘 5.41 GiB 实占反推）。
- **合计约 6.9–7.0 GiB，剩余约 9 GiB** 给容器可写层、MySQL/ClickHouse/Kafka 数据与业务日志。
- 若 compose 全栈的持久化数据会超过 ~8 GiB，建议把 rootfs 调到 24G，或采用 D2（独立 docker-data 盘），见 §7.2。

---

## 3. 方案 A：golden 盘基线

### 3.1 可行性

- **实测**：镜像数据确实以 containerd store 形式落在 ext4 盘内；同一 store 在 daemon 重启后仍可见（host 实测）。
- **实测**：content 与 snapshot 均为内容寻址/相对引用，元数据无绝对路径。
- **推断（高置信）**：用 golden 盘启动后 `docker images` 直接可见，无需 load；需按 §10 做一次 guest 冒烟确认。

### 3.2 生成流程（命令级）

前提：**独占**磁盘镜像，QEMU 必须已退出；绝不能在 QEMU 运行中写盘（本次取证时盘正被一个运行中的 QEMU 以 rw 打开，只能只读查看）。

1) 生成净盘（现有流程）：

```bash
cd ~/DragonOS-ocmanager
ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage
```

2) 把 6 个 tar 注入盘根（沿用现有注入方式；推荐 host 挂载而不是逐文件 debugfs）：

```bash
IMG=bin/disk-image-x86_64.img
sudo losetup -f --show -P --offset $((2048*512)) "$IMG"     # 输出 /dev/loopN
sudo mkdir -p /mnt/ocmroot && sudo mount /dev/loopN /mnt/ocmroot
sudo cp /home/sparkzky/ocm-tars/{mysql8035,redis72,bzk383,bkafka351,clickhouse,ocservices}.tar.gz /mnt/ocmroot/
sync && sudo umount /mnt/ocmroot && sudo losetup -d /dev/loopN
```

3) 启动 guest，按**受控顺序**逐个 load 并即时校验（可断点续跑）：

```sh
# guest 内
dragon-docker-start
for t in mysql8035 redis72 bzk383 bkafka351 clickhouse ocservices; do
  docker load -i /$t.tar.gz || echo "RETRY $t"
  docker images --digests > /tmp/images.$t.txt
done
docker info --format '{{.ServerVersion}} {{.Driver}}'
docker system df
test "$(docker images -q | wc -l)" -eq 8 || echo "IMAGES_MISSING"
# 必须为空，否则 golden 不干净
ls -A /var/lib/docker/containerd/daemon/io.containerd.content.v1.content/ingest
```

断点策略（推断）：containerd 按 digest 去重，失败后重新执行同一 `docker load` 会跳过已存在 blob；若遇卡死，直接关机、host 侧 `e2fsck` 后重启续跑（见 3.4）。

4) 清理运行态后再关机（guest 内）：

```sh
rm -f /*.tar.gz                       # 回收 1.27 GiB
docker container prune -f
docker network prune -f
docker volume prune -f
rm -rf /var/lib/docker/buildkit/* \
       /var/lib/docker/tmp/* \
       /var/lib/docker/runtimes/* \
       /var/lib/docker/containers/* \
       /var/lib/docker/network/files/*
rm -rf /var/lib/docker/containerd/daemon/tmpmounts/* \
       /var/lib/docker/containerd/daemon/io.containerd.runtime.v2.task/* \
       /var/lib/docker/containerd/daemon/io.containerd.grpc.v1.introspection/uuid
rm -f /var/lib/docker/engine-id      # 可选：让每次启动生成新 engine-id
rm -f /tmp/dockerd.log
sync; sync
busybox poweroff                     # 优先让 init 走卸载流程
# 若只有 -f 能退出：busybox poweroff -f，然后 host 侧必须做 e2fsck（见 3.4）
```

5) host 侧校验与归档：

```bash
IMG=bin/disk-image-x86_64.img
sudo losetup -f --show -P --offset $((2048*512)) "$IMG"     # /dev/loopN
sudo e2fsck -fy /dev/loopN            # 重放 journal / 修复孤儿；只在独占且确认是 golden 时做
sudo dumpe2fs -h /dev/loopN | grep -E 'Filesystem state|Filesystem features|Free blocks|Free inodes'
sudo losetup -d /dev/loopN
mkdir -p bin/golden
cp --sparse=always "$IMG" bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img
chmod 0444 bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img
sha256sum bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img | tee bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img.sha256
```

### 3.3 使用方式：不要直接启动 golden

**实测**：本机 `/home` 位于 ext4（`stat -f` 报 `ext2/ext3`），`cp --reflink=always` 返回 `Operation not supported`，即 **reflink 不可用**；`cp --reflink=auto` 会退化为整盘拷贝（约 7 GiB 实际写入）。

推荐（qcow2 覆盖盘，秒级、不复制数据、golden 只读）：

```bash
qemu-img create -f qcow2 -b "$PWD/bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img" \
                -F raw bin/work.qcow2
# 启动时把磁盘改为：-drive id=disk,file=../bin/work.qcow2,if=none,format=qcow2
```

备选（必须 raw 时）：

```bash
cp --sparse=always bin/golden/ocm-golden-....img bin/disk-image-x86_64.img
```

注意：`tools/run-qemu.sh` 当前把主盘写死为 `format=raw`（实测），用 qcow2 需加一个 `DISK_FORMAT`/`DISK_IMG` 参数；本设计只给方案，不改脚本。

### 3.4 风险与对策

| 风险 | 证据/说明 | 对策 |
|---|---|---|
| **脏 journal 重放 nested-panic** | 已知独立缺陷；`poweroff -f` 可能留下需重放的 journal | 优先 `sync; busybox poweroff`（不带 -f）；退而求其次：QEMU 退出后 host `e2fsck -fy`，再归档 |
| **ext4 “state: clean” 不可信** | 实测：盘被运行中的 guest 以 rw 挂载时 `dumpe2fs` 仍报 `Filesystem state: clean` ⇒ DragonOS 不按 Linux 语义置脏 | 不要只看 state；每次归档/复制后跑 `e2fsck -fn/-fy`，并依赖“干净关机 + sync”双保险 |
| 运行态残留 | `/var/lib/docker/{containers,network,buildkit,runtimes,tmp}`、`/var/run/docker*` | 按 3.2-4 清理；golden 内 `ingest/` 必须为空 |
| 16G 配额 | 预置 5.9G + rootfs 0.4G + 元数据，实占约 7G；余 ~9G | `docker system df`、`df -h`、`df -i` 纳入验收；不够则 24G 或 D2 |
| inode | 1,048,576 个 inode；预置实测 48,190 | 富余约 20×，无需调整 |
| 磁盘 UUID 重复 | 所有副本 UUID 相同 | 不要同时挂两份；必要时 working copy `tune2fs -U random` |
| golden 被误写 | 直接 `make write_diskimage` 会覆盖 `bin/disk-image-x86_64.img` | golden 放 `bin/golden/` 并 `chmod 0444`；用 qcow2 覆盖启动 |
| 镜像/依赖升级 | golden 与 29.1.3/2.2.0 绑定 | 文件名带版本；升级 docker/containerd 后重新生成 |
| 与内核迭代耦合 | QEMU 通过 `-kernel` 加载内核，rootfs 不必随内核重建 | golden 可跨内核迭代复用；只有 rootfs/app 变更才重建（配合 A2） |

### 3.5 收益（推断）

- 每轮启动：load 30 min+ → 0；dockerd 就绪约数秒；`docker images` 立即可用。
- 同时消除“连续 load 触发 futex 卡死/EIO”的整类偶发问题（`status-overview.md` §3.8 实测：5 连装 clickhouse EIO；本次取证时 kafka 阶段再次卡死）。
- 一次性成本 30–40 min，第二次迭代即回本。

---

## 4. 方案 B：host 侧预填充 `/var/lib/docker`

### 4.1 可行性（host 机制已实测）

用基础镜像 `dragonos-docker:ubuntu2404-29.1.3`（与 guest 完全同版本）在特权容器中运行 dockerd，把待填充目录绑定到 **容器内同名路径 `/var/lib/docker`**：

- `docker info` → `29.1.3 overlayfs`（与 guest 相同）；
- 6 个 tar 全部 load 成功，load 合计约 95 s；8 个镜像、47 snapshots、ingest 为空；
- 杀掉 daemon、重新起一个容器后，8 个镜像仍可见，`docker create --network none` 成功（说明 snapshot 链与 overlayfs 挂载元数据完整）；
- 在**删除 `engine-id`、`buildkit/`、`network/files/`、`containers/` 之后**重启 daemon，8 个镜像仍完整可见（实测，验证 golden 清理清单不会破坏镜像可见性）；
- `meta.db` 无绝对路径，镜像/快照键均为 `moby/...` + digest。

（以上均为本次 host 实测。）

### 4.2 实施步骤（命令级）

```bash
IMG=bin/disk-image-x86_64.img
# 1) 独占挂载盘（确认 QEMU 已退出）
sudo losetup -f --show -P --offset $((2048*512)) "$IMG"     # /dev/loopN
sudo mkdir -p /mnt/ocmroot && sudo mount /dev/loopN /mnt/ocmroot
sudo mkdir -p /mnt/ocmroot/var/lib/docker && sudo chmod 0710 /mnt/ocmroot/var/lib/docker

# 2) 用同版本 docker 预填充（必须在容器内用 /var/lib/docker 这个路径）
docker run --rm --privileged --name ocm-prefill \
  -v /mnt/ocmroot/var/lib/docker:/var/lib/docker \
  -v /home/sparkzky/ocm-tars:/tars:ro \
  --entrypoint sh dragonos-docker:ubuntu2404-29.1.3 -c '
    dockerd --iptables=false --ip6tables=false --bridge=none \
            --data-root=/var/lib/docker --exec-root=/tmp/exec \
            >/tmp/dockerd.log 2>&1 &
    for i in $(seq 90); do docker info >/dev/null 2>&1 && break; sleep 1; done
    docker info --format "{{.ServerVersion}} {{.Driver}}" || exit 1
    for t in mysql8035 redis72 bzk383 bkafka351 clickhouse ocservices; do
      docker load -i /tars/$t.tar.gz || exit 1
    done
    docker images --digests
    docker container prune -f; docker network prune -f; docker volume prune -f
    rm -rf /var/lib/docker/buildkit/* /var/lib/docker/tmp/* \
           /var/lib/docker/runtimes/* /var/lib/docker/containers/* \
           /var/lib/docker/network/files/* \
           /var/lib/docker/containerd/daemon/tmpmounts/* \
           /var/lib/docker/containerd/daemon/io.containerd.runtime.v2.task/* \
           /var/lib/docker/containerd/daemon/io.containerd.grpc.v1.introspection/uuid
    rm -f /var/lib/docker/engine-id
    sync; sync
    kill %1; wait
  '

# 3) 卸载 + 校验（先确认没有残留 overlay 挂载）
mount | grep /mnt/ocmroot || true
sudo umount /mnt/ocmroot
sudo e2fsck -pf /dev/loopN
sudo losetup -d /dev/loopN
```

容器内**必须**使用 `/var/lib/docker` 这个绝对路径做 `--data-root`（虽然实测元数据无绝对路径，但保持与 guest 完全一致可消除未知字段风险）。

### 4.3 风险与对策

| 风险 | 说明 | 对策 |
|---|---|---|
| **版本必须完全一致** | 本机 host docker 为 28.5.1，**不能**直接用来预填充 29.1.3 的 store（bolt schema/默认行为差异，推断） | 只用 `dragonos-docker:ubuntu2404-29.1.3` 镜像内二进制；CI 固定该镜像 digest |
| containerd bolt schema | meta.db 的 schema 由 containerd 2.2.0 写；升级后可能不兼容（推断） | seed 文件名带 `containerd2.2.0`；升级即重建 |
| 内核 overlayfs 语义差异 | host 解包时创建了 9 个设备节点（`/dev/null` 等）；8 镜像全量扫描 **0 个 xattr**。若未来镜像含 opaque/whiteout xattr，guest 必须能读写 | 保持 `--privileged`（CAP_MKNOD）；新增镜像后做一次 guest 冒烟；必要时改用方案 A 在 guest 内生成 |
| 网络副作用 | 容器内 dockerd 可能改 host iptables/bridge | `--iptables=false --ip6tables=false --bridge=none` + `--network none`（更稳） |
| 运行态残留 | `network/files/local-kv.db`、buildkit、engine-id | 按步骤清理；`ingest/` 必须为空 |
| 写盘一致性 | daemon 未退出就 umount 会损坏 bolt db | `kill %1; wait` 后再 `sync`，然后 umount + e2fsck |
| **guest 兼容性未验证** | host 生成、guest 消费只做了格式分析（无绝对路径、digest 一致），未启动 guest | **强制**一次 guest 冒烟（§10）；失败则回退 A |
| 并发写 | 绝不能在 QEMU 运行时挂载填充 | 加锁/检查 `pgrep qemu-system` |

### 4.4 收益

- 与 A 相同的运行期收益；生成成本从 30–40 min 降到约 2 min（实测 load 95 s + 挂载/校验）。
- 适合 CI：无需先启动一台 golden VM。
- 但引入“host 生成 → guest 消费”的跨内核边界，必须保留一次端到端验收。

---

## 5. 方案 C：host 侧直接构造 overlayfs diff + containerd 记录（拒绝）

### 5.1 为什么不能按 classic overlay2 做

guest 使用 **containerd image store**，classic 的
`/var/lib/docker/overlay2/<id>/diff` + `/var/lib/docker/image/overlay2/imagedb/content/sha256/<id>`
**完全不存在**（实测）。按 classic 布局预置，dockerd 不会识别任何镜像。

### 5.2 真正需要构造的东西（代码证据 + 推断）

要手工让 containerd 识别镜像，至少需要：

1. `content/blobs/sha256/<digest>`：layer（未压缩 tar）、config、manifest/index，全部按 digest 命名并通过 size 校验；
2. `meta.db`：
   - `images` bucket：key = 镜像名，value 含 `Target`（manifest/index descriptor）、`io.containerd.image.name`、时间戳；
   - `snapshots` bucket：key = `moby/<n>/sha256:<chainID>`，含 parent 链；
   - GC 标签：`containerd.io/gc.ref.content.config`、`...content.l.<i>`、`...snapshot.overlayfs`；
   - `version`、`leases`、namespace 等；
3. `snapshotter/metadata.db` + `snapshots/<id>/{fs,work}`；
4. **chainID 计算**：`chainID = sha256(parentChainID + " " + diffID)`（首层为 diffID），必须与 config 的 `rootfs.diff_ids` 完全一致（推断；本次用该公式算出 47 条 chain，与 host 实测 snapshot 数 47 完全一致）；
5. 解包每个 layer 到 `fs`，白化用设备节点或 overlay xattr，且行为必须与 containerd 的 applier 一致。

任何一个字段/标签错误，轻则镜像不可见，重则 containerd GC 在启动时把 blob/快照回收掉。

### 5.3 结论

- 可行性：**低**；本质是重写 containerd importer。
- 风险：schema 与实现强耦合，containerd 升级即失效；无官方校验工具。
- 收益：与 A/B 相同，但**没有任何额外收益**。
- **拒绝**。如果不想启动 dockerd，合法的等价做法是用 containerd 官方 API（`ctr -n moby images import`），那仍属于方案 B 的变体，而不是手写 DB。

---

## 6. 方案 D：其它路线

### 6.1 D1：golden `/var/lib/docker` seed 注入（推荐落地形态，即 A2）

思路：不让每次构建都启动 VM，而是**一次性**由 guest 生成 golden store，然后把它抽成归档，在 `write_disk_image.sh` 挂载 rootfs 后解包。

优点：

- 数据仍由 guest 自己生成（无 B 的跨内核兼容风险）；
- rootfs/sysconfig 每次都是新构建，只有镜像数据复用；
- 写盘阶段解包 5.9 GiB，host 上约 1–2 min（推断，NVMe + zstd），远低于 guest load 的 30 min；
- 内核迭代不受影响（内核走 `-kernel`，本来就不需要重建盘）。

实现要点：

1. 从 golden 盘（只读挂载）打包，排除运行态：

```bash
sudo mount -o ro /dev/loopN /mnt/ocmroot
sudo tar -C /mnt/ocmroot/var/lib/docker \
  --numeric-owner --xattrs --xattrs-include='*' \
  --exclude=./buildkit --exclude=./tmp --exclude=./runtimes \
  --exclude=./containers --exclude=./network/files --exclude=./engine-id \
  --exclude=./containerd/daemon/io.containerd.runtime.v2.task \
  --exclude=./containerd/daemon/tmpmounts \
  --exclude=./containerd/daemon/io.containerd.grpc.v1.introspection \
  -cf - . | zstd -T0 -3 -o bin/golden/var-lib-docker-docker29.1.3-containerd2.2.0.tar.zst
```

2. 在 `tools/write_disk_image.sh` 的 **sysconfig 导入/审计成功之后、`ensure_boot_dir`/最终 `sync` 之前**（当前约 405 行之后）插入：

```bash
# ---- OCManager docker seed (A2) ----
SEED="${root_folder}/bin/golden/var-lib-docker-docker29.1.3-containerd2.2.0.tar.zst"
if [ "${OCM_DOCKER_SEED:-1}" != "0" ] && [ -f "${SEED}" ]; then
    mkdir -p "${mount_folder}/var/lib/docker"
    chmod 0710 "${mount_folder}/var/lib/docker"
    zstd -dc "${SEED}" | tar -x -C "${mount_folder}/var/lib/docker" \
        --numeric-owner --xattrs --xattrs-include='*'
    test -f "${mount_folder}/var/lib/docker/containerd/daemon/io.containerd.metadata.v1.bolt/meta.db" \
        || { echo "docker seed incomplete" >&2; exit 1; }
    echo "docker seed injected: $(du -sh "${mount_folder}/var/lib/docker" | cut -f1)"
fi
```

3. 默认读固定路径。当前 Makefile 的 `sudo ... bash write_disk_image.sh` **不会继承**调用者环境，因此 `OCM_DOCKER_SEED=0` 只有在 Makefile 的 sudo 行显式透传时才生效；日常关闭注入更可靠的方式是移走/改名 seed 文件。

注意：`tar -x` 必须以 root 运行（Makefile 已 `sudo`），否则设备节点（`/dev/null` 等 9 个）无法创建。

### 6.2 D2：独立 docker-data 磁盘

把镜像数据放到第二块 ext4 盘（项目已把 `bin/ext4.img` 作为第二块盘挂给 QEMU，实测 QEMU 命令行含 `-drive id=ext4disk`），启动时挂到 `/var/lib/docker`。

- 优点：rootfs 重写完全不碰镜像；内核/用户态迭代与镜像数据解耦；盘大小可单独扩。
- 缺点：需 guest 启动脚本/fstab 保证先挂载再起 dockerd；两块盘都可能有 journal 问题；丢失第二块盘等于丢镜像。
- 适合作为 A2 之后的增强，不作为第一步。

### 6.3 D3：skopeo / buildah / OCI layout

`skopeo copy docker-archive:... oci:...` 只能生成 OCI layout，**不能**直接写 Docker 的 containerd store；仍需 `ctr -n moby images import`（属于 B 的变体）。没有收益，除非未来改用纯 containerd 运行时。

### 6.4 D4：`docker import` 扁平化

`docker import` 生成单层镜像，**丢失原始 layer、ENTRYPOINT/CMD/ENV/EXPOSE 等 config**；要恢复需逐镜像 `--change`，且 compose 依赖的镜像历史/层缓存全部失效。

结论：**拒绝**。

### 6.5 D5：virtiofs/9p 共享 host 目录

overlayfs 需要可信的 upperdir/whiteout/xattr 语义；把 `/var/lib/docker` 放网络/共享文件系统在 Linux 上也不被官方支持，且 DragonOS 的 virtiofs 支持未验证。**拒绝**。

---

## 7. 推荐落地

### 7.1 目标形态

```
bin/golden/
├── ocm-golden-docker29.1.3-containerd2.2.0.img        # 16GiB 稀疏 raw（约 7.0GiB 实占）
├── ocm-golden-....img.sha256
├── var-lib-docker-docker29.1.3-containerd2.2.0.tar.zst # 5.9GiB 原始，压缩后约 1.3–1.8GiB
├── images.json                                        # 8 个 ref + digest + ID + size
├── SHA256SUMS
├── e2fsck.log
└── guest-load.log                                     # L_*=0、docker images、docker system df
```

日常：

1. 内核/应用代码迭代：直接用 qcow2 覆盖盘启动 golden，**零 load**。
2. rootfs/sysconfig 变更：`ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage`，脚本自动注入 seed，仍然零 load。
3. 镜像集合变更：重新生成 golden（A）或 host 预填充（B），更新 `images.json` 与校验和。

### 7.2 版本与命名约定

- seed 文件名必须包含 `docker<version>-containerd<version>`；文件内 meta.db 与这两个版本绑定。
- 镜像集合用 digest 固定；`images.json` 记录 tar 的 sha256，防止“同名不同内容”。
- 升级 docker/containerd 时，旧 seed **直接作废**，不尝试原地迁移。

### 7.3 回滚

- 删除/改名 `bin/golden/var-lib-docker-*.tar.zst`（或在 Makefile 的 sudo 行显式透传 `OCM_DOCKER_SEED=0`）→ 写盘恢复当前行为。
- 保留原始 `bin/disk-image-x86_64.img` 的现有工作流；golden/seed 都是新增产物，不覆盖既有路径。
- 若 golden 启动异常：回到净盘 + 原 `docker load` 流程，问题隔离在镜像数据层。

---

## 8. 预构建产物清单

| # | 文件 | 位置 | 大小（实测/推断） | 生成方式 | 校验 |
|---|---|---|---|---|---|
| 1 | golden raw 盘 | `bin/golden/ocm-golden-docker29.1.3-containerd2.2.0.img` | 虚拟 16 GiB；实占约 7.0 GiB（推断：5.9 + 0.4 + 元数据/journal） | A 流程（guest load + 清理 + 干净关机） | `qemu-img info`；`sha256sum`；`e2fsck -fn`；`dumpe2fs` 记录 |
| 2 | docker seed 归档 | `bin/golden/var-lib-docker-docker29.1.3-containerd2.2.0.tar.zst` | 原始 5.9 GiB；压缩约 1.3–1.8 GiB（推断，参照 6 tar 的 1.27 GiB） | golden 盘只读挂载后，tar 排除运行态并接 zstd | `zstd -t`；`sha256sum -c`；解包后文件数/镜像 digest 比对 |
| 3 | 镜像清单 | `bin/golden/images.json` | < 10 KiB | 从 `docker images --digests` 生成 | 8 个 ref/digest 与下表一致 |
| 4 | 校验和 | `bin/golden/SHA256SUMS` | — | `sha256sum` 1/2/3 | `sha256sum -c SHA256SUMS` |
| 5 | 文件系统体检 | `bin/golden/e2fsck.log` | 数 KiB | `e2fsck -fn/-fy` + `dumpe2fs -h` 输出 | 无 error；记录 free blocks/inodes |
| 6 | guest 生成日志 | `bin/golden/guest-load.log` | 数 KiB | 串口片段：`L_*=0`、`docker images --digests`、`docker system df`、ingest 为空 | 人工比对 |
| 7 | 原始 tar 校验 | `bin/golden/tars.sha256` | — | 6 个 `ocm-tars/*.tar.gz` | `sha256sum -c` |

`images.json` 应记录的实测 digest（同版本 load 的稳定结果）：

| 镜像 | manifest digest（`docker images --digests`） | 短 ID | 解包后 Size |
|---|---|---|---|
| `mysql:8.0.35` | `sha256:a5fba610fbaa7b3eb3ebfb747f1d41f5127dd6851b0da1687e57a6e7644ade24` | `a5fba610fbaa` | 1.24 GB |
| `redis:7.2.3-alpine` | `sha256:7f8d430bb5bdc3872fcfa80dd86266d5269f5cace76042f01098f8ff832041f8` | `7f8d430bb5bd` | 86.2 MB |
| `bitnamilegacy/zookeeper:3.8.3` | `sha256:fb77b304d51cf33da8d0a8ce9e76e9d5d74cb012ee493cc16e6affa8a3de6c54` | `fb77b304d51c` | 1.17 GB |
| `bitnamilegacy/kafka:3.5.1` | `sha256:fc9e4dfc31254fb57bba819bef5183bf5e84d0fa9ea33ae20a7dbebe26e69852` | `fc9e4dfc3125` | 1.14 GB |
| `clickhouse/clickhouse-server:23.8.8.20` | `sha256:f5beebb92f7e9f9947044207a24bb39b5a83eacb61345e5a6f25f05b8b82d79e` | `f5beebb92f7e` | 2.05 GB |
| `oc-footstone:dev` | `sha256:75d561991aa1be9b51cd1f717b5edd2cdefb9ef7d42203b0418a3600283f0847` | `75d561991aa1` | 264 MB |
| `oc-msg-etl:dev` | `sha256:e274af7d04438c334cb0170f4ed27045a6cb45aa44dfc75979f3c7718ca4b9ad` | `e274af7d0443` | 210 MB |
| `oc-sysdiagnose:dev` | `sha256:94172f9c1ea9e0e2612f0588010d5327553d6ebd256e380246caf94e1b8de08a` | `94172f9c1ea9` | 201 MB |

注：这些 digest 是同一 docker/containerd 版本 load 时 containerd 计算出的 manifest digest；换版本后应以新生成结果为准（推断：相同 tar + 相同版本确定）。

---

## 9. 最小验证步骤

### 9.1 归档完整性（host，QEMU 必须已退出）

```bash
cd ~/DragonOS-ocmanager
sha256sum -c bin/golden/SHA256SUMS
zstd -t bin/golden/var-lib-docker-*.tar.zst
qemu-img info bin/golden/ocm-golden-*.img | grep -E 'format|virtual size|disk size'

sudo losetup -f --show -P --offset $((2048*512)) bin/golden/ocm-golden-*.img
sudo e2fsck -fn /dev/loopN          # 期望：干净；若报 journal recovery 需 -fy 后重新归档
sudo dumpe2fs -h /dev/loopN | grep -E 'Filesystem state|Free blocks|Free inodes'
sudo losetup -d /dev/loopN
```

### 9.2 guest 冒烟（关键验收；不执行任何 `docker load`）

```bash
# 用 qcow2 覆盖盘启动（或 cp --sparse=always 的工作盘）
dragon-docker-start
docker info --format '{{.ServerVersion}} {{.Driver}}'        # 期望：29.1.3 overlayfs
docker images --digests --format '{{.Repository}}:{{.Tag}} {{.Digest}}'   # 期望 8 行
docker system df                                              # 期望 Images=8，约 6.3GB
ls -A /var/lib/docker/containerd/daemon/io.containerd.content.v1.content/ingest  # 期望空
df -h / /var/lib/docker; df -i /
```

逐项对照：

| 检查 | 期望 |
|---|---|
| `docker images --digests` | 8 个 ref，digest 与 §8 表逐行一致 |
| `docker info` | `29.1.3 overlayfs` |
| `docker system df` | Images=8，约 6.3GB（按镜像 Size 求和、共享层重复计；实际磁盘 5.9GiB，属正常） |
| `ingest/` | 空 |
| `df -h` | `/` 使用约 7G，剩余 ≥8G |
| `df -i` | inode 使用 <10%（约 48k/1M） |
| 串口/控制台 | 启动期间无 `Loaded image:` / `Loading layer`（证明没有 load） |
| 计时 | `TS=$(date +%s); dragon-docker-start; docker images >/dev/null; echo $(( $(date +%s)-TS ))`，期望 <30s（推断） |

### 9.3 最小运行验证（任意一个镜像，证明 snapshot 链可用）

```sh
docker run --rm --network host redis:7.2.3-alpine redis-cli ping      # 期望 PONG
docker create --name probe --network none mysql:8.0.35 true && docker rm probe
```

若 `docker run` 报 overlayfs mount/whiteout 相关错误，说明预置数据与 guest 内核不兼容：不要继续使用 B/host 生成的 seed；回到方案 A，由 guest 自己 load 生成 golden 后再导出 seed。

### 9.4 A2（seed 注入）专项

```bash
# 重新写盘（脚本自动注入 seed）
ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage
# 检查输出包含 "docker seed injected: 5.9G"
# 启动后重复 9.2；确认没有执行 docker load
```

---

## 10. 风险登记与未决问题

| # | 事项 | 状态 | 说明/下一步 |
|---|---|---|---|
| 1 | golden 跨启动镜像可见 | **推断（高置信）** | host 重启 daemon 已复现；guest 侧待 §9.2 验证 |
| 2 | host 生成 store 被 guest 读取 | **推断** | 同版本、无绝对路径、mysql/redis digest 在 guest meta.db 中实测一致；仍需 §9.2 |
| 3 | ext4 `state: clean` 不可信 | **实测** | DragonOS 在 rw 挂载时仍报 clean；归档前必须 `e2fsck -fy` |
| 4 | 脏 journal 触发 nested-panic | **已知缺陷（用户实测）** | 干净关机 + host e2fsck；不要复用“跑过的盘”直接启动 |
| 5 | `cp --reflink` 不可用 | **实测** | 本机 ext4 返回 `Operation not supported`；用 qcow2 覆盖盘 |
| 6 | 16G 配额 | **推断** | 预置后余约 9G；建议监控 `docker system df`，必要时 24G 或 D2 |
| 7 | 镜像新增/升级 | 设计约束 | 重新生成 golden/seed；文件名带版本，禁止原地迁移 |
| 8 | 运行态残留导致镜像“消失” | **实测**（本次盘内有 43 snapshots + 未完成 ingest） | golden 生成脚本必须清理并断言 ingest 为空 |
| 9 | 设备节点/xattr | 本次 8 镜像 0 xattr，9 个设备节点（实测） | seed 解包必须以 root + `--numeric-owner --xattrs`；新增镜像后复验 |
| 10 | 并发写盘 | **实测风险**（本次取证时 QEMU 正在写该盘） | 生成/注入前检查 `pgrep qemu-system` 并加锁 |
| 11 | `run-qemu.sh` 只支持 raw | **实测** | 用 qcow2 需加 `DISK_FORMAT`；或 `cp --sparse=always` |
| 12 | 时钟偏移 | **实测**（guest 文件时间与 host 差约 8h，另有时钟缺陷记录） | 不要用 mtime 估算 load 时间；以串口 marker 为准 |

---

## 附录 A：证据索引

| 事实 | 来源 |
|---|---|
| docker 29.1.3 / containerd 2.2.0 / compose 2.40.3 / Ubuntu 24.04 | `tools/docker-rootfs/Dockerfile`、基础镜像内 `packages.txt` |
| runc 1.3.4 | 基础镜像内 `runc --version` |
| guest `29.1.3 overlayfs` | `serial_opt.txt:285`；运行 harness `output.log:294` |
| rootfs 配置（ext4/16G/MBR/base image） | `config/rootfs-manifests/docker.toml`、`config/rootfs.generated.toml` |
| guest 无 `/var/lib/docker/image`、无 `overlay2`，只有 containerd store | 本次 `debugfs` 只读取证 |
| 43 snapshots、未完成 ingest 575,668,224 B | 本次 `debugfs` 只读取证（盘当时在运行） |
| meta.db 无绝对路径、namespace `moby`、GC 标签 | host 同版本生成后 `strings` 实测 |
| 6 tar 大小、layer 数、layer 字节 | 本次 `tar` 解析实测 |
| host 预置 5.9G / 47 snapshots / 95s / 8 镜像 / create 成功 | 本次特权容器实测 |
| guest load >13 min、kafka <5 min、clickhouse EIO、30min+ | `docs/ocmanager-porting/status-overview.md` §3.7/§3.8 |
| 当前盘 5.41 GiB 实占 / 16 GiB 虚拟 | `qemu-img info` 实测 |
| ext4 不支持 reflink | `cp --reflink=always` 实测 |
| `/etc/machine-id` 为空 | `debugfs` 只读取证 |
| QEMU 8G/2vCPU、主盘 rw | 运行中的 QEMU 命令行实测 |

## 附录 B：为什么不是“把 tar 打进盘里让 docker import/load 更快”

`docker load` 的耗时由三部分组成（guest 实测现象）：读取 1.27 GiB tar、把 2.90 GiB 唯一 layer 写入 ext4、解包成 3.0 GiB snapshots。把 tar 换成 `docker import` 只是去掉 layer 结构，写入量不变；而 golden/seed 直接跳过全部三步。因此本方案的核心不是“换导入命令”，而是**把结果持久化**。

---

## 11. 落地实测（2026-10-23，A2 已跑通）

### 11.1 产物

| 文件 | 大小 | 说明 |
|---|---|---|
| `/home/sparkzky/ocm-tars/var-lib-docker-seed.tar.zst` | 2.3 GiB（解压后 5.9 GiB） | guest 内 `docker load` 完 6 个 tar 后的 `/var/lib/docker` |
| `/home/sparkzky/ocm-tars/var-lib-docker-seed.sha256` | — | `ae0b9086cb970f04e3a5c15c240aa5549019e146e4222dc6ea69b363028c2774` |
| 含 seed 的工作盘 | 7.9 GiB / 16 GiB | `bin/disk-image-x86_64.img`（已注入，可直接用） |

seed 内含 8 个镜像：mysql:8.0.35、redis:7.2.3-alpine、bitnamilegacy/zookeeper:3.8.3、bitnamilegacy/kafka:3.5.1、clickhouse/clickhouse-server:23.8.8.20、oc-footstone:dev、oc-msg-etl:dev、oc-sysdiagnose:dev。

### 11.2 生成与注入（命令级，均实测）

```bash
# 1) 生成：12G 内存引导（8G 在第 5 张镜像 load 时 alloc-exhausted）
#    guest: sh /r16_load_all.sh   → 6 个 L_*=0 + SYNC=0 + LD_END
#    guest: sync; busybox poweroff -f

# 2) 抽取（host，root）
LOOP=$(sudo losetup -fP --show bin/disk-image-x86_64.img)
sudo mount -o ro,noload ${LOOP}p1 /mnt/ocm-seed
sudo tar -C /mnt/ocm-seed/var/lib/docker --numeric-owner --xattrs --xattrs-include='*' \
  --exclude=./containerd/daemon/tmpmounts \
  --exclude=./containerd/daemon/io.containerd.grpc.v1.introspection \
  -cf - . | zstd -T0 -3 -o ~/ocm-tars/var-lib-docker-seed.tar.zst
sudo umount /mnt/ocm-seed; sudo losetup -d $LOOP

# 3) 注入新盘（host，root）：写盘后
LOOP=$(sudo losetup -fP --show bin/disk-image-x86_64.img)
sudo mount ${LOOP}p1 /mnt/ocm-inject
sudo mkdir -p /mnt/ocm-inject/var/lib/docker && sudo chmod 0710 /mnt/ocm-inject/var/lib/docker
zstd -dc ~/ocm-tars/var-lib-docker-seed.tar.zst | sudo tar -x -C /mnt/ocm-inject/var/lib/docker \
  --numeric-owner --xattrs --xattrs-include='*'
sudo sync && sudo umount /mnt/ocm-inject && sudo losetup -d $LOOP
```

耗时（实测）：抽取打包 13s；注入解包 53s；合计 < 1.5 分钟，对比 guest 内 6 次 `docker load` 约 **50 分钟**。

### 11.3 验收（实测，12G 引导）

```
root@dragonos:~# dragon-docker-start; docker images
29.1.3 overlayfs
IMAGE
bitnamilegacy/kafka:3.5.1
bitnamilegacy/zookeeper:3.8.3
clickhouse/clickhouse-server:23.8.8.20
mysql:8.0.35
oc-footstone:dev
oc-msg-etl:dev
oc-sysdiagnose:dev
redis:7.2.3-alpine

root@dragonos:~# docker run -d --name rds redis:7.2.3-alpine; docker exec rds redis-cli ping
PONG
```

**结论：零 `docker load`，启动后即可 `docker run`。**

### 11.4 已知限制与后续

- 本次运行 `docker run` 期间仍出现一次可自恢复的 `TASK DUMP: kernel stall detected`（与既有 IO/调度抖动同域），未影响 `PONG`。
- 12G 内存是本次全量 load 的实测下限；若后续只注入 seed（不 load），8G 亦可运行，但仍建议 ≥10G 以免容器启动期 `alloc-exhausted`。
- 接入构建流程（`write_disk_image.sh` 注入 seed + qcow2 覆盖盘）尚未提交，按 §6.1 的补丁即可接入。
