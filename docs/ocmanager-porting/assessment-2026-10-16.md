# OCManager 移植 DragonOS 评估报告（调研阶段）

- 日期：2026-10-16
- DragonOS 基线：origin/master @ `a4013617`（worktree `~/DragonOS-ocmanager`）
- ocmanager 代码：`~/ocmanager`
- 结论速览：**Docker 引擎本体（29.1.3 + containerd 2.2.0 + overlayfs + cgroup v2 + nft iptables）已可在 DragonOS 上运行单容器**；但以 compose 全栈拉起 ocmanager 目前被 **5 个实测确认的缺陷**阻塞，全部集中在内核（详见 §3）。另有若干按代码证据确认的次级缺口（§4）与部署形态问题（§5）。

---

## 1. OCManager 是什么 / 部署形态

OpenCloudOS 智能运维平台：Vue3 前端 + Go 后端（manager：footstone/msg-etl/sysdiagnose）+ 数据通道（tms：api/gateway/asset-manager/heartbeat/task-manager/file-svc/data-proxy）+ 基础设施（MySQL/Redis/Kafka/ZooKeeper/ClickHouse）。**全部 16 个容器经三层 docker-compose 编排**：

| 层 | 容器 | 运行时 |
|---|---|---|
| infra（5） | mysql:8.0.35 / redis:7.2.3-alpine / bitnami/zookeeper:3.8.3 / bitnami/kafka:3.5.1 / clickhouse-server:23.8 | glibc C / Java(bitnami, UID 1001) / C++ |
| tms（7） | api gateway asset-manager heartbeat task-manager file-svc data-proxy | Go 1.24 静态二进制 + alpine:3.19（file-svc 用 SQLite） |
| manager（4） | footstone / msg-etl / sysdiagnose / frontend(nginx:1.25-alpine) | Go 静态二进制 / nginx |

依赖的 Docker 特性：**bridge 网络驱动 + 自定义子网 172.22.0.0/24 + external network（ocmanager-net 跨项目共享）+ 网络别名（ocm-redis/ocm-kafka）+ embedded DNS 服务名解析**、named volumes、单文件 ro bind mount（SQL 初始化目录）、healthcheck（容器内 exec：mysqladmin/redis-cli/bash /dev/tcp/wget）、ulimits(nofile 262144)、端口发布（含 127.0.0.1 绑定）、depends_on、restart 策略。

官方资源建议：POC 4C/8G 起，全栈功能验证 8C/16G（`docs/FAQ.md`）。

## 2. 移植策略（已验证可行）

1. **镜像全部在 Linux host 预构建**：`Dockerfile.backend` 是"预编译静态二进制 COPY 进 alpine"模式（`make build-bin-static`，CGO_ENABLED=0），前端多阶段（node18 构建、nginx 托管）。host 已具备 docker 28.5.1 + compose v2.40.3 + go 1.25.3。**guest 内不需要 Go 工具链**。
2. **镜像交付**：guest 直连 docker.io 拉取已验证可行（SLIRP 出网+DNS 通），但见 §3-E 大镜像拉取卡死问题；稳妥路径是 host `docker save` → 附加 ext4 盘（`bin/ext4.img`，run-qemu.sh 自动挂为 /dev/vdb）→ guest `docker load`（该通道已验证）。
   - ⚠️ Bitnami 老版本镜像已从 docker.io 下架（`bitnami/zookeeper:3.8.3` 404），需改用 `bitnamilegacy/` 或保留腾讯镜像源（compose 默认 `mirror.ccs.tencentyun.com`，目标网络需可达）。
3. **运行时 rootfs**：仓库自带 docker profile（`config/rootfs-manifests/docker.toml` → `tools/docker-rootfs/`，Ubuntu24.04 + docker-ce 29.1.3 + containerd 2.2.0 + iptables-nft）。构建：host `docker build -t dragonos-docker:ubuntu2404-29.1.3 tools/docker-rootfs/` → `ROOTFS_MANIFEST=docker make run-nographic`。
4. 本调研在 worktree 做的脚手架改动（未提交，属移植工作首批物料）：
   - `tools/docker-rootfs/Dockerfile`：补装 `docker-compose-plugin=2.40.3`（原镜像缺失，guest 内 `docker compose` 不可用——实测补装后 v2.40.3 正常）
   - `config/rootfs-manifests/docker.toml`：size 4G→16G（4G 装不下镜像集，实测盘满后 ext4 元数据刷盘 EIO 风暴）
   - `tools/run-qemu.sh`：`QEMU_MEMORY` 支持 `DRAGONOS_QEMU_MEMORY` 环境变量覆盖（原硬编码 2G）
   - `config/app-blocklist.toml`：临时屏蔽 virtiofs_bench（nix devshell ld 缺静态 libc；host 工具链编 user 程序即可绕开）

## 3. 实测确认的硬阻塞（按优先级）

环境：QEMU q35+KVM、2 vCPU、2G RAM、SLIRP 网络、docker rootfs 16G。
基线通过项：dockerd 启动（29.1.3/overlayfs/cgroup v2）、docker0 与自定义 bridge 创建、redis 容器 run/exec（PONG）、registry 拉取（alpine/redis/zk）、根 netns 完整 iptables 规则（DOCKER 链/MASQUERADE/DNAT 钩）、跨容器裸 IP ping（172.22.0.2↔0.3，0% 丢包）、host→guest ext4 盘传文件。

### A. overlayfs 合并目录 getdents 枚举损坏（阻塞所有 entrypoint 类工作负载）
- 现象：容器内 `ls /opt/bitnami/zookeeper/conf.default/` → `Permission denied`；`ls /usr/bin`（redis 容器）→ 空输出；`stat` 同一目录正常（0775 root:root）；root+全 capability 同样失败。guest 本机 ext4 目录枚举正常（对照通过）。
- 影响：bitnami ZK/Kafka entrypoint（`cp -R` 拷贝默认配置）启动即挂，**JVM 根本没机会启动**；MySQL 官方 entrypoint 扫描 `/docker-entrypoint-initdb.d/`、nginx 加载配置目录、apk/apt 均依赖目录枚举。
- 定位方向：overlayfs merged 目录的 getdents/权限路径（`kernel/src/filesystem/overlayfs/`），路径查找与 exec 正常、仅枚举坏。

### B. 容器 netns 内 iptables-nft 规则编程失败 → Docker embedded DNS 死亡（阻塞 compose 服务名互连）
- 现象：`dockerd` 日志 `Resolver Start failed ... [-t nat -I DOCKER_OUTPUT -d 127.0.0.11 -p udp --dport 53 -j DNAT --to-destination 127.0.0.11:39139]`；容器内 `nslookup <别名> 127.0.0.11` 超时。启动期有 `standard netlink families 6 is not supported yet`（NETLINK_NETFILTER）。
- 对照：根 netns 的 nat/filter 全套规则编程正常（docker0/probe-net 的 MASQUERADE 均已落上）。
- 影响：compose 栈内 `mysql`/`redis`/`kafka`/`clickhouse` 等服务名解析全灭 → ocmanager 全栈无法以标准方式拉起。
- 定位方向：netlink socket 与非根 netns 绑定/每-netns nftables ruleset（`kernel/src/net/socket/netlink/`、`kernel/src/net/nftables/`、`kernel/src/process/namespace/net_namespace.rs`）。

### C. 内核内存回收不能承受大镜像导入 → panic（阻塞 mysql 等大镜像落地）
- 现象一（2G）：`docker load -i mysql-8.0.35.tar`（606MB）→ 用户态缺页 `VM_FAULT_OOM`，`oom: no victim` → **内核 panic**（Linux 语义应杀进程）。
- 现象二（2G，另一次）：`global_alloc_error, layout: size 4096` → panic×3 → halt。
- 影响：所有 ≥600MB 的镜像（mysql/kafka/clickhouse/nginx、乃至 11 个 Go 服务镜像打包）无法导入/落盘。
- 定位方向：页缓存/内核堆回收（`kernel/src/mm/oom.rs`、page_reclaimer）、OOM victim 选择。

### D. QEMU 内存 >2G 时启动挂死（阻塞资源达标）
- 现象：`-m 6G` 与 `-m 3584M` 均停在 `Try to mount root fs...`（串口 11min/6min 无输出，vCPU 0.3% 空转等待）；`-m 2G` 稳定启动。挂起点在 virtio-blk rootfs 挂载。
- 影响：官方建议 ≥8G 内存；当前只能 2G 运行 → 与 C 叠加，无法承载全栈。
- 定位方向：高内存布局下 virtio-blk DMA（>4G 物理地址/中断路由），需上游确认。

### E. 大镜像 registry 拉取稳定卡死（旁路：save/load，但又被 C 阻塞）
- 现象：`docker pull mysql:8.0.35` 反复停在 `Pulling fs layer`（无字节进度）；同会话 alpine/redis 拉取正常。
- 定位方向：大 blob 长连接 TCP/TLS（SLIRP + DragonOS TCP 栈，怀疑零窗口/分段路径）。

## 4. 代码证据确认的次级缺口（scout 调研，均给出内核路径）

| 类别 | 缺口 | 证据 | 对 ocmanager 影响 |
|---|---|---|---|
| 异步 I/O | io_uring 与 POSIX AIO（io_setup/io_submit/io_getevents）均未实现（仅 syscall 号） | 全仓库无实现 | ClickHouse 需退化线程池同步 I/O（用户态可配置）；MySQL 必须加 `--innodb-use-native-aio=0` |
| 文件 | fallocate 仅 mode=0，PUNCH_HOLE/KEEP_SIZE 返回 EOPNOTSUPP | `kernel/src/filesystem/vfs/syscall/sys_fallocate.rs:1-40` | InnoDB undo 截断/双写压缩、CH 稀疏文件持续报错 |
| sysctl | /proc/sys/vm 仅 drop_caches+oom_fault_inject，无 overcommit/max_map_count/swappiness | `kernel/src/filesystem/procfs/sys/vm.rs:34-60` | MySQL/CH/容器运行时读取这些文件得到 ENOENT（多数有降级，部分脚本会失败） |
| 调度/内存 | membarrier 未实现（ENOSYS，实测 runc/JVM 路径可容忍） | nr.rs:333 无 handler | 低风险 |
| futex | FUTEX_WAIT_REQUEUE_PI/CMP_REQUEUE_PI 是 `todo!()`（调用即 panic） | `kernel/src/libs/futex/syscall/sys_futex.rs:253-258` | PI mutex 消费者会打崩内核；Java 默认不用 PI，风险低但属地雷 |
| 随机数 | getrandom 为弱 PRNG（无 CRNG） | `kernel/src/syscall/sys_getrandom.rs:82` | TLS/mTLS（gateway 的 CA 体系）密钥质量隐患 |
| 性能 | 无 vDSO；clock_gettime/getcpu 全走 syscall | 全仓库无 vdso | JVM/Go 高频时钟调用性能受损；大镜像层解压 500MB≈2min+ |
| 语义 | 盘满时 ext4 元数据刷盘返回 EIO 而非 ENOSPC（伴随 writeback 告警风暴） | 实测 serial 日志 | 应用无法正确处理空间不足 |

JVM 相关逐项核查（rseq✅/futex 常规✅/mprotect W^X✅/epoll/timerfd/memfd/procfs✅）：JVM 启动所需基础 syscall 基本齐备，**当前 ZK 失败与 JVM 无关，纯粹是缺陷 A**。

## 5. 部署形态待办（内核之外）

1. **服务自启**：guest 是 busybox init（`user/sysconfig/etc/inittab`），无服务管理；dockerd 靠手动 `dragon-docker-start`。需在 rcS/init 路径加入 dockerd + compose up（或引入服务框架）。
2. **QEMU 端口暴露**：run-qemu.sh 仅 hostfwd 一个端口（→guest:12580）；host 访问 ocmanager Web（13070）等需追加 hostfwd 或改用网桥。
3. **compose 密钥注入**：deploy.sh 要求 shell export 三个密钥（CA passphrase/token/HMAC），guest 内需等效注入；sysdiagnose 制品上传步骤（tms/api HTTP）需在 guest 执行。
4. **磁盘容量**：全栈镜像约 4-5GB 压缩/8GB+ 解压 + 数据卷；docker profile 建议 ≥30G；host 侧需预留构建与 save 空间。
5. **文件分发**：ocmanager 仓库（compose 文件+configs+migrations bind mount 源）需进入 guest：走 sysconfig 打进 rootfs，或 ext4 附加盘/virtiofs（`make qemu-virtiofs-nographic`，DAX 支持已有）。

## 6. 建议的攻坚顺序

1. 修 A（overlayfs getdents）——解锁 entrypoint 类容器，复测 ZK/JVM；
2. 修 B（netns 内 nftables 编程）——解锁 embedded DNS，compose infra 层即可拉起验证（redis+mysql 健康检查通过为标志）；
3. 修 C+D（内存回收 + >2G 启动）——解锁大镜像与全栈资源需求；
4. 复测 E（大 blob 拉取）；补 compose 栈逐层验证（infra → tms → manager）；
5. 按 §4 清单处理 AIO/fallocate/sysctl 次级缺口与 §5 部署形态项。

## 7. 复现环境备忘

- worktree：`~/DragonOS-ocmanager`（origin/master@a4013617）
- 构建：host 工具链 `ROOTFS_MANIFEST=docker make all`（nix devshell 内 ld 缺静态 libc，user 程序须在 host 编）；`ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage`
- 启动：`cd tools && ROOT_PATH=$PWD/.. bash run-qemu.sh --bios=legacy --display=nographic`（内存默认 2G，>2G 挂死）
- guest：Enter 激活控制台 → `dragon-docker-start`；镜像传输盘：`bin/ext4.img` → `mount -t ext4 /dev/vdb /mnt2`
- 串口日志：`~/DragonOS-ocmanager/serial_opt.txt`；dockerd 日志：guest `/tmp/dockerd.log`

---

## 8. 开发阶段进展（2026-10-17 更新）

### 已修复并经 QEMU 实测验证 ✅

| 缺陷 | 根因 | 修复 | 验证 |
|---|---|---|---|
| A overlayfs 目录枚举 | `OvlInode::find()` 不处理 `.`/`..`，点号透传 `MountFSInode::lookup_backing` 返回 EACCES；深度≥2 目录 getdents 必挂 | `kernel/src/filesystem/overlayfs/lookup.rs` find() 目录先行应答点号 | `ls /usr/bin` 143 条目；ZK entrypoint 走通、JVM 完整启动、`ruok/srvr` 应答；相对路径 `..` 分量正常 |
| B embedded DNS | iptables-nft 将 `--dport` 序列化为 xt "udp" match，内核 `xt_match()` 只实现 tcp/addrtype/conntrack → ENOENT → batch 中止（与 netns 无关，family 6 警告是 XFRM 红鲱鱼） | `nftables/rule.rs` 等实现 NftXtUdp（Linux 6.6 xt_udp 语义）+ dunitest | 127.0.0.11 解析别名/容器名 ✅；compose（healthcheck/depends_on/网络/别名）全链路拉起 ✅ |
| G open EINVAL | `FileFlags::from_bits` 严格拒绝未定义位；Linux 忽略未知位（mysqld 传 0x1e8 含 0o10/0o40） | `open_utils.rs`/`sys_openat.rs` 改 `from_bits_truncate` | mysql entrypoint `is_writable` 创建通过，进入 InnoDB 初始化 |
| times() 缺失 | SYS_TIMES 未实现，mysqld/bash 轮询刷出 2530 万条 ENOSYS 日志淹没控制台 | 新增 `kernel/src/time/syscall/sys_times.rs`（rusage utime/stime+children） | 控制台干净，mysql 日志可读 |

### 已实现待复测 ⏳
- **C OOM/direct reclaim**（mm/page.rs try_direct_reclaim + alloc_in_buddy 回收重试 + NoVictim 兜底杀触发进程 + 防递归守卫）：待 docker load 2G 复测
- **F 脏 ext4 挂载**（jbd2 日志尾重放 + 挂载失败显式 panic）：主路径验证通过（EIO 消除）；**残余**：RECOVER=true 重放路径在 recovery 入口前出错（错误码消息体被 hvc0 单深度 TX 队列丢弃）后静默挂住——第三轮调查中（重放进度日志+BIO 卡死探针已入内核）

### 判定修正
- **D（>2G 启动挂死）为误诊**：A+B 内核 + 全新磁盘在 6G 完美启动（含 >4G 高区内存，virtio DMA 正常）。原挂死全部由脏 ext4 触发（即缺陷 F），与内存量无关
- **E（大镜像拉取卡死）**：6G 下 mysql 606MB 拉取成功（约 12 分钟，I/O 慢但稳定）；2G 停滞疑为 C 的内存压力表象，待 C 复测确认

### 新发现问题（待处理）
- **mysql 临时服务器阶段 guest 楔死**：entrypoint → InnoDB 初始化 ✅ → native AIO 优雅降级 ✅ → 系统表创建 ✅ → 临时服务器启动后全系统楔死（vCPU 0%，bio 探针无输出）——复现中
- ptrace×execve：strace 跟踪下 execve 永不返回（独立内核缺陷）
- FAT BPB 解析对损坏盘 div0 panic（`fat/bpb.rs:269`，健壮性）
- 非 root DAC 放水：uid 27 可写 root 属主 755 目录（volume/bind 路径）
- 关键日志消息体在控制台背压下丢失（error/panic 可诊断性）

### 里程碑
Docker Compose 机制（网络/别名/embedded DNS/healthcheck/depends_on/编排）在 DragonOS 上全链路工作；ZooKeeper（JVM）容器完全功能化。剩余主路径：F 残余、mysql 服务器阶段楔死、C 复测。

## 9. 第二日进展（2026-10-17 晚更新）

### 新增修复
- **关键日志通道**：error/warn/panic 全部改走同步 virtio-console 通道，消息体不再被控制台背压丢弃——此基建揭穿了所有"静默挂死"的真身
- **do_kern_addr_fault oops 语义**（主 agent 补完）：进程上下文内核态 fault 改为打印现场+终止任务（对齐 Linux oops），不再全局 panic；中断上下文保持 panic
- **SYS_TIMES 实现**（主 agent）：mysqld/bash 高频调用，此前 ENOSYS 刷屏 2530 万条
- **停顿看门狗+任务转储**（主 agent，`kernel/src/debug/task_dump.rs`，SysRQ-T 等价物）：上下文切换零增长→关键通道转储全部任务 pid/名称/状态

### H（mysql 阶段全系统楔死）根因实锤
- 现象：mysqld 临时服务器阶段（稳定复现 3 次）全系统静默楔死，双 CPU halt、无 panic、BIO 探针无声、看门狗不触发
- **根因：LAPIC 定时器中断停摆**——gdb 双读 jiffies 冻结（0x3f5ad 不变）。tick 死→一切定时唤醒（nanosleep/调度）永不发生→看门狗自身也是受害者
- 楔死前最后事件恒为 mysqld 用户态 null-probe fault（rip 0x65451, addr 0x8）——SIGSEGV 交付路径与定时器停摆的关联待查（疑 idle 入口关中断竞态或 one-shot 重编程丢失）

### F 状态
- 轻脏盘 journal 重放已验证通过（#34 内核：replaying now → Successfully migrate rootfs）
- 重脏盘（mysql 数据）仍有 -15 链：脏 extent 树无限递归→内核栈写穿→PCB 槽被 errno 覆写。修复方向明确（extent 深度链校验+PCB 槽合理性检查），第四轮实现曾因限额中断留下半截编辑并**引入启动三重故障循环**（已 git checkout 还原，#33/#34 恢复正常），待安全重做

### 流程教训
- subagent 中断（限额/超时）可能留下编译通过但语义破坏的半截编辑；恢复后必须先 git status 审计增量再继续

## 10. 第三日进展（2026-10-17 深夜）

### H（mysql 阶段全系统楔死）修复验证通过 ✅
- 修复（FixerTimerDeath）：clocksource_delta 回退读数守卫 + tick 异常重锚定 + one-shot 截断告警
- **第 5 次复现场景（此前 4/4 必死）系统存活**：mysql 容器 Up 9 分钟、sleep 定时唤醒正常（TICK1/TICK2）、shell 响应正常。此前 6 分钟串口静默后自行恢复

### F（脏 ext4）当前状态
- 重脏盘（楔死会话残留）启动：journal 重放启动后 **PCB 槽守卫精准捕获栈写穿**（`value=0x1 not aligned`，明确诊断替代静默死亡）——防护体系按设计工作
- 残余：extent 四处防护之外**仍有第五处递归/深栈**在重放路径炸栈（第五轮修复中，等 API 限额恢复 22:02）

### 新前沿：dockerd 用户态退化（区别于内核 H）
- H 修复后，mysql 到达临时服务器阶段，内核存活，但 dockerd 的 CLI 请求挂起（docker logs 90s+ 无响应），shell 被前台命令阻塞
- 候选方向：mysqld 重 I/O 下 dockerd/containerd 的内存压力（C 域）或 shim 管道问题

### 本日新增基建（主 agent）
- 串口键触发停摆取证探针（console_input_stall_probe）：控制台 IRQ 旁路检测 jiffies 冻结，转储 x2APIC 定时器 MSR + deadline 队列 + 全任务状态（本次未触发=未发生定时器死亡，侧证 H 修复生效）
- kvm_clock_read pub(crate) 化、TIMER_JIFFIES/QUEUES 访问器

### 残余楔死家族的统一框架（I 调查结论，2026-10-17 21:35）
- 三种退化形态实测：①定时器停摆（H，已修）；②procfs 读取渐进挂起（ps/cat 阻塞，shell 内建存活）；③本轮：全 IRQ 死亡（console RX 无回显、热键 D 无法触发、vCPU 0%）
- 统一嫌疑：**中断投递/tasklet 机制在重 I/O 负载下的渐进性退化**（virtio-console RX 走 `VIRTIO_CONSOLE_RX_RETRY_TASKLET` tasklet 路径；IOAPIC 向量屏蔽/软中断队列 jams 均可解释三种形态）
- 已建工具：任务转储热键（'D'，IRQ 存活时可用）、串口键停摆探针、BIO 卡死探针——在 IRQ 死亡形态下均失效，下一步需 IRQ 投递层追踪（IRQ vector 入口计数 + IOAPIC/APIC 屏蔽状态转储，可在 NMI 或时钟之外的强制路径注入）

### 下次会话起点
1. F 第五轮（重放路径第五处递归）——限额 22:02 恢复后唤醒 FixerExt4Mount
2. 残余楔死：IRQ 层追踪基建（入口计数/屏蔽状态取证）
3. C 第二轮 2G 复测
4. mysql ready 终验（被残余楔死阻塞）

## 11. IRQ 投递实验结论（2026-10-17 23:00，ftrace kvm_inj_virq + QMP 物理读）

### 实测数据（楔死现场）
- ftrace 直方图（全程）：0x97(timer) 注入 337 万次、0x40/0xc8（块设备/其他）56 万/45 万——**中断注入从未停止**
- 注入间隔：0x97 每 ~0.8ms 一次（≈1250Hz），持续整个会话
- **TIMER_JIFFIES 活着**（QMP 物理读双采：5 秒 +1178 tick≈235/s）——定时器处理与调度核心存活
- vCPU 稳定 13%、串口死、shell 死

### 结论修正
- **「中断投递断路」假说否定**：KVM 一直在注入，vCPU 线程正常 kvm_vcpu_block，中断正常送达
- **真正图景：guest 侧活锁/饥饿**——1250Hz 定时器中断（FixerTimerDeath 的 re-anchor/one-shot 修复在 mysql 负载模式下的副作用嫌疑：某路径持续以极短 delta rearm）+ 仅 ~19% 被服务为 tick；其余 CPU 时间被定时器处理路径消耗，用户态/串口任务饥饿
- 之前的「jiffies 冻结」楔死（H 原型）与本形态并存——残余家族至少两类：①真停摆（H 已修）②定时器风暴饥饿（本轮实锤，新）

### 下一步（已具备的抓手）
- 复现时带 `-gdb tcp::PORT` 的手动 QEMU：gdb 直接遍历任务栈找活锁线程
- guest 内核 IRQ_VECTOR_COUNTS（符号地址随重建漂移，用 nm 现查）+ TM 期计数已就位
- 交给 FixerTimerDeath 第二轮的核心问题：为什么 deadline 路径会以 0.8ms delta 持续 rearm 而非到下一个真实期限；以及为何 19% 服务率

## 12. F/H 复测与第五、六轮（2026-10-17 深夜第二班）

### H（定时器风暴）根因闭环与修复（FixerTimerDeath 第二轮）
- **根因**：cfs_bandwidth 的 `RuntimeDeadline` 把 CPU 时间预算当**墙钟** one-shot 武装（`arm(now+remaining)`）；任务无实际执行时 remaining 不变 → 固定周期自续（0.78ms）→ 1250Hz 风暴 → 自持活锁（数字全部吻合：19% tick 率=0.78ms/4ms 网格交叉；HZ=250 实测 235/s）
- **修复**：`expire` 加执行计费门控（无 sum_exec_runtime 推进不续排），对齐 Linux「runtime 只随真实执行推进」
- 首轮 H 修复（kvm-clock 回退守卫等）保留为加固且实证零触发

### F 复测矩阵结果
- 净盘回归：✅（#55 正常启动，F5 栈修复零行为变化如预期）
- 重脏盘（timer-storm 会话残留）：❌ 重放起点楔死——`replaying now` 后仅一帧 `[255] slabmalloc deallocate` backtrace，无 panic 头/无 recovery begins/vCPU 0（第六轮进行中：错误展开路径审计）

### 残余楔死家族图谱（当前认知）
| 形态 | 根因 | 状态 |
|---|---|---|
| 定时器停摆（jiffies 冻结） | kvm-clock 跨 CPU 偏差→tick 毒化 | ✅ 已修+验证 |
| 定时器风暴（1250Hz 活锁） | cfs_bandwidth 墙钟自续 | ✅ 已修（待复测） |
| 重放起点楔死 | 脏数据错误展开崩溃（slab 释放损坏结构） | ⏳ 第六轮 |

### 第六轮（F6）：KMSG 锁内堆操作死锁（FixerExt4Mount）
- 根因：`printk.rs` KMSG 记录在自旋锁+关中断内 clone/分配/释放（ring 满驱逐锁内 drop）→ 分配器慢路径睡眠持锁 → 一切 printk（含 panic 头/探针）重入死等。解释重放起点楔死的全部症状（无头/零星帧/0%CPU/盘零写）；timer-storm 日志风暴恰好填满 ring 触发驱逐路径
- 修复：push 纯槽位化（驱逐消息锁外 drop）+ 控制台输出前置于 KMSG 记录

### #58 全修复内核综合复测（2026-10-18 凌晨）
- 净盘启动 ✅；mysql 拉取完成（盘 1.4G）
- **新残余形态**：`docker run` 容器启动点楔死（vCPU 0%、'D' 热键无响应=console RX IRQ 死）——F6/KMSG/cfs 修复之外的独立缺陷，需 gdb 装备 QEMU 直接遍历任务栈定位（下次会话第一优先）

### 残余楔死家族图谱（最终版）
| 形态 | 根因 | 状态 |
|---|---|---|
| 定时器停摆 | kvm-clock 偏差→tick 毒化 | ✅ 修复+验证 |
| 定时器风暴 1250Hz | cfs_bandwidth 墙钟自续 | ✅ 修复（本轮未及单独复测，被下一形态掩盖） |
| 重放起点楔死（无头/零星帧） | KMSG 锁内堆操作死锁 | ✅ 修复（F6） |
| 重放起点楔死（slab 帧） | 同上（F6 修复对象） | ✅ 同 |
| 容器启动点全 IRQ 死 | **virtio 设备中断死亡**（0x40 零注入，timer 活）+timer 9 倍高频残留 | ❌ 下次：队列状态活检+注入源定位（§13） |

## 13. 容器启动点楔死终极判据（2026-10-18 00:30）

### 活体取证（QMP 物理读 + ftrace 双重确认）
- **LAPIC 本地中断存活**：jiffies 246/s、调度 24 切换/s、0x97 注入 2253/s、0xc8 注入 4/s
- **virtio 设备中断死亡**：0x40（块设备）/0xc9 计数冻结；ftrace 10 秒窗口 KVM **零注入 0x40**——设备侧未产生中断，非 guest 拒收
- 串口死=console RX 暂停（tty 满，shell 阻塞不读 stdin → pump_input 暂停）；dockerd/shim 阻塞在永不完成的块 I/O

### 根因域锁定
**virtio-blk 完成路径死亡**（guest 发出的请求设备永不完成——嫌疑：virtqueue kick 丢失/无屏障、avail→used 推进停滞、或 QEMU 设备侧状态机卡死）+ **timer 0x97 仍有 9 倍高频注入**（2253 注入/s vs 246 tick/s——cfs 修复不完全或存在新 deadline 源，且高频硬中断拿 rq 锁本身加剧干扰）

### 下次会话第一优先（路径已铺好）
1. virtio-blk 队列状态活检：gdb 读 virtio common config（BAR 映射）的 avail.idx vs used.idx——判别「请求未送达设备」vs「设备完成未回写」
2. timer 9 倍放大：ftrace+kvm_apic 找出 2253/s 中非 tick 的 2000/s 注入源（哪个 deadline 在自续——cfs 修复的漏网分支或 posix timer/timerfd）
3. 复测盘：/tmp/f6-dirty-disk.img（F6 验证）、2G load（C 验证）

## 14. 第四日午夜班结论（2026-10-18 01:05）

### 楔死家族最终定位（活检数据链完整）
1. **strace 主机侧 12s 零盘 I/O**：guest 提交管道死（非设备完成路径）
2. **ftrace TMICT=0x2fbaf 同值 0.78ms 自续**（#58 含 cfs 修复仍复现）：`expire()` 门控在通过但 remaining 恒定不降 → 计费域(sum_exec_runtime)与扣减域(runtime_remaining)不匹配；且 `refresh_runtime_event()` 无门控无条件 arm——两条嫌疑路径（已移交 FixerTimerDeath R3，限额 03:03 恢复）
3. **F6 重脏盘复测**：守卫显式捕获 `value=0x1c01d6023`（被篡改的内核指针 ≈stack_base+0x6023，wild-write 特征而非纯深度溢出）→ F7 方向：重放路径 wild-write 审计

### 严重事故：FixerOom 全部改动丢失
- C 第二轮复测失败（global_alloc_error 仍 panic 于 read_block 4K Vec 分配）→ 源码审计发现：try_direct_reclaim/oom_kill_trigger_fallback/emergency_print_noalloc/PANIC_IN_PROGRESS **全部消失**（文件回原始态），仅 fault.rs 残留 1 处
- 时间线：#38 内核（含 R1）实测过 OOM 路径生效；之后某次重建/操作丢失。FixerOom 自己在 R2 也报告"仓库被重置"——worktree 存在未知的外部重置源（或 agent 间 git 操作冲突）
- **待办（03:03 后第一项）**：按 R1+R2 报告规格（报告历史完整保留）重新应用全部 OOM 修复并立即 git add 暂存防丢

### 防再发措施
- 每轮 agent 交付后立即 `git add -A`（快照入索引，重置至少可检出）；重要修复考虑本地 commit

## 15. 第五日进展（2026-10-18 上午）

### OOM 修复回归 + C 复测通过 ✅
- 谜底：FixerOom 的修改从未丢失——它在 DragonOS-12 主仓库（其工作树），从未进过 ocmanager worktree；现已移植
- 2G docker load 复测：`[alloc-exhausted] size=4096/37656; parking task, kernel alive`——**不再 panic halt，内核存活**（核心验收达成）

### F7 交付 + 启动循环事故与修复
- F7：重放路径写入面全部有界（无 wild-write）；value=0x1c01d6023 判型=深栈帧 TSC 计时局部量（IRQ 链叠任务栈，ist=0 无独立 IRQ 栈——结构性残压单列）；补 write_block 范围校验+栈底 hexdump 分型守卫
- **事故**：F7 守卫 hexdump 无条件执行（每次 current_pcb 都打 error!）→ 早期 printk 洪泛 → 启动三重故障循环（2540 次复位）。我条件化修复（仅守卫命中时转储）。期间误回退 R3，已重落

### R3 复测：cfs 风暴确认消灭，但 H1 停摆以新路径回归
- mysql 决定性测试：拉取完成（1.4G）→ docker run 后楔死，vCPU 0%
- **jiffies 冻结**（gdb 双读不变）= tick 停止服务；**但 timer 注入照常 1250Hz**（TMICT 持续写 0x2fbaf/0xf2d85 交替）——不是经典停摆（无注入），是 **tick 分类持续失败**：next_tick 在远期，每次到期判"非 tick"→callbacks→reprogram 循环
- R3 风暴探针未触发（1250 < 2048 阈值）——探针参数已在 R4 任务中修正（降至 512 + 加 next_tick 差值 + 武装者分类标记）
- R4 已派（找 0x2fbaf 武装者 + tick 分类失败根因）

### 当前缺陷谱系（最终）
| 形态 | 根因 | 状态 |
|---|---|---|
| cfs 1250Hz 风暴 | set_next_task 每切换武装 + RETAIN 地板自续 | ✅ R3 消灭（复测无风暴探针输出） |
| tick 停止服务（jiffies 冻结） | next_tick 期限毒化（新路径）| ⏳ R4 |
| OOM panic halt | 分配器无回收重试 | ✅ 修复+验证（parking 存活） |
| KMSG 锁内分配死锁 | printk 记录路径 | ✅ F6 |
| 重放栈写穿 | 4KB 栈帧叠加（已修）+ IRQ 叠栈（结构性） | ✅ F5 / ⏳ IRQ 独立栈单列 |

### R4 探针复测（2026-10-18 12:20）：探针静默但风暴仍在
- #72 内核（含 deadline 全源探针）mysql 楔死：**注入 1756Hz**（ftrace 实证）而 dlprobe/cfs_bw **零输出**——武装路径绕过了 deadline::arm/finish_callback/handle_irq 三个挂钩点（或探针 bug）
- R5 已派：探针下沉到 TMICT 写入点（apic_timer 编程函数入口=唯一 ground truth）锁定武装者身份

### 会话检查点（2026-10-18 12:25）
- 累计修复（已验证）：A/B/G/times/oops语义/H/H'风暴引擎(R3)/F5/F6/C(parking存活)
- 待收口：tick 服务死亡（武装者身份待 R5 探针）、F6 重脏盘终验（等 R4/R5 后联合）
- 全部修复已 git add（52 文件）；复现材料：/tmp/f6-dirty-disk.img、/tmp/t6.dat（风暴现场）

### R5 探针终极数据（2026-10-18 12:55）：kvm-clock 原始读数倒走
- tmict 探针（TMICT 写入点 ground truth）：`1shot=530431 delta=3978021ns(=TICK_NS)`——TMICT 全部来自 tick 分支自重编
- dlprobe：`tick=530431 non_tick=0 arm=0 rearm=0 pend=0 first_in=MAX`——deadline 队列恒空，全部判 tick 空转
- **now32 跨行单调递减**（-100M units/s）——**kvm-clock 原始读数持续倒走**（pvclock 层问题：版本协议缺失或 unstable TSC），R1 守卫把倒走永久钳 0 → timekeeping 冻结 → 1756Hz tick 空转
- R6 已派：pvclock 版本协议核查 + clocksource watchdog 语义（持续倒走→切兜底时钟源）

### R6 复测（2026-10-18 13:25）：watchdog 未触发，最后一环未闭合
- 探针活体输出正常（tmict/dlprobe 每 2048 次一行），kvm-clock 倒走持续（now32 递减），tick 空转风暴持续，楔死复现
- **R6 的「64 次倒置→唤醒 watchdog kthread→强制重选」链未生效**（无 forcing reselection 日志）——嫌疑：watchdog kthread 被风暴饿死 / 倒置计数路径与 probe 读数路径不同链
- 已反馈 FixerTimerDeath（下轮收口点：核实守卫计数器递增路径 + kthread 调度保障）

### 第五日最终检查点（2026-10-18 13:30）
- **已验证修复（12 项）**：A/B/G/times/oops语义/H停摆防护/H'cfs风暴引擎(R3)/F4防护/F5栈帧/F6 KMSG/F7审计+守卫/C OOM(parking存活)/R5探针体系
- **唯一待收口**：kvm-clock 倒走引发的 tick 空转楔死（watchdog 触发链断裂——最后一节）
- 全部 53 文件已 git add；探针/判读表/复测材料（/tmp/f6-dirty-disk.img 等）齐备
- 下一动作：FixerTimerDeath R7（守卫计数路径核实+kthread 调度保障）→ 通过后 mysql ready 终验 → compose infra 全层

### R7 终极判读（2026-10-18 13:55）：kvm-clock 平滑倒走实锤，守卫模型结构性失明
- 四元组探针：`inv=0`（守卫从未触发）+ `base32/cl32/raw32 同步平滑递减`（-201M/interval）——**pvti 内容在平滑倒走**，timekeeper 内部状态平滑跟随，无单次倒置可捕获
- R6 watchdog 失效同理：自一致性双读无法检测方向性
- **真根因推断**：mysql 负载触发 vCPU 迁移 → 新 pCPU 的 pvti 值旧于前一 pCPU（unstable TSC）→ 读数平滑回退。Linux 语义：pvclock 读数需对 tsc_timestamp 做方向校验/标记 unstable
- 修复方向已移交 R8：①watchdog 对照独立 rdtsc（方向性检测）②pvclock tsc_timestamp 交叉校验（真根因）

### R8 复测（2026-10-18 14:15）：钳制层未生效，tick 空转仍复现
- mysql 楔死同前（vCPU 0%、tick=485375 all-tick 空转、无 forcing reselection、inv=0）
- R8 双保险（读数 fetch_max 钳制 + rdtsc 交叉验证）均未触发/未生效——钳制层路径或 watchdog 唤醒链仍有断点（下轮直接在钳制点打印计数即可验证是否到达）

### 会话最终状态（2026-10-18 14:20，跨五日攻坚）
**已验证修复 13 项**：A（overlayfs getdents）/B（xt_udp DNS）/G（open flags）/times()/oops语义/H（tick 毒化防护）/H'（cfs 风暴引擎 R3）/F4-F7（ext4 全链：栈帧/KMSG/审计守卫）/C（OOM parking 存活）/F7b（hexdump 条件化）
**唯一未收口**：kvm-clock pvti 平滑倒走 → tick 空转楔死（R8 双保险未触发的断点待查——探针/判读表/全部历史数据就绪，下一次会话从钳制点计数打印开始，一步可定位）
**方法论资产**：critical 输出通道、四级探针（IRQ 计数/tmict 四元组/dlprobe/cfs_bw）、ftrace+QMP+gdb 活体取证、判读表体系
**工作区**：54 文件 staged；报告 §1-15 完整；复现材料齐备

### F6 重脏盘终验（2026-10-18 14:20）：静默死亡彻底消灭
- storm 会话脏盘启动：journal 重放启动 → **嵌套 panic 守卫显式工作**（`[nested-panic] printing skipped to avoid alloc cascade; parking`——替代了历史上的 panic×220 级联 halt）
- 结论：防御体系（PCB 守卫/KMSG 修复/oops 语义/panic 嵌套守卫/critical 通道）全部实战生效；**所有失败形态都已可见化**。该盘重放本身仍 park（storm 残留的超脏 journal 在重放路径触发 panic-park——已知待解，与 R8/R9 时钟链独立）

### R9 采样（部分）：脏盘启动占用了本轮，mysql 多行四元组采样未完成——下次会话从净盘 mysql 开始

## 16. R9 重大突破（2026-10-18 15:45）：旧叙事全面推翻，双故障定量钉死

### 核心反转：低 32 位截断伪象毒害了 R1-R8 的全部"倒走"叙事
- 全量 64 位探针（now={高}.{低} 格式）证明：**now_ns/base_ns 全程单调**，REGRESS 判定（内核侧全量比较）从未触发
- 旧"−201ms/行平滑倒走"实为 now 全量 +1.11s/行 经 mod 2^32 的截断伪象——R7 的"平滑倒走实锤"结论作废

### 真实故障形态（空闲态即现，无需 mysql）——两个独立缺陷
**缺陷 A：kvm-clock 以 ~25% 真实速率慢走**
- 宿主侧定时：tmict 行 4s/行，now 每行 +0.94s → now_ns ≈ 25% real rate
- 慢钟使 next_tick（now 单位）恒在未来 → dlprobe tick 极少服务（637 vs non_tick 36k）→ jiffies/timer wheel 全部爬行 → mysqld 超时永不达 → "楔死"
- 某些启动段更冻结：raw/cl/now 完全同值 + inv(INVERTED_READS) 爆炸（191k→201k/行）——pvclock 稳态位旁路 LAST_VALUE 棘轮（pvclock.rs:111）+ clocksource_delta 把 backward→0 elapsed 双重作用的形态
**缺陷 B：APIC 定时器 2.03 倍早触发**
- 武装 3.97ms（cnt≈993k counts）实际 ~1.95ms 触发 → 实际计数率 ≈509MHz vs 标定 250.19MHz（apcal 探针实证：双 CPU 标定一致 250.19MHz，elapsed=2,501,925/10ms 窗）
- 与"cpu_khz 标低了 2 倍"（真 TSC=7.78G?物理不可能）或"标定后 DCR/APIC 输入率改变"二选一——**下轮首查：启动后重读 DCR 寄存器值 + 用 PMTMR 交叉测 rdtsc 真实频率**（PMTMR 现已可用：wd_nsec 样本 1.5s 证明）
- apcal 探针已内置（apic_timer.rs:275，打印 elapsed/apic_khz/pcount/cpu_khz/分支/常量）

### 已修入内核（本日，编译验证 #90+）
1. **watchdog 长间隔失明修复**（clocksource.rs:1076）：长间隔不再 `continue` 跳过偏差检测——Linux 6.6 无此豁免；启动期实测捕获 cs=504ms vs wd=1504ms（5 倍慢）进入 unstable 分支
2. **active_clocksource_regressed 增加慢速检测**（clocksource.rs:1157）：<75% TSC 参考速率即判不可信（原仅冻结/倒退，对慢走失明）；注意其自旋窗 khz×1000=1s（注释写 1ms，下轮修正为真 1ms）
3. **R9 探针族**：tmict 全量 64 位（cl/raw 去截断）+ 内核侧 REGRESS 全量判定 + invdet（倒退分支全量值打印，%4096 采样）+ tksetup（timekeeper 安装审计，证明风暴期 mono 实例从未被替换=2 次）+ wdkth 心跳（kthread 确在运行 32+ 次）+ apcal（标定取证）

### 关键排除（避免下轮重走弯路）
- ✗ base_ns 被"替换/重置"——tksetup=2（仅 boot 两次安装），风暴期无任何重装
- ✗ RwLock 写互斥失效——CAS 实现正确（rwlock.rs:342）
- ✗ 除数编码错误——4→0b0001 映射符合 SDM
- ✗ pvclock_scale_delta 数学错误——与 Linux 逐位一致
- ✗ "平滑倒走"——截断伪象（见上）
- ✓ F6 链实战通过：PM2 脏盘 journal 重放成功（1 事务/5 块）；PM3 风暴脏盘重放 panic-park（独立待办不变）

### 下轮收敛路径（一步可判）
1. 启动后 guest 内跑 PMTMR×rdtsc 交叉（或加内核探针）：rdtsc 真频 vs cpu_khz=3,892,686 → 定缺陷 B 归因（标定错 vs 运行时改率）
2. kvm-clock 25% 慢走：打印 pvti 原始字段四元组（system_time/tsc_timestamp/mul/shift）随时间变化——判 KVM 侧 pvti 更新频率/多 pCPU 偏斜
3. 修 A/B 任一即解 mysql 楔死主链：B 修后 fire 恢复 252/s；A 修后 now 恢复真率 → timeout 恢复
4. QEMU 疑似被外部 SIGKILL ×2（exit 137/静默死）——已改 setsid；若复现查 host OOM/cgroup

## 17. R10/R11（2026-10-18 16:30）：三修复落地 + 时间织物深水区

### 已修入内核（编译验证 #95+，全部 Linux 对齐）
1. **acpi_pm 无条件注册**（init/mod.rs:138）：原 `!kvmclock_ok && !hpet_ok` 条件导致 kvm-clock 存在时 PMTMR 永不初始化——watchdog 无参考、系统无备用源。现无条件注册，实测 `pmtmr port=0x608 registered ✓`
2. **watchdog 参考源独立化**（clocksource.rs:independent_watchdog_preferred）：参考=rating 最高者会选 kvm-clock（26% 慢）→ 健康源反向被判"偏快"。现独立硬件时基（hpet/acpi_pm）优先。实测 `wdtick run=true ref=acpi_pm` ✓
3. **APIC 运行形态重标定**（apic_timer.rs:recalibrate_apic_count_runtime）：启动标定（周期+屏蔽+u32::MAX）与运行形态（单次+解屏蔽）在本平台语义不一致 → 以 PMTMR 为参考、运行形态武装重测并刷新 pcount（BSP 在 setup_arch_post、AP 在自身 init 尾部）。实测 `aprecal rate=250.2MHz`，fire 率归一 ~245/s（原 512-2048/s 风暴消失）✓
4. **wdtick/wdcmp/aprecal/pvti 探针族**：watchdog 心跳+参考名、比对原始值（cs/wd delta+last/now 样本）、重标定打印、pvti 四元组

### 关键实测数据（颠覆性）
- **wdcmp：kvm-clock 的 watchdog 比对样本以真实速率前进**（cs=504ms ≈ wd=504ms，jiffies 亦真实速率）——**与 timekeeper 的 now_ns（23%速率）矛盾**：同一时钟源两个读者看到不同速率！
- pvti 四元组：st/ts 冻结（宿主一次性写入）、mul 映射 3.89GHz 正确、rdtsc 真频 3.89GHz（PMTMR 交叉）
- 标定与运行形态重测均得 250MHz → "运行期 8x 改率"假说作废；fire 风暴消失归因于 pcount 刷新/时序
- 空闲态 now=23% + tsc_hz 探针漂移 9.1GHz：时间织物失谐比 mysql 负载更早就存在

### 下轮唯一悬案（一步可判）
**同源双速率之谜**：kvm-clock.read() 在 watchdog 上下文=真实速率、在 timekeeper 上下文=23%。探针方案：tmict 直接打印 tk.mono 的 mult/shift/base/cycle_last 全量四元组 + monotonic_now 与 clock.read() 并排采样——若 mult≠1 即换算注册缺陷；若 read 本身分叉则查 per-CPU pvti 页绑定。
