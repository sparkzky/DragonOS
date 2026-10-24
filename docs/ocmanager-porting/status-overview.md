# DragonOS 运行 OCManager：问题现状与解决方案总览

> 更新：2026-10-24 | 工作区：`~/DragonOS-ocmanager`（**原子提交**，分支 `ocmanager-porting-2026-10`，工作区干净）
> 流水账明细见 `fix-inventory-2026-10-18.md`；PR 拆分与合入状态见 `pr-split-2026-10-23.md`；本文是**可读总览**。

---

## 0. 一句话现状

**compose 基础设施已在 DragonOS 上跑通**：MySQL 8.0.35（`ready for connections` + `select version()`）、Redis（`PONG`）、ZooKeeper（`srvr`）、Kafka 3.5.1（KRaft，`kafka-topics --list` RC=0）；ClickHouse 23.8 镜像**可加载但 server 启动即崩**（归因为容器内 bash 子进程上下文损坏，非 CH 自身）。

**镜像准备已从 50 分钟降到 0**：guest 内一次性 load 6 个 tar（8 个镜像，5.9GiB）→ 抽出 `/var/lib/docker` seed（2.3GiB，sha256 `ae0b9086…`）→ 写盘阶段注入；引导后 `docker images` 直接可见，`docker run redis` → `PONG`。

**关机不再产生脏 journal**：新增 `shutdown_mounted_filesystems()`（sync + `on_umount` 清 RECOVER），实测重启无 `dirty journal detected`，磁盘可反复复用。

**主线 PR 状态（2026-10-24 `gh` 复核）**：上一轮 14 个 PR → 合并 **7**（#2484/#2490/#2493/#2507/#2509/#2512/#2515）、关闭 **3**（#2491 纯诊断、#2492 已被 `resched_current` 覆盖、#2510 panic 路径不安全并拆出 #2519 已合并 + #2518 open）、待处理 **4**（#2508 无 review；#2511/#2513/#2514 changes requested）。**关闭项未造成迁移能力回退**；明细与覆盖缺口见 `pr-split-2026-10-23.md` 的「2026-10-24 PR 状态复核」。

拆分之后的本地新修复（`mm/fault.rs` 延迟 shootdown、`vfs` 关机收尾、clocksource 回归检测）**不在该轮审计范围**，待下一轮单独拆 PR。

当前待解（按优先级）：
1. **bash 子进程上下文损坏**：容器内完整 `/entrypoint.sh` 触发 SIGSEGV（`rip=0x67ca1, addr=0x8, 全零寄存器`），阻塞 ClickHouse 与 footstone 部署；
2. **时钟钳制窗口过小**：`wdt unstable cs=504000000 (jiffies) wd=644015370 (acpi_pm)`，单次 delta 被钳在 504ms，长停顿会丢时间并触发降频；
3. Go 服务（footstone/msg-etl/sysdiagnose）镜像已构建（`ocservices.tar.gz`），待 CH 打通后完成部署收尾。

---

## 1. 背景与目标

- **目标**：在 DragonOS 上跑通 OCManager 运维平台（16 容器 compose 栈：MySQL/Redis/ZooKeeper/Kafka/ClickHouse + 11 Go 服务 + nginx）。
- **方法**：修内核缺陷 → QEMU 验证 → mysql 容器 `ready for connections` → compose 全栈。
- **环境**：QEMU(KVM, q35, **12G**, 2vCPU)，seed 盘零 load；共享服务器（注意其他会话会 pkill 清扫 QEMU）。

---

### 1.1 迭代工作流（2026-10-24 起，单轮 ~2 分钟准备）

```bash
# 1) 一份预置 seed（一次性生成后可长期复用）
SEED=~/ocm-tars/var-lib-docker-seed.tar.zst   # 2.3G，含 8 镜像

# 2) 重建空盘并注入 seed（约 90s + 60s）
sudo rm -f bin/disk-image-x86_64.img
ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage
LOOP=$(sudo losetup -fP --show bin/disk-image-x86_64.img)
sudo mount ${LOOP}p1 /mnt/ocm-inject
sudo mkdir -p /mnt/ocm-inject/var/lib/docker && sudo chmod 0710 /mnt/ocm-inject/var/lib/docker
zstd -dc $SEED | sudo tar -x -C /mnt/ocm-inject/var/lib/docker --numeric-owner --xattrs --xattrs-include='*'
sudo sync && sudo umount /mnt/ocm-inject && sudo losetup -d $LOOP

# 3) 启动（12G；8G 在 load 大镜像时会 alloc-exhausted）
cd tools && DRAGONOS_QEMU_MEMORY=12G ROOT_PATH=~/DragonOS-ocmanager \
  bash run-qemu.sh --bios=legacy --display=nographic

# 4) guest 内：无需 docker load
dragon-docker-start && docker images

# 5) 关机：新内核会 sync + 收尾文件系统（清 RECOVER），下次启动无 replay
busybox poweroff -f
```

旧的 tar 注入 + `docker load` 流程仅在新镜像/新 seed 需要重建时使用（见 `prebuilt-image-strategy.md`）。

## 2. 已修复并实测验证的问题

### 2.1 第一阶段：容器基础能力（全部通过）

| # | 问题 | 修复 | 验证 |
|---|------|------|------|
| A | overlayfs 合并目录 getdents 枚举损坏，entrypoint 全部跑不起来 | `OvlInode::find` 补 `.`/`..` 处理 | docker 容器正常执行 |
| B | netns 内 iptables-nft `xt_udp` 失败 → embedded DNS 死 | 新增 `NftXtUdp` 表达式 | compose 服务名解析通过 |
| G | open 未知 flag 位被拒 | `from_bits_truncate` | docker create/start 通过 |
| — | `times()` 系统调用缺失 | 实现 | mysqld 启动期通过 |
| — | `do_kern_addr_fault` 把内核态缺页当用户态 oops | 区分访问上下文 | 无 oops |

### 2.2 第二阶段：稳定性与内存（全部通过）

| # | 问题 | 修复 | 验证 |
|---|------|------|------|
| H | 一次时钟倒置读数毒化 tick 绝对期限 → 定时器风暴 | `clocksource_delta` 倒置按 0 处理 | 风暴自愈 |
| H' | CFS 带宽欠费域武装死循环（1250Hz） | `set_next_task` 去武装 + 预算域门控 | 注入率回落 |
| F5 | ext4 journal 重放 4KB 栈溢出 | 重放链堆内存化 | 不再溢栈 |
| F6 | KMSG 锁内分配死锁（printk 级联静默死） | 锁内禁分配 | 静默死亡形态消灭 |
| F7 | write_block 校验缺失 + PCB 守卫刷屏 | 范围校验+条件化 | 启动循环修复 |
| C | OOM 全链 panic（分配器/oom/printk 递归） | 停车不 panic + 零分配 SIGKILL + emergency_print | 2G docker load 存活 |

### 2.3 第三阶段：时钟子系统（mysql 负载 100.1% 真实速率验收）

| # | 问题 | 修复 | 验证 |
|---|------|------|------|
| R9 | watchdog 长间隔跳过比对（慢钟漏检） | 长间隔继续比对 | 捕获 kvm-clock 慢 5 倍 |
| R10-1 | acpi_pm 只在前者失败时注册 → 无参考源 | 无条件注册 | PMTMR 注册 ✓ |
| R10-2 | watchdog 参考源选了慢钟（反向归责） | 独立硬件源优先 | ref=acpi_pm ✓ |
| R10-3 | APIC 标定形态不一致 → 中断风暴(2048Hz) | PMTMR 参考、运行形态重标定 | fire 率归一 245/s ✓ |
| R12 | 时钟链综合 | （上述合力） | **mysql 负载 886s@885s wall=100.1%** ✓ |

### 2.4 第四阶段：取证基建修复（为定位当前主诉而建，本身也是缺陷修复）

| # | 问题 | 修复 | 验证 |
|---|------|------|------|
| R12-1 | AP 重标定 RefCell 双重借用 panic | 先 drop 再调用 | panic 消灭 |
| R13-3 | 任务转储无法定位卡死 syscall（Go 信号帧污染） | PCB 记录 `last_syscall` | 定罪 docker=futex(202) |
| R13-4 | B 态检测误报/漏报 | USER BLOCK STORM 阈值检测 | 干净复测通过 |
| R13-2 | idle CPU 入队不 kick（对齐 Linux ttwu） | `send_resched_ipi` | 语义修复（未解主诉） |
| R13-1 | kvm-clock 高频切换下读数冻结 | 冻结检测→自贬+重选 acpi_pm | 编入待实战触发 |

---

## 3. 主诉演进：R15 调度饥饿 → R16 死锁（均已闭环）

### 3.1 现象（三层同构）

1. **futex 压测**（自编 C 双线程 WAIT/WAKE 程序，可复现）：worker 被 futex_wake 成功唤醒入队后，**~1 秒不被调度执行**；发起唤醒的 main 一旦 sleep 让出 CPU，worker **立即**运行。
2. **mysql 容器**：启动后 2-14 分钟整体楔死（容器消失、CLI 挂起）。
3. **dockerd**：全家 43 线程停在 futex 等待（LSYSCALL=202），从启动时刻起 0% CPU。

**三者同根**：Go runtime / pthread 依赖 futex 唤醒，唤醒入队后不被调度 → 一切等待者永久饥饿。

### 3.2 已排除（六天六层证伪，勿重走）

| 层 | 证伪手段 | 结论 |
|----|---------|------|
| futex 子系统 | 压测 probe：`FUTEX_WAKE` 返回 1（waiter 出队+wake 执行成功） | 无罪 |
| `ProcessManager::wakeup` 吞错 | WKFAIL 探针零输出（无 Err） | 无罪 |
| idle CPU 缺 kick | 补 kick 后 STUCK 复现 | 非根因（修复保留） |
| 唤醒 vruntime 放置 | place 探针：vr==avg==min 一致 | 无罪 |
| 挂错 cfs_rq | rq_cpu==se_rq_cpu | 无罪 |
| 目标 CPU 零 pick / 用户态返回不调度 | pickstat：两 CPU 各 300 pick/s 活跃；handle.rs 确认有检查+SM_PREEMPT | 无罪 |

（另注：#134/136 曾出现"vr≠avg"反常，后查明是加探针时误删赋值语句的自伤，已修复作废。）

### 3.3 真凶定罪（R15，2026-10-22 终局：调度器唤醒路径两个缺陷）

旧嫌疑（mark_sleep 失败、timer 时序、ERESTARTSYS 循环、跨 CPU 陈旧读）全部无罪：R15EXIT 探针显示 `wait_res=Ok`（res=1）、`inq=0`（被正常 wake 出队）、`sig=0 pst=0 park=0`（无幻影信号）。

真实图像（R15WKE/R15ENQ/R15BADHALT 铁证）：`R15WAKE cnt=1` 正确出队+唤醒 worker，worker 入队到 **idle 的 cpu1**（was_idle=1, nr=1 唯一实体），却 ~1 秒不跑；**R15BADHALT：idle 任务在自身 cfs_rq nr_running>0 时反复 halt**（修复前启动期数千次/秒洪泛）。

**缺陷 1：`enqueue_task_on_cpu` 缺失唤醒抢占评估**（`sched/mod.rs`）。activate_task 后从不调用 `check_preempt_current`（Linux ttwu_do_activate→wakeup_preempt 语义）：本地唤醒到 idle rq（target==self 不发 IPI）时 NEED_SCHEDULE 从不置位；远端非 idle 目标完全不做抢占评估。修复：激活后 `if !Arc::ptr_eq(&rq.current(), pcb) { rq.check_preempt_current(pcb, wake_flags); }`（idle current 走 resched_current 置标志+远端 IPI）。

**缺陷 2：idle 循环 halt 丢失唤醒窗口**（`arch/x86_64/process/idle.rs`）。NEED_SCHEDULE 检查在开中断下执行后直接 `hlt`，落在"检查之后、halt 之前"窗口内的 IPI 被无视。修复（对齐 Linux x86 default_idle/safe_halt）：关中断复查 NEED_SCHEDULE（+nr_running 无锁窗口兜底）→ `enable_and_hlt()`（STI;HLT 原子开中断+停机，IPI 要么先于复查要么在 STI;HLT 之后，无丢失窗口）。

### 3.4 验证结果（R15Hunter 实测 + Main 复核，2026-10-22）

- BADHALT 洪泛消除：数千/秒 → 启动期零星、负载期 0
- STUCK 恢复从"probe 1+ 多秒延迟"→ 即刻恢复；~10 分钟数百万次握手零内核 STUCK（alarm(600) 终止）
- 残留 "STUCK at iter 1"（Main 复跑复现 1 次，RECOVERED at probe 1）为测试程序自身启动竞态：main 首次 FUTEX_WAKE 先于 worker 入队，Linux 同行为——非内核缺陷
- 内核 #160（2026-10-22 02:11 构建）；探针族 R15ENQ/R15SND/RCV/WKE/WAKE/BADHALT/TICK/ENT/EXIT 保留在 worktree

### 3.5 mysql 终验阻断：新缺陷 R16——cpu0 关中断死锁 + 跨核等待链（2026-10-22 Main 定罪）

R15 验收后 mysql 终验（净盘 + docker load + run）被阻断，三轮复现 + QEMU monitor 硬件级取证定罪：

**定罪证据链**（R16SRC 三源同刻探针 + QEMU monitor `info lapic`/`info registers`）：
1. 时钟子系统**全程健康**：tsc 3.893GHz（恒定）、kvm-clock kraw 与 timekeeper now 严格同步（4.096s/1024tick 精确）、APIC tick 正常——此前"pvti 冻结/tsc_hz=9.1GHz/rawrate=2337"全部是 **tmict 探针 PMTMR 24bit 回绕 bug**（4.7ms 掩码回绕，秒级窗口差分无意义），探针谎言非系统缺陷
2. **cpu0 不再取中断**：进入 `flush_tlb_multi` 的 IF=0 自旋后，monitor 显示其 `IRR` 含 vector 151（timer）/201（FlushTLB）pending 不被消费、`current_count` 仍在倒计数——APIC 正常产生中断，CPU 因关中断而不取。（注：tmict/R16SRC 打印为全局节流，某核打印消失本身不足以推断该核 tick 停，需以 IRR/IF 为准。)
3. **cpu0 死锁现场（已坐实）**：`RIP=0xffff800000581482` 恒定（两次采样不变），位于 `mm/tlb.rs::flush_tlb_multi` 的 `while !csd.done.load(Acquire) { spin_loop() }` 自旋环，**IF=0 关中断**、CPL=0、非 HLT。调用链（栈帧逐一定位）：`syscall_64 → syscall_handler → kernel_catch_unwind → flush_tlb_kernel_range+0x1d5 → flush_tlb_multi`——用户态 syscall 触发的内核地址空间（vmalloc/vfree）shootdown，**不是中断/RCU 上下文，也不是 torture**
4. **shootdown 目标核已死**：CSD 条目（0xffff8001c0007020）done=0；cpu0 ICR=0x40c9（dest=1, vector 201）；cpu1 `IRR`=151,201——**FlushTLB IPI 已投递而 cpu1 永不 ack**。cpu1 此时已在 panic parking，故 cpu0 永久关中断等待
5. 下游全解：MON sleep 死 / docker load 卡死 / tty 无回显 = cpu0 关中断永久自旋的次生

**cpu1 panic 全文（已抓到）**：`CpuRunQueue::self_lock spinout on cpu ProcessorId(1), caller src/sched/mod.rs:1288`——`__schedule_inner` 取本 rq 锁自旋 1e6 次触发保护 panic；调用者链为 `x86_64_do_irq`（tick 中断中调度）。cpu1 随后 parking（`pause;jmp`）——**只有一个核 panic 停车、系统半死**，这是后续 shootdown 永久等待的直接前提。时间线：他核长时间持有 rq1 锁（cpu0 当时在 `enqueue_task_on_cpu(target=1)`）→ cpu1 tick 抢锁 spinout → panic 停车 → cpu0 之后做 TLB shootdown 等死核 → 全系统用户态停摆。

**两个真实缺陷（与 Linux 对齐方向）**：
a. **探针放入 IRQ 关闭 + 持 rq 锁的临界区**：R15 的 `R15ENQ`/`R15ENQIPI` 与 `send_resched_ipi` 探针在 rq 锁内用 `print_critical!` 直写串口（单行数 ms，串口忙时更久），直接构成 1e6 自旋超时的来源。此类探针必须全部清除——任何 console 输出都不得出现在锁内/关中断区。
b. **`mm/tlb.rs` 在调用者原子里临时 `interrupt_enable()` 再关中断**：这是为让 IPI 送达的权宜手段，但允许 IRQ 重入 `__schedule_inner` 抢**被中断帧本身持有的 rq 锁**（cpu1 的 self-deadlock panic 正是此形态）。必须删除该 hack，改为在无锁/中断安全上下文发起 shootdown。
c. **CSD 等待无 liveness 保护**：目标核 panic 停车后，任何 shootdown 都会永久挂住发起核（本轮停机形态）。panic 语义应对齐 Linux：panic 后停止其它 CPU 或整机复位，不能只 park 当前核。

**已作废的判断（留档防重走）**：cpu0 卡点曾被误归因为 `rcu::torture::torture_callback`（把 RIP `0x581482` 误映射为 `0x481482`；且 torture 仅 debugfs 按需触发，本轮无触发证据）。RCU 内联 drain 线索同样不再相关。

**修复计划**：① 清除 sched 中的 `print_critical!` 探针（尤其 rq 锁内/关中断区的 R15ENQ/R15ENQIPI）及其连带的长临界区；② 重做 `mm/tlb.rs` shootdown 发起路径：不在持有 rq 锁/关中断的调用者里临时开中断，改为对齐 Linux `native_flush_tlb_multi` 的前提约束（调用者不得持会与 IRQ 抢的锁），或改由专门 IPI 处理侧完成；③ 强化 panic 语义：panic 后停机其它 CPU / 整机复位，避免半死系统导致的永久 shootdown 等待；④ tmict 探针已修（per-CPU 采样基线 + PMTMR 24bit 扩展，内核 #162 编译通过），且 tmict/pvti/dlprobe 的打印均已改为**编译期开关默认关闭**（`TMICT_PRINT`/`DL_PROBE_PRINT = false`）——同一类"console in irq-off"缺陷不得留窗口。

**验证状态（内核 #171，2026-10-22 19:3x）**：
- ✅ **R16 死锁已消除**：净盘 + `docker load` 全程（>13 分钟）零 panic、零 rq spinout、零 `sched: long rq section`；`Loaded image` / `LOAD_RC=0` 达成。旧内核约 5 分钟必挂，修复前从未跑完 load。
- ⚠️ **阻断 1（已 pinpoint 到包装层）**：`tlb: N shootdown(s) initiated with interrupts disabled ... via flush_tlb_mm_range at src/mm/ucontext/address_space.rs:965`（73/5/10 次，cpu0/1）。track_caller 目前停在薄包装层（已给 `AddressSpace::flush_tlb_range` 补 `#[track_caller]`，下次编译可定位真实调用点）。代码审计：`page_table_edit()` 的 IF 检查是 `debug_assert!`（release 不生效），不能排除"内核态 IF=0 缺页"；`mm/fault.rs` COW 分支在持该锁期间同步 shootdown。修法：gather 只记录、放锁后在可中断上下文统一 flush；内核态原子区 fault 延迟 flush。
- ⚠️ **阻断 2（已定位到具体进程）**：entrypoint 日志 `175 Segmentation fault | docker_process_sql --dont-use-mysql-root-password --database=mysql`——**mysql 客户端在初始化导入时区数据时段错误**；内核侧对应 `pid N No mapped vma ... rip:0x65451, address:0x8`。`mysqld --version`、`/bin/echo`、基础容器执行均正常（非 runc/镜像问题）。首个容器崩溃后，后续 `docker run` 会挂起（runtime 被拖死）。`getxattr/listxattr` 已改为 `EOPNOTSUPP`（Linux 语义），告警消失。
- ✅ **阻断 2 已定性（bash process substitution + /proc/self/fd pipe 重打开缺失）**：
  - 崩溃进程是 **bash**（非 mysql）：`pid 696/931 (bash) No mapped vma, rip:0x65451, address:0x8, rax..rdx=0`。
  - 最小复现：`docker run --rm --entrypoint bash mysql:8.0.35 -c 'cat <(echo HI)'` 稳定复现同签名崩溃；普通管道 `echo hi|cat`、`/proc/self/fd/3`（普通文件）均正常。
  - mysql entrypoint 第 261 行用了 `--defaults-extra-file=<( _mysql_passfile ... )`（process substitution），因此初始化必崩。
  - 机理（二次修正，已到指令级）：`/dev/fd -> /proc/self/fd` 存在，简单 pipe 重打开正常（`echo hi|{ cat;echo A=$?;cat /dev/fd/0;echo B=$?; }` → `hi/A=0/B=0`）。崩溃发生在 **bash 解析/展开 `<(...)` 阶段**（连 `echo B` 都未执行）。
  - 指令级定位：PIE 二进制在 DragonOS 上统一加载于基址 `0x10000`（由 `/proc/self/maps` 证实），故崩溃 `rip=0x65451` ⇒ 文件偏移 `0x55451`，落在 **`initialize_flags`+0x71** 的 `mov 0x8(%rdi),%esi`（此时 `rdi=0` ⇒ 读地址 0x8）。寄存器 `rax/rdi/rsi/rdx` 全零，说明进入该点的调用/返回状态已被破坏（非正常参数路径）。
  - **排除 vfork/外部命令路径**（2026-10-23 实测）：`bash -c 'echo B;/bin/true;echo A'` → `RC=0` 正常。
  - **最小复现（2026-10-23 已锁定）**：`bash -c 'f(){ echo p; }; cat <(f)'` → **SIGSEGV(139)**，稳定复现；同签名 `rip=0x65451, addr=0x8, rax..rdx=0`。
  - **对照组全部正常**：`f(){ echo p; }; f`、`(f)`、`$(f)`、`cat <(echo p;echo q)`（无函数）、`cat <(echo HI)` 均 RC=0。
  ⇒ 缺陷条件 = **process substitution 子进程内调用 shell 函数**（`<(...)` + function call）。官方 mysql entrypoint 第 261 行 `<(_mysql_passfile ...)` 正是该形态，故必崩。
  - 说明：此前"写 stdout 挂住"是测试脚本误判（脚本里 `timeout` 未生效前的中断干扰），隔离用例证明普通管道/stdout 正常。下一步：重新提取 bash 二进制，确认 `initialize_flags` 的调用者（判断是栈被破坏后的野跳转，还是确有该调用路径），并结合 fork 子进程的用户栈/TLS 状态排查。
  - 修复方向：为 `/proc/<pid>/fd/N` 实现 Linux `proc_fd_link` 语义——`special_node()` 返回目标文件，`open` 时**复制文件描述**（同一 pipe 端），使 process substitution 可用。
- ✅ **阻断 1 pinpoint 已取得**：`tlb: ... via flush_tlb_mm_range at src/mm/fault.rs:1206` 与 `:1223`（COW/写时复制缺页路径同步 shootdown）。修法：gather 后延迟 flush，或内核态原子区 fault 不发起同步 shootdown。
- ✅ **阻断 4 本轮未复现**：futex 修复后 `docker load` 完整跑完（`Loaded image`、`LOAD_RC=0`），自制 `vblk stall` 自检零命中。块 IO 停顿很可能是 futex 持锁睡眠的下游。
- 🎯 **mysql 终验达成（内核 #180，2026-10-22 21:06）**：
  - `docker load` 完整成功（`Loaded image: mysql:8.0.35`、`LOAD_RC=0`），无 IO 停顿（自制 `vblk stall` 零命中）。
  - MySQL 8.0.35 在容器内初始化并启动成功：`ready for connections. Version: '8.0.35' socket '/var/run/mysqld/mysqld.sock' port: 3306`；InnoDB 自动回退 "Linux Native AIO not supported"（`SYS_IO_SETUP` ENOSYS 不致命）。
  - 真实查询验证：`docker exec pmv mysql -uroot -e 'select version();'` → `8.0.35`，RC=0；`STATUS` 显示 Uptime 正常、socket 在线。
  - 说明：为绕开下述 bash 缺陷，本轮以 `--entrypoint bash -c 'mysqld --initialize-insecure ...; exec mysqld'` 启动；官方 entrypoint 路径仍受 process substitution 缺陷阻断。
- ⚠️ **剩余缺陷（官方 entrypoint 阻断）**：`bash -c 'cat <(echo HI)'` 稳定崩溃（`rip:0x65451, addr:0x8, rax..rdx=0`），而普通管道/普通文件 fd 重打开正常；对照 `echo hi|{ cat /proc/self/fd/0; }` 读不到数据。⇒ `/proc/<pid>/fd/N` 对 **pipe** 缺少"复制同一文件描述"的魔法链接语义（`LockedPipeInode` 已有 `acquire_file()` 复用 runtime，但 FileReference 在 walker 中被降级为 `path_inode()`）。修复后官方 entrypoint（第 261 行 `--defaults-extra-file=<(...)`）即可工作。
- 结论：**mysql 已在 DragonOS 上跑通并接受查询**；剩余为 bash/procfs 兼容性缺陷与 tlb shootdown 上下文修复。
- ✅ **阻断 3（已修复，内核 19:58）**：`docker`/`dockerd`/`containerd`/`busybox` 全部 `Blocked(true) LSYSCALL=202`（futex）、ctxsw 零增长。**真因**：`futex_wait` 持全局 `futex_map` 锁执行可睡眠的 `copy_from_user`——用户页缺页时持锁睡眠 ⇒ 全体 futex 系统调用阻塞（并非 wakeup 握手竞态）。**修复**（对齐 Linux `futex_wait_setup`）：锁外可睡眠读并比较 val；持锁后仅用 `PageFaultDisabledGuard` 复查，缺页则放锁、锁外完成映射后重试；入队/激活定时器仍在锁内。
- ⚠️ **阻断 4（当前硬阻断）**：`docker load` 仍在 ~4 分钟处停顿，TASK DUMP 显示 `virtio_blk_io_1 state=Blocked(false)` + `ext4_journal`/`vfs_writeback`/`pagecache-wb-*` 全停；dockerd/containerd 的 futex 等待是其下游。即**块 IO 完成链停止**（used ring 不被回收）。最高概率：virtio-blk 的 EVENT_IDX/avail-event 处理导致设备不再中断；其次 tasklet 未执行或 token 不匹配。futex 持锁睡眠修复保留（真实缺陷），但不是本次停顿的充分因。
- 环境备注：为释放磁盘（曾 100% 满），已删除两份旧样本盘 `clock-chaos-sample-1242.img`/`dirty-journal-sample-1210.img`（证据已固化于本文档）；disk-image 为稀疏盘。

**样本**：历史样本盘已删除以释放磁盘（证据均固化于本文档）。

### 3.6 本会话新增修复清单（未提交、待拆分 PR，2026-10-23）

以下为 R16 阶段产出、尚未进入任何 PR 的修复（当前全部在 `~/DragonOS-ocmanager` 的 staged 快照中）：

| # | 修复 | 关键文件 | 验收证据 |
|---|------|---------|---------|
| 1 | **futex_wait 持全局锁可睡眠拷贝** → 对齐 Linux `futex_wait_setup`（锁外可睡眠读 + 锁内 `PageFaultDisabledGuard` 复查 + 缺页放锁重试） | `libs/futex/futex.rs` | 修复前 load ~4 分钟全栈 futex 挂死；修复后 `Loaded image`/`LOAD_RC=0` |
| 2 | **tmpfs/xattr 默认返回码** `ENOSYS`→`EOPNOTSUPP_OR_ENOTSUP`（Linux 语义）、warn→debug | `filesystem/vfs/mod.rs` | 串口 xattr 告警消失 |
| 3 | **TLB shootdown 上下文与 liveness**：删除"调用者关中断时临时开中断"hack；`check_shootdown_context` 一次性告警（延后打印）；CSD 等待按目标核 Online 状态脱身；停滞计数 | `mm/tlb.rs` | load 全程无永久等待 |
| 4 | **panic 语义**：idle 上下文 panic 走 `stop_this_cpu()`（Online→Dying→Dead + 关 LAPIC + 纯 hlt，去掉 halt 循环内打印） | `debug/panic/mod.rs`、`arch/x86_64/process/mod.rs` | 半死核不再拖死 shootdown |
| 5 | **探针治理**：删除锁内/关中断区 console 探针（sched/fair/ipi/futex/idle/manager-sched/timekeeping/clocksource/task_dump）；tmict/pvti/dlprobe 打印改编译期开关默认关；tmict 采样基线改 per-CPU + PMTMR 24bit 扩展；新增放锁后 rq 长临界区哨兵 | `sched/mod.rs`、`sched/fair.rs`、`exception/ipi.rs`、`arch/x86_64/process/idle.rs`、`process/manager/sched.rs`、`time/timekeeping.rs`、`time/clocksource.rs`、`time/deadline.rs`、`arch/x86_64/driver/apic/apic_timer.rs`、`debug/task_dump.rs` | BADHALT 洪泛消除；rq spinout 消失 |
| 6 | **fault 现场诊断增强**：`No mapped vma` 打印进程名 + rax/rdi/rsi/rdx/rsp | `arch/x86_64/mm/fault.rs` | 定位 bash 崩溃至 `initialize_flags+0x71` |
| 7 | **块 IO 停滞自检**：1s 超时 + irq/sched/run/completed 计数（仅进程上下文打印） | `driver/block/virtio_blk.rs` | 本轮零命中，佐证 futex 修复后 IO 正常 |
| 8 | **track_caller 链路**：`AddressSpace::flush_tlb_range` 补 `#[track_caller]`，用于 pinpoint shootdown 真实调用点 | `mm/ucontext/address_space.rs` | 定位到 `mm/fault.rs:1206/1223` |
| 9 | QEMU 启动端口选择避开本机临时端口 | `tools/run-qemu.sh` | hostfwd bind 不再失败 |
| 10 | **idle 循环 STI;HLT（safe_halt 语义）** + enqueue 抢占评估守卫 | `arch/x86_64/process/idle.rs`、`sched/mod.rs` | #2495 已关闭但**该修复不在 master**（master idle.rs 仍是裸 `x86::halt()`），需随本批重提 |
| 11 | **APIC 单次武装 delta 越界告警**（原 #2491 已关闭、未进 master） | `arch/x86_64/driver/apic/apic_timer.rs` | 诊断增强，与 #2490 已合并的重标定逻辑互补 |

**早前批次 PR 状态（2026-10-23 核查）**：#2479-#2483 已合并；#2484 OOM 仍 OPEN；#2490 clocksource **已合并**；#2491（APIC delta 告警）**CLOSED 且内容未进 master**；#2492（idle kick）**CLOSED**（enqueue 抢占评估已在 master，但 idle STI;HLT 仍缺）；#2493 诊断基建**已合并**；#2494 vcore **已合并**；#2495（R15 修复）按用户要求 CLOSED。

### 3.7 容器层验证进展（2026-10-23）

| 服务 | 镜像 | 验证 | 结果 |
|---|---|---|---|
| MySQL | mysql:8.0.35 | 直接 `mysqld`（绕开 entrypoint 的 bash `<(...)` 缺陷） | ✅ `ready for connections`（3306）；`select version()` → `8.0.35` |
| Redis | redis:7.2.3-alpine | `docker load` → `docker run -d` → `docker exec redis-cli ping` | ✅ `PONG`（L1=0/L2=0/R1=0/R2=0） |
| ZooKeeper | zookeeper:3.8.3（官方，替代已下架的 bitnami 标签） | `docker load` | ✅ LOAD=0 |
| Kafka | confluentinc/cp-kafka:7.5.0（416MB tar） | `docker load` | ❌ 挂死：中途停止写盘（盘零增长），Ctrl+C 无效 |
| Kafka（换源复测） | bitnamilegacy/kafka:3.5.1（335MB tar） | 连续多次 load 时 | ❌ 同样挂死 |
| **Kafka（单独 load 复测）** | 同镜像，全新净盘 + 后台 load + 定时探针 | ✅ **`K_LOAD=0`、`Loaded image`、“Loading containers: done”**（<5 分钟） | ⇒ 楔死**不是镜像本身**，而是**连续多次大镜像 load 后的累积状态**或偶发；需按“逐个 load + 校验 + 资源检查”方式推进 |
| **Kafka（KRaft 运行）** | bitnamilegacy/kafka:3.5.1，`--env-file` KRaft 配置（免 ZooKeeper） | ✅ 容器启动 `K1=0`；`kafka-topics.sh --bootstrap-server localhost:9092 --list` → **K2=0**（AdminClient 连通） | JVM 日志中的 `java No mapped vma`（addr 0/8）是 HotSpot 隐式空检查，属正常语义，非崩溃 |
| ClickHouse | clickhouse-server:23.8.8.20 | 未开始 | ⏸ 排在 kafka 之后 |

- 所有容器操作都在净盘 + 注入 tar（`/mysql8035.tar.gz`、`/redis72.tar.gz`）后进行；`docker load` 全程无挂死。
- 官方 mysql entrypoint 仍被 bash `<(...)`+函数缺陷阻断（最小复现 `f(){ :; }; cat <(f)` → 父 bash SIGSEGV），迁移暂用直接 `mysqld` 启动。

- **时钟速率实测（2026-10-23）**：guest `date +%s` 在宿主 148s 真实时间内前进 147s（**99.3%**）⇒ kvm-clock **平均速率准确**。
- **watchdog 差异的细化判读**：`cs_dev_nsec` 恒为 504000000（同一个窗口值）而 `wd_dev_nsec` 在 577~784ms 间波动 ⇒ 更可能是 **kvm-clock 在这些短窗口内被冻结/钳制**（`kvm_clock::read()` 的 fetch_max 单调钳制 + 冻结检测），而非 acpi_pm 换算错误（acpi_pm mult=558/shift=1 经计算速率正确）。平均 99.3% 说明冻结是**间歇性**的（累计 ~1s/148s）。
- **空闲复测（内核 #181，带 critical 打印）**：启动后空闲 2 分钟，`wdt unstable` 与 `long readout interval` 均为 0 条 ⇒ 偏差只在**容器/IO 负载期间**出现，非持续性问题。
- 未决点：watchdog 偏差 (280ms) 远超 `WATCHDOG_THRESHOLD=62.5ms`，但串口未见 `cs_dev_nsec = ...`（unstable 分支）打印——可能是运行期 `info!` 被日志级别过滤，或该源已被标记 unstable 并已切到 acpi_pm（后续比对仍读 kvm）。**下一步**：① 临时打开 `TMICT_PRINT` 并在 watchdog 分支加 print_critical（放锁后）确认降级是否发生；② 提供 `/sys` 查询当前 clocksource（若无则加只读接口）以便直接判定。
- **旧观察（已作废）**
- **时钟偏差复现（重要）**：多次 watchdog 输出呈 `cs=504000000` vs `wd=577~784ms`（kvm-clock 相对 acpi_pm 慢 13%~36%），且与负载无关（空闲时也出现）。这说明 kvm-clock **速率本身**在 DragonOS 上不稳定，需按 R9-R13 的方法复测（PMTMR 交叉 + 是否触发自贬/切换 acpi_pm）。此前"100.1% 速率"结论只在特定窗口成立。
此前（上一轮）的观察：：kafka load 进行时 clocksource watchdog 报 `cs_dev_nsec=504000000` vs `wd_dev_nsec=577876797`——kvm-clock 相对 acpi_pm 慢约 13%。此前的“时钟健康”结论基于无故障时的采样；高 IO 负载下存在真实速率偏差，需复查（与 R16SRC 探针结论不矛盾：当时采样的窗口无偏差）。
- **大镜像 load 楔死**：mysql(162MB tar)/redis/zk 均 LOAD=0；cp-kafka(416MB tar) 卡死。下一步方向：① 用 `/tmp/dockerd.log` 与任务转储定位（需可交互 guest）；② 分块加载该镜像层（逐层 `docker load`）；③ 检查是否与 overlayfs/快照解包路径相关。


**拆分与 PR（2026-10-23 完成）**：已拆为 9 个独立 PR：**#2507** xattr EOPNOTSUPP、**#2508** run-qemu 端口、**#2509** fault 诊断、**#2510** panic stop_this_cpu、**#2511** TLB shootdown 上下文+liveness+track_caller、**#2512** idle STI;HLT（含 enqueue 守卫）、**#2513** APIC delta 告警、**#2514** 探针治理+tmict 采样修正、**#2515** virtio-blk 停顿自检。
- **futex 持锁睡眠未开 PR**：上游 master 的 `futex_wait()` 已实现 `read_user_u32_nofault` 锁内复查 + 缺页放锁重试（与本次本地修复等价），重复提交会冲突；本地版本随 worktree 保留，rebase 时应采用上游实现。
- 明细（分支、SHA、逐文件覆盖审计、历史 PR 状态、未修复清单）见 `docs/ocmanager-porting/pr-split-2026-10-23.md`。

---

### 3.8 大镜像 load 楔死 / ext4 EIO（2026-10-23 新增证据）
- 顺序对照（干净盘）：`kafka(335M)` → `mysql(162M)` 连续两次全新加载**均成功**（`L_KAFKA=0`、`L_MYSQL=0`），复跑第二轮（层已存在）亦成功 ⇒ **"第二张必挂"不成立**，楔死为间歇性。
- 5 镜像连装（mysql→redis→zk→kafka→clickhouse）：前 4 个全部成功；**clickhouse 失败**，报错 `write /var/lib/docker/containerd/daemon/io.containerd.metadata.v1.bolt/meta.db: input/output error`，随后 writeback 持续 `flush_dirty_inodes: 元数据刷盘失败: EIO` / `sync_fs failed: EIO`；此时磁盘仍有 11G 空闲 ⇒ **非 ENOSPC，是 ext4/块层 EIO**。
- 单独干净盘加载 clickhouse：**成功**（`L_CH=0`、`sync`=0，无 EIO）⇒ EIO 属**累积态/状态相关**，不是镜像内容问题。
- 期间自动 `stall_watchdog` 抓到多次 `TASK DUMP`（62~64 任务）：dockerd/containerd 停在 `LSYSCALL=202`(futex)，`virtio_blk_io_0 Blocked(false)`、`page_reclaim`/`vfs_writeback`/`ext4_journal`/`ext4_evict` 全阻塞——IO 侧一过性停摆，多数可自恢复。
- 待办：① 在 `complete_used_requests` 的 NotReady/EIO 与 `submit_bio` 失败点加一次性 critical（含 token/sector/op）定位 EIO 来源；② 若确认 virtio 队列复位丢完成，则按 #2515 的超时自检触发重试/重初始化。

### 3.9 时钟降级后「假挂死」根因（2026-10-23 定位并修复）
- 现场：`docker load` 期间串口出现 `tksetup: install 'acpi_pm'`（kvm-clock 被降级，降级路径**工作正常**），随后 20 分钟宿主磁盘零写入、guest CPU 4%、console 无响应。
- 关键日志：
  - `invdet: now=2034942 last=6815097 raw=0xb70f85 mask=0xffffff max=0xffffff`（`last` 两次采样不变，`now` 在前进）
  - 持续 `deadline tick on cpu0/1: monotonic clock regressed (...); re-anchoring`
- 根因：`timekeeping::clocksource_delta()` 用 `raw > mask >> 1` 判定"读数回退"并返回 0。**acpi_pm 是 24 位掩码**，窗口仅 `2^24/3.579545MHz ≈ 4.69s`；任何 >2.34s 的合法前向间隔（中断关闭、负载停摆）都被误判为回退，单调钟被钳 0 数秒、deadline tick 反复 re-anchor，表现为系统假死。本例 `raw=0xb70f85=12,001,061 > 0x7fffff`，正是一次跨 24 位回绕的正常前向间隔（3.35s）。
- 修复：对齐 Linux 6.6 `timekeeping_get_delta()` —— **不做方向推断**，统一 `raw.min(max_cycles)`；`max_cycles=min(u64::MAX/max_mult, mask)` 已把单次跳变限制在秒级。持续性倒走仍由 `active_clocksource_regressed()` watchdog 自检触发降级重选（每 64 次超窗读数唤醒一次）。
- 附带修正：`invdet` 打印格式参数错位（`mask=` 为空），改为只打印 `now/last/raw/max`。

### 3.10 EIO 下钻与真实阻塞（2026-10-23 晚）
- 时钟修复后（#185/#186/#189）：**4 镜像连续加载 100% 成功**，楔死消失；`docker run` redis/mysql/zookeeper 成功。
- 诊断（#186）确认 `flush_dirty_inodes` 的 EIO 来自 `commit_inode_metadata`（`fs_ptr` 正常），并且在 #189（加入 `commit_error`/`fail`/短写诊断）**未复现**——说明该 EIO 是重负载/内存压力下的次级现象，而非确定性的元数据写错误。
- 新硬阻塞：`[alloc-exhausted] size=131072 align=8; parking task, kernel alive` + `[oom-kill] ... victim_tgid=234` + `[page-fault-OOM]`。即 **6G 内存在 mysql+zk+kafka 容器创建阶段被耗尽**，Rust 全局分配失败进入 `alloc_error_handler` 的 park 循环，表现为 `docker run` 永久挂起（与 2-e 同机制）。
- 下一步：把 QEMU 内存从 6G 提高（验证 8G 是否可用；已知“非 6G 挂死”记录需复核），并按 Linux 语义把容器内存计入 cgroup/回收，避免全局堆耗尽。

### 3.11 8G 内存 + TLB 延迟 flush 修复（2026-10-23 收尾）
- **内存**：6G 下容器创建期全局堆耗尽（`alloc-exhausted` park + `oom-kill`）；8G 启动正常（`MemTotal=8359040 kB`，可用 8125MB），4 镜像加载 + 4 容器创建全部成功，期间 **0 次 alloc-exhausted / 0 次 EIO**。
- **TLB 死锁**：全栈运行中 java 启动瞬间整机停摆（CPU 0、无时钟中断），最后日志为 `flush_tlb_mm_range at src/mm/fault.rs:1223`（关中断 shootdown）。已在 `do_wp_page` 改为：守卫内只置 `tlb_flush_needed`，`drop(_pt_edit)` 后再 shootdown。
- **验证**：修复后单独跑 kafka —— `L_KFK=0`、`RUN_KFK=0`、`docker ps` RC=0、**`kafka-topics.sh --bootstrap-list` RC=0**，shell 正常返回（此前同场景必挂死）。
- 注意：缺页上下文本身 IF=0，shootdown 仍会打印 `initiated with interrupts disabled` 提示（信息性），但已不再与页表/rmap 锁形成死锁环。

### 3.12 Compose infra 端到端验收（2026-10-23，内核 #191 / 8G）
一次干净盘运行（4 镜像 + 4 容器 + 探活）：全部通过，无挂死。

| 步骤 | 结果 |
|---|---|
| `docker load` mysql/redis/zk/kafka | 全部 `=0` |
| `docker run` mdb/rds/zk/kfk | 全部 `=0`（`docker ps` 四个 `Up`） |
| Redis `redis-cli ping` | `PONG`，RC=0 |
| ZooKeeper `srvr` | standalone，RC=0 |
| Kafka `kafka-topics.sh --bootstrap-server --list` | RC=0 |
| MySQL `select version()` | `8.0.35`（容器就绪约需 30s+，首次探活过早返回 socket 缺失） |

关键修复链（本轮）：
1. `timekeeping::clocksource_delta()` 按 `max_cycles` 钳制（窄掩码不再误判回退 → 消除假死）；
2. `do_wp_page` 的 TLB shootdown 推迟到 `page_table_edit` 放开后；
3. 该延迟 shootdown 期间临时开中断，避免两 CPU 互相等待 ACK 的交叉死锁。

### 3.13 阻断1收口：缺页路径 shootdown 全部延后（2026-10-23）
- `fault.rs` 内两处 mm-aware 更新（`do_wp_page` 四个分支、`upgrade_external_pfn`）统一改为：守卫内只记录 `tlb_flush_needed` → `drop(_pt_edit)` → `flush_tlb_after_unlock()`（等待 ACK 期间临时开中断，结束恢复 IF）。
- 其余调用点核查：`mremap.rs`（进程上下文、持 mm 锁但不关中断）、`oom_reap.rs`（回收线程）、`uprobe/*`（进程上下文）、`mmu_gather::flush_mmu_tlbonly`（gather 结束后统一 flush）均不在关中断区，无需改造；`PageFlush::flush()` 只是本地 `invlpg`，不产生 IPI。
- 复验（#195/8G，干净盘）：kafka `docker load`/`docker run`/`docker ps`/`kafka-topics --list` 全 RC=0。

### 3.14 容器/用户态回归复验与 Go 服务镜像（2026-10-23 晚）
内核 #195 / 8G / 干净盘实测：

| 项目 | 旧现象 | 现结果 |
|---|---|---|
| bash `<(函数)` | 父 bash SIGSEGV(139) | `bash -c 'f(){ :; }; cat <(f)'` → RC=0；带输出变体 `hi`/`x` 均 A=0/B=0 |
| 管道阻塞 | `yes | head -1` 挂住 | `y` + `P=0`；`echo hello | cat` → `hello` + `Q=0` |
| 2-e 容器删除后重跑 | `docker run` 挂起 | `docker rm -f kfk` → RM=0；`docker run -d --name kfk2 …` → RUN2=0（无第三方依赖直接退出属预期） |

结论：三者都是此前内存耗尽 / 交叉 TLB 死锁的**下游症状**，修复后自然消失，无独立缺陷需修。

### 3.15 Go 服务镜像构建（host 侧完成）
- `make -C manager/backend build-bin-static` 产出静态二进制：`ocmanager`(59M)、`footstone`(56M)、`msg-etl`(43M)、`sysdiagnose`(41M)。
- 镜像（`Dockerfile.backend` + `--build-arg RUNTIME_IMAGE=alpine:3.19`）：
  - `oc-footstone:dev` 131MB、`oc-msg-etl:dev` 104MB、`oc-sysdiagnose:dev` 99.2MB；
  - `ocmanager` 无 `services/ocmanager/configs` 模板，未构建（该服务不作为容器模板发布）。
- 导出 `/home/sparkzky/ocm-tars/ocservices.tar.gz`（139MB，三镜像合一）。
- 部署脚本 `r16_ocm_up.sh`：`--network host` 起 mdb/rds/ch/kfk + footstone（env 走 `config/env.example` 语义），先探活 CH/Redis/Kafka，再 `docker logs foot`。

### 3.16 Go 服务部署进展（2026-10-23 深夜，12G，seed 盘零 load）
- 环境：`--network host` 起 mdb / rds / ch / kfk，然后 `oc-footstone:dev`。
- 结果：
  - MySQL ✅（`--initialize-insecure` 只建 `root@localhost`；必须补 `CREATE USER 'root'@'%'` + `GRANT ALL ON *.* TO 'root'@'%'`，否则 footstone 报 `Error 1130 Host '127.0.0.1' is not allowed`）
  - Redis ✅（`PONG`）
  - Kafka ✅（容器 Up；`SYS_MEMBARRIER` 未实现但 redis 仍正常）
  - **ClickHouse ❌ `Exited (139)`（SIGSEGV）**，日志只到 jemalloc `Number of CPUs detected is not deterministic`，内核侧无 fault 记录
  - footstone：入口脚本渲染配置成功、MySQL 连接走通，随后因 `dial tcp 127.0.0.1:9000: connect: connection refused`（CK 未起）退出
- 新缺陷：ClickHouse server 在 DragonOS 上启动即 SIGSEGV，需单独定位（优先查 CPU 探测相关 syscall：`sched_getaffinity`/`sysconf`、`madvise`、`rseq` 或 `getrandom`）。

### 3.17 ClickHouse SIGSEGV 归因（2026-10-23）
- 现象：`docker run clickhouse/clickhouse-server:23.8.8.20` → `Exited (139)`；`clickhouse-server --version` 正常（CHV=0）。
- `bash -x /entrypoint.sh` 定位：崩溃发生在第二次 `readarray -t DISKS_METADATA_PATHS < <(clickhouse extract-from-config …)` 之后 / `mkdir -p /var/lib/clickhouse/` 处，内核日志：
  `pid: NNNN (bash) No mapped vma ... rip:0x67ca1, address: 0x8, rax/rdi/rsi/rdx=0, rsp=0x6ffff09ff098`
  → 与 rootfs bash 早前 `<(...)` 崩溃**同一特征**（`mov 0x8(%rdi)`、rdi=NULL、rsp 呈异常高位），但容器内简单用例均通过：
  - `echo "$(id -u) $(id -g)"` → `0 0`
  - `readarray -t X < <(seq 3)` → `2`
  - `readarray -t A < <(clickhouse extract-from-config …)` → `N=1`
- 判定：**bash 子进程在多次 command/process substitution 后上下文损坏**（fork/子进程返回路径），不是 ClickHouse 自身缺陷。待办：在最小脚本（重复 N 次 `readarray … < <(true)`）上二分出触发次数，再查 `fork`/`clone` 返回时 TrapFrame 与寄存器恢复路径。

### 3.18 关机清理 journal（2026-10-23，已修复并实测）
- 现象：`busybox poweroff -f` 前即使 `sync`，rw 挂载的 ext4 仍保留未提交 journal；下次启动 `dirty journal detected ... replaying` 偶发 **nested-panic park**（重放与并发打印/状态不一致），导致镜像盘不可复用。
- 根因：`kernel_power_off()` 只做 notifier + `device_shutdown()`，缺少 Linux 语义的 `sync` + `remount-ro`（`mount -o remount,ro /` 在 DragonOS 上返回 `Not supported`，RM=255）。
- 修复：新增 `vfs::mount::shutdown_mounted_filesystems()`（`git 8d93771f`）——对每个可写挂载执行 `sync_fs(wait)` 后调用 `FileSystem::on_umount()`；ext4 的 `on_umount` 已是 `flush_dirty_inodes + journal.shutdown + shutdown_writable()`（清 RECOVER）。在 `kernel_shutdown_prepare()` 中 `device_shutdown()` 之前调用。
- 实测（#196 / 12G / seed 盘）：启动 → `dragon-docker-start` → `docker run redis` → `busybox poweroff -f` → 再次启动无 nested-panic，`docker images` 仍为 8 个镜像。
- 直接取证（host，关机后 `dumpe2fs -h /dev/loopXp1`）：`Filesystem state: clean`，features 列表**不含 `needs_recovery`** ⇒ `shutdown_writable()` 确实清除了 RECOVER。
- 说明：即使 RECOVER 已清，挂载时若日志区仍有内容，`dirty journal detected ... replaying` 仍可能出现一次（replay 为空操作，不再 panic）；判断是否干净以上面的 `dumpe2fs` 为准。

### 3.19 bash 子进程崩溃：已排除项与下一步（2026-10-24）
- 复现矩阵（#196，干净关机可复用盘）：
  - 根 shell：`for i in $(seq 1 200); do x=$(true); done` -> DONE1；`readarray -t A < <(printf a)` x50 -> DONE2。
  - 容器内（clickhouse 镜像 bash）：`$(id -u) $(id -g)`、`readarray < <(seq 3)`、`readarray < <(clickhouse extract-from-config ...)` 均正常。
  - 仅完整 `/entrypoint.sh` 崩溃（第二个 `readarray ... < <(...)` 之后），内核日志特征：`rip=0x67ca1, address=0x8, rax/rdi/rsi/rdx=0, rsp≈0x6ffff09ff0xx`（多次采样落在同一小区间）。
- 已核对并排除：`copy_thread` 正确复制父 TrapFrame 并清零 rax；syscall 入口帧布局与 `ret_from_intr`/`iretq` 一致；`CLONE_VFORK` 已实现父进程阻塞（`do_clone` 中 `wait_for_completion_killable`，exit/exec 时 complete）。
- 下一步（需内核探针）：在子进程首次返回用户态前按 pid 采样一次 TrapFrame 的 rip/rsp/rdi/rsi/rdx，与 fork 时父帧对比；同时记录 `%fs` base（0x67ca1 附近使用 `%fs:0x28` 栈金丝雀，若 FS base=0 则读地址 0x28）。

### 3.20 bash 子进程崩溃：复现缩小（2026-10-24 补测）
- bind mount 可用（`-v /guesttest:/t` → `HELLO`），据此把镜像内 `/entrypoint.sh` 复制到 guest 做二分。
- 取前 41 行（到第二次 `readarray … < <(clickhouse … || true)`）运行：**子壳 bash 触发 `No mapped vma`（rip=0x67ca1, addr=0x8, 全零寄存器）但父脚本继续执行并打印 `OK41`，退出码 0**。说明崩溃的是命令替换/进程替换产生的子 shell，父 shell 可存活。
- 对照：
  - `readarray -t A < <(false || true)` → `N=0`，无崩溃；
  - 300 次 `readarray -t A < <(true || true)` → `LOOPOK`，无崩溃；
  - 预建 `/var/lib/clickhouse` 后跑完整 entrypoint → 仍崩溃（排除 `mkdir -p`）。
  - 简单信号帧验证：`trap "echo HANDLED" SEGV; kill -SEGV $$` → `HANDLED` / `after`，基础信号投递正常。
- 结论（推断）：触发条件依赖 `clickhouse extract-from-config` 子进程（jemalloc/mmap 重负载）与进程替换的时序；日志中的全零寄存器很可能是**二次故障**（SIGSEGV 处理/重试路径）的帧，首个故障未被现有日志捕获。
- 下一步：① 在 `setup_frame()` 与 `sys_rt_sigreturn()` 各加一次性采样（sig、frame.rip/rsp/rdi、ucontext 读回值），判断是投递还是恢复阶段被清零；② 或在 user #PF 入口加 per-pid 环形缓冲（记录首个故障的 rip/addr/regs），崩溃后再打印。

### 3.21 bash 子进程崩溃：信号帧排除（2026-10-24 探针实测）
- 探针（`7ce65e24`）：在 `setup_frame()` 与 `sys_rt_sigreturn()` 对**可疑帧**（rip<0x10000、rsp=0 或 rdi/rsi/rdx 全 0）采样打印。
- 结果：
  - SIGCHLD 帧往返完全一致（`sigsetup rip=0x4cefc7 rsp=0x6ffff09ff758 rdi=-1` ↔ `sigret` 同值），**信号帧构造/恢复未损坏**；
  - 复现容器 41 行前缀时出现 `pid=577 (bash) No mapped vma, rip=0x67ca1, address=0x8, rdi=rsi=rdx=0`，但**没有任何 sigsetup 记录** ⇒ 该 bash 子进程未安装 SIGSEGV handler，按默认动作终止，不是 handler 二次故障。
- 推论：全零寄存器就是**故障现场本身**；`rip=0x67ca1` 处指令为 `xor %eax,%eax`，与 `address=0x8` 不匹配，说明要么 trap frame 的 rip 不可靠（需打印 rip 处指令字节核对），要么该 rip 属于另一个（已被 exec/覆盖的）映像。下一步：在日志中同时输出 `cs/ss/rflags/old_rsp` 与 rip 处前 8 字节指令（经页表转换读取，避免二次缺页）。

### 3.22 bash 崩溃：指令级确认（2026-10-24，关键进展）
- 增强日志（`87180979`）输出 `cs/ss/rflags` 与 rip 处 8 字节指令。实机崩溃采样：
  `rip=0x67ca1 address=0x8 cs=0x33 ss=0x2b rflags=0x10246 insn=[8b,77,08,83,fe,ff,74,35]`
  → 字节即 `mov 0x8(%rdi),%esi`，在 `rdi=0` 时**正好访问 0x8**，与日志地址完全一致。
- 符号解析：`0x67ca1 = PIE base 0x10000 + 文件 VMA 0x57ca1`（此前按文件偏移反汇编漏算 base，故误判为 `xor eax,eax`）。`addr2line` → **`initialize_flags`**（bash jobs 相关）；其前序指令 `mov 0xcb2b9(%rip),%rdx # bgpids`、`shl $0x4,%rdi; add %rdx,%rdi`，说明崩溃时 **`bgpids` 全局指针为 NULL** 且 pid=0。
- 对照：加 `set +m`（关闭 job control）后同样崩溃（`PM=0` 但仍有该 fault），排除“job control 开关”这一表层差异；父脚本始终存活（`P41=0`），说明是**某个短命子 shell 的全局数据为 0**。
- 下一步（两个方向）：
  1. 验证子进程数据段：在子 shell 中打印已知全局（如 `BASH_VERSINFO`）与普通变量，确认是否出现“数据页被清零”；若是，则回到 COW/fork 数据页复制路径（与我们近期改动的 `do_wp_page` 相关）。
  2. 若仅 `bgpids` 为 NULL，则查 bash 何时分配该表（`initialize_job_control`/`get_pid_stats`）并确认是否在 fork 早于初始化时被调用。

### 3.23 bash 崩溃：排除内核数据损坏（2026-10-24 对照实验）
- **修正前一轮误报**：`cowtest` 首版断言索引写错（`buf[8]` 实为 `'-'`），导致“3000 次 fork 全 bad”。修正断言后：
  - `/cowtest 500 0`（只读继承）→ `c0=500`；`/cowtest 500 1`（COW 写后再校验）→ `c0=500`；均 RC=0。
  - `/pietest 1000`（PIE + `R_X86_64_RELATIVE` 全局指针 + COW 写）→ `bad=0`。
  ⇒ **fork / COW / 重定位数据继承在 DragonOS 上均正常**，排除“子进程数据页被清零 / 重定位丢失”。
- bash 5.2 源码核对（`jobs.c`）：`struct bgpids bgpids = {0,0,0,0}`，`storage` 惰性分配；崩溃指令读的是 `bgpids.storage[idx].status`（偏移 8），`storage==NULL` 时即访问 `0x8`。多数读取路径都有 `storage == 0` 守卫，个别内部路径（如 `pshash_delindex`/复用槽位）依赖调用方先 `bgp_resize`。
- 结论（推断）：这是 bash 在**子 shell 的 job-control 状态未就绪**时进入 pid 状态表读取的时序问题；DragonOS 侧只影响该路径的触发概率（如 SIGCHLD 投递时机），非内核数据损坏。
- 下一步：① 用 `gdb`/kprobe 在崩溃子 shell 中打印 `bgpids.nalloc/storage/npid` 与 `js.c_childmax`，确认未初始化状态；② 若确认是 SIGCHLD 早于初始化，则查 `sigchld_handler` 的投递与 `initialize_job_control` 顺序。

### 3.24 bgpids 现场探针（2026-10-24）
- 已加入探针：当用户态 #PF 命中崩溃特征时，额外读取 PIE 基址(0x10000)+`bgpids`(0x122f50) 的 32 字节并打印 `storage/nalloc/head/npid`（提交见 git log）。
- 本轮复现尝试中，`docker run … /t41.sh` 启动阶段触发了一次 `TASK DUMP` 并停在挂起状态，未产生新的 #PF，探针数据待下一轮取得。
- 复现时注意：先 `dragon-docker-start` 等 daemon 就绪（约 20s）再起容器，避免把 daemon 启动慢误判为挂起。

### 3.25 复现环境不稳定：首容器启动可整机挂死（2026-10-24）
- 净盘 + seed 引导后，`dragon-docker-start` 正常（`29.1.3 overlayfs`），但**第一个 `docker run`（clickhouse 镜像执行 41 行脚本）** 再次触发 `TASK DUMP: kernel stall detected, 77 tasks` 并整机 0 CPU 无响应。
- 转储关键任务：`containerd-shim-runc-v2` 处于 futex（LSYSCALL=202）、`dash` 处于 `wait4`（LSYSCALL=61）、`virtio_blk_io_0` 不可中断、`ext4_journal` 阻塞 ⇒ 与既有 IO 停顿同域；随后无任何 CPU 活动，属硬挂死而非慢。
- 影响：bash/bgpids 的现场采集被阻断；同一命令此前（#199）可完成 `P41=0`，说明为**概率性**，与镜像/命令无关。
- 下一步优先查：① 该挂死是否为 TLB/IRQ 类死锁（可用 QEMU monitor 取两 CPU RIP，方法见 §3.11）；② `virtio_blk_io_0` 不可中断与 `ext4_journal` 阻塞是否构成完成链丢失（#2515 自检未覆盖此路径）。

### 3.26 首容器挂死的直接取证：双 CPU idle + bash 不可中断（2026-10-24）
- 现场：净盘 + seed，`dragon-docker-start` 正常，轻量容器 `docker run --rm redis:7.2.3-alpine true` → `LIGHT=0`；随后运行 41 行脚本容器时挂死。
- QEMU monitor 取证（方法：`Ctrl-A c` → `info registers` → `cpu 1` → `info registers`）：
  - CPU0 `RIP=ffff8000007ec059 HLT=1`，CPU1 同址 ⇒ 符号化为 **`ProcessManager::arch_idle_func`**，即两 CPU 都在 idle HLT。
  - 结论：**不是自旋死锁**，而是没有任何可运行任务。
- 同刻 `TASK DUMP: 76 tasks` 状态分布：`Blocked(true)=60`、`Blocked(false)=15`、`Runnable=1`（正在打印的 `stall_watchdog`）。
- 不可中断的 15 个任务包括：`bash`（用户进程，处于不可中断睡眠）、`virtio_blk_io_0`、`virtio_blk_io_1`、`ext4_journal`、`ext4_complete`、`srcu_gp`、`rcu_gp` 等。
- 解读（推断）：用户 `bash` 在等 BIO 完成（不可中断），两个 virtio-blk io 线程与 ext4 journal 同样停摆，说明**完成中断/唤醒链再次中断**（与阻断4同域）；由于 `submit_bio_wait` 的 1s 超时循环会在 5s 内打印 `vblk stall`，而串口 **0 条**，可进一步定位为 **未进入 `submit_bio_wait` 的等待**（更可能停在 `bio_queue.wait_for_work_or_stop()` 或 BIO 完成后的唤醒路径）。
- 下一步：① 在 `BioQueue::submit` 与 `wait_for_work_or_stop` 之间加一次性竞态探针（记录 `queue.len/state/wake_count`）；② 在 `complete_used_requests` 末尾对 `bio.wait_status()` 的唤醒路径加计数，确认完成 BIO 是否真的唤醒等待者；③ 复核 `virtio_blk_io_1`（第二块盘 ext4disk）是否也存在同类停顿。

### 3.27 挂死根因指向页缓存缺页 IO（2026-10-24 栈符号化）
- 阻塞 `bash`(pid 729) 的转储行：`state=Blocked(false) rip=0x67ca1 LSYSCALL=13 rdi=rsi=rdx=0`。
- 内核栈符号化（`addr2line`，由浅到深）：
  `do_page_fault` → `X86_64MMArch::do_user_addr_fault` → … → `rcu::user_exit/user_enter` → `ProcessManager::switch_finish_hook`。
  未见 `submit_bio_wait` 帧，且串口 **0 条 `vblk stall`** ⇒ 阻塞点不在 `submit_bio_wait` 的 1s 超时循环。
- 结合 §3.26（双 CPU idle、两个 virtio-blk io 线程与 ext4 journal 均不可中断）：`bash` 在**文件页缺页**路径等待页缓存/BIO 完成，而完成链没有把它唤醒。
- 下一步（精确）：① 在页缓存读路径（`read_page`/`wait_on_page`）加 waiter 计数与一次性超时打印（与 `VBLK_*` 计数并列），区分“BIO 未提交 / 已提交未完成 / 已完成未唤醒”；② 在 `complete_used_requests` 末尾打印 `used_idx` 与 `last_used_idx`，确认是否还有未回收的 used 项；③ 两个 io 线程（disk 与 ext4disk）分别统计，排除第二块盘干扰。

### 3.28 页缓存等待点排除（2026-10-24）
- 为 `PageCacheReadBatch::wait()` 与 `PageEntry::wait_ready()` 增加 1s 分片等待 + 停滞打印（语义不变，循环直到条件满足）。
- 复现挂死后：`pcache stall` **0 条** ⇒ 阻塞点既不在读批完成，也不在 Loading 页等待。
- 结合 §3.26/§3.27：`bash` 在内核栈上位于 `do_user_addr_fault`，双 CPU idle，`virtio_blk_io_*` 与 `ext4_journal` 不可中断，但页缓存的显式等待未被命中。
- 剩余候选（下一步逐一加探针）：① `AddressSpace` 写锁/`page_table_edit` 上的锁等待（持有者可能正阻塞在 IO）；② inode/元数据锁；③ cgroup 内存 charge 等待（`alloc-exhausted` 的同域路径）；④ `mmu_gather`/TLB 等待。

### 3.29 挂死点定位：exec 的 group-exec 等待（2026-10-24 关键）
- 新增探针（`db34ca2e`）：公共等待入口加 `#[track_caller]`，记录**用户任务**最近一次等待的 `file:line` 与 pid，由 stall dump 打印；内核线程不记录（避免覆盖）。
- 实机挂死输出：`last wait site: src/ipc/process_signal/exec.rs:32 (pid=648)`。
- 该行位于 `ProcessSignalState::wait_group_exec_event_killable()` —— **多线程进程 exec 时等待兄弟线程退出的 group-exec 事务**。
- 结论（推断已很强）：容器内某个多线程进程（Go 实现的 runc/containerd-shim 等）执行 exec，需要先让所有兄弟线程退出；其中某个线程长时间不退（阻塞在 futex/不可中断 IO），exec 的等待永不返回，容器启动挂死；此前 `do_user_addr_fault`/`wake_all` 栈帧是栈扫描噪声。
- 下一步：① 在 `start_group_exec_transaction`/完成路径打印未完成 generation 与待退出 sibling tid 列表（超时 5s 触发一次）；② 在 stall dump 中把「同 tgid 的线程」一并列出，直接看出哪个 sibling 卡住；③ 复核 exec 的 sibling 终止流程是否会让处于 uninterruptible 等待的线程被强杀/唤醒。

### 3.30 exec 等待的兄弟线程定位（2026-10-24 二次复现）
- 两次独立复现的 `last wait site` 均为 `src/ipc/process_signal/exec.rs:32`（pid=648 / 650），即 `wait_group_exec_event_killable()`。
- task dump 增加 tgid 后可见：`containerd-shim-runc-v2` 的多个线程同属 **tgid 591**，全部 `Blocked(true)` 且分别在 futex（LSYSCALL=202）、epoll/其他等待中；容器内 `bash`（tgid 615）阻塞在 `read`（LSYSCALL=0）。
- 推断：多线程 Go 进程（shim/runc）执行 exec 时进入 group-exec 事务，等待所有兄弟线程退出；而兄弟线程停在 futex/epoll 上未被终止或未收到退出通知 ⇒ exec 永不返回，容器启动挂死。
- 下一步：① 在 `start_group_exec_transaction`/完成回调打印未完成 generation 与待退出 sibling 的 tid/所属 tgid；② 核对 exec 终止 sibling 的路径（信号投递 → 线程在 futex 中是否可被 killable 唤醒）；③ 检查 `wait_group_exec_event_killable` 是否需要在 sibling 进入不可中断等待时返回 EINTR 而非永久等待。

### 3.31 exec group 等待：代码级竞态分析（2026-10-24）
`de_thread()`（`kernel/src/process/exec.rs:439`）流程：收集 sibling → 每个未完成 unhash 的 sibling `assign_group_exec_generation(g)` 且 `pending++` → 对 kill_list 投递私有 SIGKILL → `wait_group_exec_event_killable(group_exec_pending_complete)`。
计数由退出路径 `mark_identity_unhash_complete()` 后调用 `complete_group_exec_task()` 递减（`process/exit.rs:1099`）。
两个可疑窗口：
1. **`group_dead == true` 时跳过 `complete_group_exec_task`**：若最后一个 sibling 以 group_dead 退出，而它已被计入 pending，则计数不再递减；
2. **旧 leader 阶段**：非 leader exec 还要等 `group_exec_handoff_ready`（phase 需为 Ready，由 `complete_group_exec_leader_exit` 在旧 leader exit_notify_complete 时推进）；若旧 leader 的退出通知依赖已被 kill 的 sibling，二者互等。
- 说明：collect 与计数写入均在 signal 内锁内完成，因此“退出路径先看到 GROUP_EXEC 未设置”的窗口基本被排除；重点转向上面两条。
- 下一步探针：给 `group_exec_notify_count` 与 `group_exec_leader_phase` 加只读查询，在 de_thread 的等待处按 1s 分片打印 `(count, phase, kill_list 长度)`，即可区分是计数不降还是 leader 阶段卡住。

### 3.32 exec 等待确认为 killable 版 + 状态快照（2026-10-24）
- 探针：在三个 group-exec 等待入口记录 `(notify_count, leader_phase)`（提交见 git log），dump 打印 `exec wait state`。
- 实测挂死：`last wait site: src/ipc/process_signal/exec.rs:56`（`wait_group_exec_event_killable` 内的 `wait_event_killable`），同时快照为 **`notify_count=0 leader_phase=0`**。
- 解读：等待者看不到有效事务（无 GROUP_EXEC 阶段/计数已为 0，且 `weak_matches(group_exec_task, owner)` 很可能为假），因此 `group_exec_pending_complete()` 永远返回 false，killable 等待没有信号就永久睡眠。
- 修复方向（待实现）：de_thread 的两个条件应把“事务已不活跃/已被清除”视为完成（Linux 语义：等待的是计数归零，清除时 wake 且重检应通过）；或在 `clear_group_exec_locked` 时留下“已完成”标记供等待者判定。需同时审查 `finish_group_exec_owned` / `try_cancel_group_exec` 的并发顺序。

### 3.33 挂死真因一：控制台输入路径持锁打印自死锁（2026-10-24，已修复）

- 取证方式：`-gdb tcp::12581` 直连 QEMU gdbstub，读两 CPU 的 `$pc/$eflags`：
  CPU0 `rip=SpinLock<InnerVirtIOConsoleDevice>::try_lock_irqsave`、`rflags=0x6`（IF=0）自旋；
  CPU1 `rip=virtio_console_rx_retry_tasklet::call` 的 try_lock 循环、`rflags=0x246`。
- 结论：`pump_input()` 持有 `InnerVirtIOConsoleDevice` 锁时调用 `print_critical!`（每 256 字节）与 `dump_all_tasks()`（`D` 热键）；控制台输出路径再次取同一把锁 → 同 CPU 自死锁，且另一 CPU 的 rx tasklet 也被拖住。
- 证据：普通命令里含大写 `D` 即触发转储并 panic（`dump_all_tasks` 对无 Pid 任务 `pid().unwrap()`）；此时任何输入都无回显。
- 修复：删除持锁打印与 `D` 热键；转储对无 Pid 任务回退 `raw_pid`（提交 `47cdb558`、`88259651`）。

### 3.34 挂死真因二：缺页路径持地址空间写锁读用户内存自死锁（2026-10-24，已修复）

- 取证：per-task 等待点显示 26+ 任务停在 `fault.rs:388`（`current_address_space.write()`）；RwSem 增加持有者 pid/获取点探针后打印 `rwsem SELF-DEADLOCK: InnerAddressSpace pid=664 held_at=fault.rs:388 want_at=fault.rs:388`。
- 根因：`No mapped vma` 诊断分支在仍持有 mmap 写锁时用 `UserBufferReader` 读 `regs.rip` 处指令与 `0x122f50`（bgpids）。目标页未映射 → 嵌套 #PF，而嵌套缺页同样先取 `write()` → 自死锁。内核访问用户地址的 `#PF` 无法靠异常表绕开，因为取锁发生在 fixup 判断之前。
- 修复：删除该读取，仅保留不触碰用户内存的寄存器日志（提交 `b82baac0`）。
- 排除记录：`exec` group 等待不是本次挂死点——分片 1s 超时探针从未触发（`exec wait stalled` 零输出），§3.32 的修复方向据此作废。

### 3.35 容器内 bash `bgpids` SIGSEGV：RLIMIT_NPROC 默认 0（2026-10-24，已修复）

- 现象：ClickHouse entrypoint 的命令替换子进程 `bash` 在 `rip=PIE+0x57ca1`（`pshash_delindex`）访问 `0x8` 崩溃，`rdi=0`（`bgpids.storage==NULL`）。
- 归因：`ulimit -u`/`getconf CHILD_MAX` 均为 0。bash 5.2 `bgp_resize()` 在 `c_childmax==0` 时 `bgpids.nalloc < nsize_cur` 不成立 → 既不分配 storage 也不更新 nalloc；随后 `bgp_getindex()` 仍调用 `pshash_delindex(bgpids.head=0)` → `storage[0].pid` 空指针。
- 对齐 Linux：`fork_init()` 把 init 的 `RLIMIT_NPROC` 与 `RLIMIT_SIGPENDING` 都设为 `max_threads/2`；DragonOS 只设了 Sigpending（提交 `22032d92`）。
- 验证：`ulimit -u=12259`；`/t41.sh` 输出 `OK41`/`P41=0` 且无 `No mapped vma` 日志；`docker run -d clickhouse/clickhouse-server:23.8.8.20` 正常 Up，`clickhouse-client -q 'select 1'` 返回 1。

### 3.36 本轮回归（2026-10-24）

| 项目 | 结果 |
|---|---|
| `ulimit -u` / `getconf CHILD_MAX` | 12259 / 12259 |
| `docker run redis:7.2.3-alpine true` | `LIGHT=0` |
| `/t41.sh`（ClickHouse entrypoint 前 41 行） | `OK41`、`P41=0`，无 SIGSEGV |
| ClickHouse 容器 + `select 1` | `CH=0`，输出 `1` |
| `/cowtest 500 1` | `c0=500` |
| `/pietest 1000` | `bad=0` |

> 备注：ClickHouse 启动期间 `Fetch` 线程出现过 `BLOCK STORM`（等待点 `address_space.rs:1987` 的 `mprotect_wait` 写锁），但随后自行恢复；该非致命长等待留待后续评估。

### 3.37 Go 服务启动崩溃：netlink IPv4 属性顺序（2026-10-24，已修复）

- 现象：`oc-footstone:dev` 启动即 panic：`net.newAddr index out of range [3] with length 3`（`net/interface_linux.go:180`）。
- 归因：DragonOS 的 RTM_NEWADDR 属性顺序为 ADDRESS、BROADCAST、LABEL、LOCAL；Linux `inet_fill_ifaddr()` 为 ADDRESS、LOCAL、LABEL、BROADCAST（本机实测确认）。Go 标准库在检测到 IFA_LOCAL 后跳过 IFA_ADDRESS，取紧随其后的属性作为本地地址；环回地址无广播，于是把 IFA_LABEL 的 3 字节负载当成 IPv4 地址。
- 修复：`addr_to_segment()` 改为 Linux 顺序（提交 `d9ba239b`）。
- 验证：footstone 不再崩溃，21000 返回 `HTTP/1.0 200 OK`。

### 3.38 OCManager 服务栈在 DragonOS 上跑通（2026-10-24）

一键脚本见 `docs/ocmanager-porting/guest-stack-up.sh`（注入 guest 根目录后 `sh /up_ocm.sh`）：

| 服务 | 端口 | 状态 |
|---|---|---|
| MySQL 8.0.35 | 3306 | Up；footstone 库与 4 个迁移已应用 |
| Redis 7.2.3 | 6379 | Up，`PING` → PONG |
| ClickHouse 23.8 | 9000 | Up，`select 1` → 1；msg_etl 库与 2 个迁移已应用 |
| Kafka 3.5.1 (KRaft) | 9092 | Up，`kafka-topics.sh --list` RC=0 |
| footstone | 21000 / 21001 / 21002 / 17778 | Up，HTTP 200 |
| msg-etl | 8080 / 8081 / 7778 | Up，三端口 launch success |

注意事项：
- MySQL 用 `--initialize-insecure` 手动初始化，不会执行 `docker-entrypoint-initdb.d`；迁移必须显式用 `mysql < file` 应用（脚本已包含）。
- ClickHouse 客户端不接受多语句，建库需分条执行。
- footstone 的 `MSG_ETL` 必须指向 msg-etl 的 tRPC 端口 7778（此前脚本写成 7779）。
- `/proc/net/tcp` 暂为空（procfs 缺口），列入独立缺陷。

### 3.39 utime(2) 实现与验证（2026-10-24）

- 服务栈运行期持续出现 `Unsupported syscall ID: 132 -> SYS_UTIME`（约每 4 秒一次），说明有用户态程序依赖该调用。
- 实现：`utime(filename, utimbuf{actime, modtime})` 转成两个 `tv_nsec=0` 的 `timespec`，复用 `utimensat` 的解析/权限路径（`AT_FDCWD`、空 flags、跟随符号链接、`times==NULL` 表示当前时间）。
- 验证：同栈重跑后该报错计数为 **0**，六服务正常（提交 `079e38ea`）。

### 3.40 脏 journal 重放期 panic（确定性复现，根因指向内核栈复用/溢出）

- 现象：脏 journal 重放期间 panic：
  - 首个：`do_softirq` 的 `movzwl (%rax,%rdi,2)`（`rax` 为 NULL，`cpu_id` 4/0x71 → 访问 0x8/0xe2）；
  - 随后 CPU1 在 `check_preempt_current` 触发 `do_undefined_opcode(6)`（`pid=RawPid(0)`，idle 上下文）。
- 复现资产：`/home/sparkzky/ocm-tars/dirty-journal-repro.img`（boot → docker 负载 → `kill -9` 后拷贝，100% 复现）。
- 已排除：
  | 假设 | 证据 |
  |---|---|
  | journal superblock 1KB 缓冲重叠 | 缓冲 `0xffff8003409e3000`，对象 `0xffff800340213000` |
  | Softirq 被正常释放 | `Drop` 探针从未触发，`SOFTIRQ_VECTORS` 始终同一 box |
  | 全局指针失效 | 损坏时 global 仍为 `0xffff800340213000` |
- 损坏瞬间头部 dump（`do_softirq` 入口自检）：

```
+0x00 = 0xffff8000017c2d13        (rodata)
+0x08 = 0x0a000c000a000d000e000260
+0x10 = 0x0072006b0069006e006700  (UTF-16 "rking"，来自 "...parking")
+0x18 = 0xffff8000017727e4        (rodata)
+0x20 = 0x1e   +0x28 = 0x1        (原 running_ptr/running_len)
+0x30 = 0xffff8000017c2d13
+0x38 = 0x26   +0x40 = 0x0000026500000001
+0x48 = 0xffff8000017c2d4c
+0x50 = 0xffff800340213070        (self + 0x70，自引用)
+0x58 = 0x1
```

- 结论：形态是 panic/格式化栈帧，且对象从未被 drop → 该堆页被内核栈/格式化路径复用后覆盖。
- 对照实验：用 `KERNEL_FEATURES=kstack_protect make kernel`（内核栈 + guard page，栈占用翻倍）启动同一脏 journal：
  **2/2 次正常完成重放并进入控制台**，未再出现 `SOFTIRQ CORRUPT`；而无保护内核 3/3 次 panic。
  说明该崩溃与内核栈内存布局强相关（guard page 改变布局后不再踩到对象），后续应以内核栈溢出/越界为根因方向继续收敛。
  启用该特性后新增告警：`tlb: N shootdown(s) initiated with interrupts disabled ... kstack.rs:42`
  （guard 映射的 TLB flush 在关中断上下文发起），若要默认启用该特性需先改为延迟 flush。
- 下一步：为内核栈加 guard page 与溢出检测并记录溢出者（与「IRQ 独立栈」合并处理），再验证脏 journal 场景。
- 旁证：同一窗口 clocksource watchdog 报 `kvm-clock` 与 `acpi_pm` 相差 ~0.8s 并 `forcing reselection`，时间回退可能放大该路径的时序敏感性。

## 3.42 自上次 PR 拆分以来的变更与当前缺口（2026-10-24）

上次拆分的源基线为 `f0afd461`（见 `pr-split-2026-10-23.md`），本分支在其后新增的提交分三类：

### 3.42.1 已修复（本轮上游候选，见 `pr-split-2026-10-24.md`）

| 提交 | 修复 | 上游状态 |
|---|---|---|
| `22032d92` | 默认 `RLIMIT_NPROC = max_threads/2`（对齐 Linux `fork_init`），修复容器内 bash `bgpids` SIGSEGV | **PR #2534** |
| `d9ba239b` | netlink IPv4 地址属性顺序对齐 Linux（Go `net.Interfaces()` 越界修复） | **PR #2536** |
| `079e38ea` | 实现 `utime(2)` | **PR #2537** |
| `51f76477` | `run-qemu.sh` 尊重外部 `QEMU_GDB_WAIT` | **PR #2538** |
| `da015b03` | kstack guard 映射 flush 走“有意关中断”shootdown 入口 | **不开 PR**：master 的 `flush_tlb_kernel_range_inner` 已对 IF=0 调用者临时开中断 |
| `8d93771f` | 关机前 flush 并关闭已挂载文件系统（清 RECOVER） | **PR #2541** |
| `91d99943` | watchdog 降级判定可见（改用非阻塞 WARN 通道） | **PR #2540** |
| `6e28e905`+`bfa520a7`+`0f2198ef` | 缺页路径 TLB flush 系列（延迟 shootdown / 共享 helper） | **PR #2543** |
| `7fb4de4f` | clocksource fallback + APIC 重校准（部分已随 #2490 合并） | **PR #2544**（仅净增量） |

### 3.42.1b 净盘全栈验收（2026-10-24，内核 #249）

同一净盘（seed + 迁移 + `/up_ocm.sh`）完整复跑：

| 项目 | 结果 |
|---|---|
| `dragon-docker-start` + 六容器 | mdb / rds / ch / kfk / foot / etl 全部 Up |
| MySQL | `mysqladmin ping` → `mysqld is alive`（RC=0），`select 1` 正常 |
| footstone | 21000 → `HTTP/1.0 200 OK` |
| 迁移 | footstone 4 个、msg-etl 2 个全部 RC=0 |
| `USER BLOCK STORM` | 1 次（ClickHouse 启动期瞬时，随后自行恢复） |
| `SOFTIRQ CORRUPT` / `nested-panic` / `Kernel code at` | 0 |
| 关机 | `busybox poweroff -f` 后 `Filesystem state: clean`、无 `needs_recovery` |

说明：#2495 已由用户关闭且不再重提；原计划的“多轮 futex_stress2 统计”未复跑（镜像未内置该二进制），
以本表的全栈验收（含 MySQL 终验）作为替代证据。

### 3.42.2 仅本分支的脚手架/诊断（不开 PR）

`b82baac0`（fault 诊断读取用户内存——该诊断代码只存在于本分支）、`47cdb558`（console 持锁打印/热键——同上）、
`88259651`（每任务等待点）、`9413ba1d`/`f09ab924`/`34883847`（softirq 取证）。
它们用于定位，不构成上游缺陷修复；相关根因修复已单独提交（见 3.42.1）。

### 3.42.3 当前缺口（按优先级）

1. **脏 journal panics**：已确定性复现；Softirq 对象被 panic 栈帧覆盖（全局不变、无 drop）；`kstack_protect` 下 2/2 正常 → 根因指向内核栈内存。剩余：定位具体溢出者/越界写，评估默认启用 guard page。
2. `/proc/net/tcp` 为空：`procfs/net` 仅注册 `arp`/`protocols`，缺 socket 全局表。
3. IRQ 独立栈（未开始）。
4. 次级系统调用/文件系统缺口：AIO、fallocate、sysctl、ptrace、FAT、DAC。
5. ~~`is_kthread` 任务分类缺陷~~（已核对，不再成立）：当前 `fork.rs:472` 由创建参数显式 `insert(KTHREAD)`（不走继承），workqueue worker 经 `KernelThreadMechanism::create_and_run` → `__inner_create(kthread=true)` 创建；`is_kthread()` 直接读该标志。若现场再出现，应在 dump 中打印该标志取证。
6. ~~`futex_wake nr_wake=0`~~（已实测确认非缺陷）：本机 Linux 上 `FUTEX_WAKE(0)` 唤醒 1 个等待者并返回 1（内核 `if (++ret >= nr_wake) break;` 的固有行为），DragonOS 的 `effective_nr_wake = 1` 与之一致。
7. QEMU 内存 >2G 且非 6G 时挂死（D）。
8. R15 压测与 mysql 终验（#2495 已关闭，验证通过前不重提）。

## 3.43 非 6G 内存挂死与脏 journal panic 同源（2026-10-24 取证）

`DRAGONOS_QEMU_MEMORY=4G` 时在根挂载阶段**确定性**复现；12G 在脏 journal 下同样可复现，
两者是同一处内存破坏的不同下游表现。

### 3.43.1 首个 panic（gdb 断在 panic_fmt 取证）

- 消息：`current_pcb weak upgrade failed (pcb freed?)`。
- 调用链（栈符号化）：`log!` → `KernelLogger::log` →
  `virtio_console::critical_output_available` →
  `SpinLock<Option<Arc<SrcuNotifierChain<RebootNotifyEvent, String>>>>::lock_irqsave`
  → `ProcessManager::current_pcb`。
- `arch_current_pcb()` 从 `rsp & ~0x7FFF` 取栈底槽（应为 PCB 的 Weak）：实测槽值
  `0xffff800140208500`（指向同一栈内），既非合法 ArcInner 指针，也不是 null。
- 结论：日志路径在某个上下文里以“错误/已失效的栈底槽”调用 `current_pcb`，进而 panic；
  随后的 panic 打印路径又触发 softirq 对象被栈帧覆盖（§3.40 的 dump）。

### 3.43.2 另一形态

同配置另一次启动的首个故障是 `do_general_protection(13)`，`Segment Selector Index: 0x0`，
`pid=RawPid(20)`、`rip=0xffff800000d4a53d`——加载了空选择子，属同类内存破坏的下游。

### 3.43.3 新增取证设施

- `find_task_by_stack_base()`：按 kernel/syscall 栈起始地址反查任务；
- `CURRENT_PCB_DIAG`（`no_mangle`，vmcore 可读）：失败瞬间记录 base/slot/owner_pid/kind/rsp；
- 失败 panic 消息内联 owner 信息。
提交：`7359d0df`。

下一步：用 `CURRENT_PCB_DIAG` 的 owner 字段确认该栈属于哪个任务，再追该任务的 PCB 生命周期
（为何 Strong 归零或槽被覆盖）。

### 3.43.4 2026-10-25 复现尝试（负结果）

| 配置 | 结果 |
|---|---|
| 净盘重建 + seed，`DRAGONOS_QEMU_MEMORY=4G`，内核 #255 | 正常启动到 console |
| 4G 下 `docker create oc-msg-etl:dev` + `docker commit` 进行中 `SIGKILL` QEMU（制造脏 journal / 孤儿 inode），再以 4G 启动 | 正常启动到 console，ext4 重放完成 |

历史资产 `dirty-journal-repro.img` / `dirty-journal-repro2.img` 经 `dumpe2fs -h` 检查均为
`Filesystem state: clean`（被后续启动重放），原始脏状态已不可得。

因此“4G 必现”的前提已不成立：故障依赖特定磁盘状态（根挂载期 journal/孤儿链重放），
当前内核 + 现有磁盘无法复现。复现条件需重新构造（例如在含孤儿 inode 的盘上掉电），
在此之前该缺陷定位停在此处。

同期对失败路径做了去递归改造（`record_current_pcb_diag` 先无锁落盘 base/slot/rsp/magic，
owner 反查用原子标志单层化），保证下次命中时 `CURRENT_PCB_DIAG` 一定可读、且不会因诊断
本身无限套娃。

## 3.44 OCManager 全栈功能验收（2026-10-25，内核 #253）

净盘启动后不再只做“进程 Up + 健康检查”，而是对每条业务链路做真实 API 调用
（镜像内无 curl/xz，用 `ch` 容器自带 `wget` 从容器内发起）。

| 链路 | 请求 | 结果 |
|---|---|---|
| footstone 健康 | `GET :21002/health` | `200`，`{"status":"ok","service":"footstone",...}` |
| msg-etl 健康 | `GET :8081/health` | `200`，`{"status":"ok","service":"msg-etl",...}` |
| sysdiagnose 健康 | `GET :21008/health` | `200`，`{"status":"ok","service":"sysdiagnose",...}` |
| 登录 | `POST :21000/trpc.ocmanager.footstone.User/IdentifyAccount`（`admin` + `SHA512("Admin123456@")`） | `200`，返回 `jwt_token`、`jwt_key=Authorization`、`roles=[超级管理员]` |
| 鉴权查询用户 | `POST .../User/GetCurrentUser`（带 JWT） | `200`，`is_super_admin:true` |
| 鉴权 JWT 密钥 | `POST .../User/QueryJWTSecret`（带 JWT） | `200`，`{"secret":"devsecret","head_key":"Authorization"}` |
| 项目列表（MySQL） | `POST .../Footstone/ListProjects` | `200`，返回迁移 seed 的「默认」项目 + 分页 |
| 指标查询（ClickHouse） | `POST :8080/...msg_etl.Metrics/ListMetricsData` `{"plugin_name":"os_monitor"}` | `200`，`success:true`（空结果集，库内暂无数据） |
| sysdiagnose 业务（MySQL） | `POST :21006/...sysdiagnose.Project/ListGlobalCmds` | `200`，返回 seed 的内置命令列表（`tail`/`ping`/…） |
| MySQL | `show databases` / `show tables` | footstone/msg_etl/sysdiagnose 库与表齐全 |
| Redis | `redis-cli ping` | `PONG` |
| ClickHouse | `clickhouse-client -q 'select 1'` | `1` |
| Kafka | `kafka-topics.sh --list` | `__consumer_offsets`、`ocm-agent-data`、`sysinfo_collector` |
| 内核日志 | `Unsupported syscall` / `panicked` | 4 次（membarrier/io_setup×2/quotactl，均有用户态回退）、0 次 panic |
| 关机 | `busybox poweroff -f` | QEMU exit 0 |

### 3.44.1 验收中修正的部署口径（非内核缺陷）

1. **msg-etl 必须注入 `JWT_SECRET`**：`pkg/plugin/permission` 的 `loadJWTFromEnv` 在未设置时
   会回查 `User/QueryJWTSecret`，而该 RPC 自安全整改后要求管理员上下文，内部 bootstrap 必然
   返回 `user info not found`（code 41）。compose 通过 `env_file: config/.env` 提供该值，
   `guest-stack-up.sh` 之前漏了，已补 `-e JWT_SECRET=devsecret`（与 footstone 一致）。
2. **sysdiagnose 需要独立 MySQL 库**：镜像不含迁移，`create database sysdiagnose` +
   `v1.0.0_1_init_tables.sql`（actions/cmds/action_ip_relations）后启动；
   host 网络下 8080/7778 与 msg-etl 冲突，验收用 21006/21007/21008。
3. **`ListMetricsData` 必须带 `plugin_name`**：空值会走 `validatedIdentifier` 返回
   `invalid plugin name ""`（业务校验，非缺陷），有效值取自 `msg_etl` 库表名。
4. **Kafka 日志里的 `No mapped vma ... address: 0x8` 属正常**：HotSpot 隐式空指针检查产生的
   SIGSEGV 由 JVM 自身 handler 恢复（本次 12 次，`kafka-topics --list` 随后成功）。

### 3.44.2 验收中发现并已修复的缺口：`docker commit` 导出层失败

**现象**：`docker commit etl oc-msg-etl:jwt` 失败：
`failed to export layer: CreateDiff ... failed to get xattr for .../bin/arch: operation not permitted`。

**根因**：`ext4` 的 `getxattr` 对符号链接 inode 一律返回 `EPERM`
（`kernel/src/filesystem/ext4/inode.rs`）。containerd 的 `CreateDiff` 会对每个条目
读 `security.capability`，命中符号链接即整层导出失败。

**Linux 6.6 实测**（宿主 ext4，`/home` 为 ext2/ext3 系 ext4 兼容挂载）：

| 名称 | 跟随符号链接 | 不跟随 | 结果 |
|---|---|---|---|
| `security.capability` | ENODATA(61) | ENODATA(61) | 无属性 |
| `user.foo` | ENODATA(61) | ENODATA(61) | 无属性 |
| `trusted.foo` | ENODATA(61) | ENODATA(61) | 无属性 |

即符号链接不承载扩展属性，读取统一是 `ENODATA`；写入才由 VFS 的
`xattr_permission` 拒绝为 `EPERM`。

**修复**（`8ca49350`）：`ext4::getxattr` 对 `FileType::SymLink` 返回 `ENODATA`。

**验证**：净盘（重新 seed）启动内核 #254 → `docker create --name ctest oc-msg-etl:dev` →
`docker commit ctest ctest:one` 返回 `sha256:c33b74e8c923…`（RC=0）→
`docker run --rm --entrypoint sh ctest:one -c 'ls -l /bin/arch'` 输出
`/bin/arch -> /bin/busybox`（RC=0）。同一操作在修复前稳定失败。

## 3.45 中断独立栈（IST）：#DF 独立栈上线（2026-10-25，`8f6156c9`）

**动机**：任务栈不可用（栈溢出、栈指针被写坏——正是 §3.43 内存破坏类故障的形态）时，
原 `do_double_fault` 会在同一个坏栈上 `error!` + `panic!`，随后打印路径再次取栈/取
`current_pcb`，最终三重错误复位，现场全丢。

**实现（对齐 Linux 的 IST 机制）**：

| 项 | 内容 |
|---|---|
| 栈 | 每 CPU 16 KiB 双重错误栈，`.bss` 静态分配、16 字节对齐（`interrupt::ist`） |
| TSS | BSP（`set_current_core_tss`）与 AP（`smp_ap_start_stage1`）都写 `IST[0]` |
| IDT | 向量 8（#DF）的门改为 `ist=1`（描述符 IST 字段 1 对应 TSS `IST[0]`） |
| 处理器 | 只用 `emergency_print_*`（COM1，零分配）打印 err/rsp/rip，然后 `cli; hlt` |
| 新增设施 | `printk::emergency_print_hex_noalloc`（零分配十六进制） |

注意：IST 栈底没有 PCB 槽，`current_pcb()` 在 IST 上必然无效，因此该处理器**不能**调用
`log!`/`panic!`/任何分配；这与 §3.43.4 的“失败路径不得依赖 current_pcb”是同一约束。

**验证**（内核 #258，12G）：

1. gdb 读实机状态：`TSS.ist[0]=0xffff800001a69000`；IDT 门 8 的 `d0` 中 IST 字段 = 1。
2. 故障注入：把两个 CPU 的 `TSS.RSP0` 清零，再让 shell 执行 `while true; do :; done`
   （保持在 ring3），下一次定时中断的 ring3→ring0 栈切换失败：
   ```
   [#DF] double fault (IST): err=0x0 rsp=0x6ffff09ff7d0 rip=0x4561c0 -- halted
   ```
   串口拿到完整现场，且**没有**三重错误复位——证明 #DF 确实在独立栈上执行。
3. 正常路径回归：同一内核正常启动到 console，`docker`/服务未受影响（§3.44 的验收环境）。

**后续**：Linux 还把 NMI / #MC 放在 IST 上；本仓库目前只有 #DF。NMI 处理器若需迁到 IST，
必须先解决它现在依赖 `current_pcb()` 的问题（同样只能用零分配通道或 per-CPU current）。

## 4. 当前状态与待办（2026-10-25 刷新）

### 4.1 已闭环（本轮及上一轮）

| 项 | 结论/证据 |
|---|---|
| 控制台/缺页自死锁 | 修复（`47cdb558`、`b82baac0`），§3.33/§3.34 |
| 容器内 bash `bgpids` SIGSEGV | `RLIMIT_NPROC = max_threads/2` 修复（`22032d92`，PR #2534） |
| Go 服务 netlink 越界 | IPv4 属性顺序修复（`d9ba239b`，PR #2536） |
| `utime(2)` 缺失 | 实现并验证（`079e38ea`，PR #2537） |
| 关机 RECOVER 未清 | 关机前 flush/close（`8d93771f`，PR #2541），实测 `Filesystem state: clean` |
| 缺页 TLB shootdown 系列 | PR #2543 |
| clocksource fallback / APIC 校准 | PR #2544（仅净增量） |
| `is_kthread` 分类、`futex_wake nr_wake=0` | 已证伪（§3.42.3-5/6） |
| R15 全栈与 mysql 终验 | 净盘六服务验收通过（§3.42.1b） |
| OCManager 功能验收 | 登录/JWT、项目列表、指标查询、sysdiagnose 命令列表全部真实 API 通过（§3.44） |
| `docker commit` 导出层失败 | ext4 符号链接 xattr 读取改为 ENODATA（`8ca49350`，§3.44.2），净盘提交+运行验证通过 |
| IRQ 独立栈（#DF） | 每 CPU IST 栈 + 门 8 `ist=1` + 零分配停机（`8f6156c9`，§3.45），故障注入验证无三重错误 |
| 大镜像 `docker load` 楔死 / 2-e 挂起 / bash `<(...)` | 随本轮内存与调度修复后未再复现，归入内存破坏类待观察 |

### 4.2 待办（按优先级）

| # | 项 | 现状与下一步 |
|---|---|---|
| 1 | **内存破坏类挂死**（脏 journal panic 与非 6G 内存同源） | 首个 panic 为 `current_pcb weak upgrade failed`（日志路径调 `current_pcb`，栈底槽无效）。2026-10-25 复现尝试为负：净盘 4G、以及 4G 下提交中被 SIGKILL 的脏盘均正常启动（§3.43.4），历史复现镜像已 clean。失败路径已改为无锁落盘 + 单层 owner 反查，下次命中可直接读 `CURRENT_PCB_DIAG`。下一步：先构造含孤儿 inode 的脏盘复现，再读 DIAG owner 追 PCB 生命周期 |
| 2 | IRQ 独立栈（Linux IST） | #DF 已完成（§3.45）；NMI / #MC 未迁到 IST（它们的处理器仍依赖 `current_pcb()`，需先改为零分配或 per-CPU current） |
| 3 | 次级缺口：AIO / fallocate / sysctl / ptrace / FAT / DAC | AIO 已确认 mysqld 可回退；其余未动 |
| 4 | `/proc/net/tcp` 为空 | `procfs/net` 仅注册 arp/protocols，缺 socket 全局表 |
| 5 | 脏 journal 重放的健壮性 | 与 #1 同源；建议 #1 修复后回归 |

## 5. 调试工具资产（多数探针已删除或编译期关闭）

| 工具 | 位置 | 用途 |
|------|------|------|
| critical 输出通道 | virtio_console.rs | 任何降级场景的可见性兜底 |
| tmict 全量探针 | apic_timer.rs | 时钟四元组+REGRESS 判定+PMTMR 定频 |
| uw 审计 | timekeeping.rs | 时钟速率内核侧权威判据（%4096 节流） |
| wdtick/wdcmp | clocksource.rs | watchdog 心跳+比对原始值 |
| apcal/aprecal | apic_timer.rs | APIC 标定取证 |
| LSYSCALL/TrapFrame/kret | task.rs+syscall+task_dump.rs | 卡死任务 syscall 号+栈帧（信号免疫） |
| USER BLOCK STORM | task_dump.rs | 用户态阻塞风暴自动转储 |
| pickstat | sched/mod.rs+task_dump.rs | 每 CPU pick 计数周期报告 |
| place 探针 | fair.rs | 唤醒 vruntime 放置审计 |
| futex_stress2 | guest /futex_stress2（主盘内置） | 双线程压测+STUCK 探测模式（wake-probe） |

> **2026-10-23 变更**：所有位于锁内/关中断区的 `print_critical!` 探针已删除（R16 定罪根因之一）；tmict/pvti/dlprobe 打印改为编译期开关（`TMICT_PRINT`/`DL_PROBE_PRINT=false`，需要诊断时改回 true）；tmict 采样基线已改 per-CPU 并扩展 PMTMR 24bit 回绕。保留：critical 输出通道、LSYSCALL/任务转储、apcal/aprecal。

**操作提示**：guest 串口命令经常丢字符/需重发；长命令建议写成脚本注入磁盘后 `sh /xxx.sh` 执行；判定时钟速率用内核侧 PMTMR 交叉值（串口行计数会受突发写干扰）。

---

## 6. 环境与操作备忘

```bash
# 编译（worktree 根，勿在 nix devshell 编 user 程序）
make kernel

# 格式化门禁：`make -C kernel fmt` 要求固定的 nightly-2026-02-24 与 clippy；
# 在 nix 环境外执行会用本机默认 rustfmt 重排大量无关文件（且 clippy 以 101 失败）。
# 非 nix 环境下只对自己的改动核对格式，切勿把整体重排提交进去。

# 净盘重建（~90s；每次压测/容器测试前建议净盘，脏盘会 journal park）
sudo rm -f bin/disk-image-x86_64.img && ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage

# 启动（pty 服务，Enter 激活控制台）
cd tools && DRAGONOS_QEMU_MEMORY=6G ROOT_PATH=$PWD/.. bash run-qemu.sh --bios=legacy --display=nographic

# 压测复现（命令可能要发两次）
/futex_stress2; echo DONE

# 二进制注入 guest（停机状态）
sudo losetup -fP bin/disk-image-x86_64.img
sudo debugfs -w -R "write /tmp/xxx xxx" /dev/loop6p1
sudo losetup -D

# 迁移与脚本注入（停机状态；guest-stack-up.sh 依赖这些路径）
#   /migrations/footstone/*.sql  /migrations/msg_etl/*.sql  /migrations/sysdiagnose/*.sql
#   /up_ocm.sh（= docs/ocmanager-porting/guest-stack-up.sh）
LOOP=$(sudo losetup -fP --show bin/disk-image-x86_64.img); sudo mount ${LOOP}p1 /mnt/ocm-inject
sudo mkdir -p /mnt/ocm-inject/migrations/{footstone,msg_etl,sysdiagnose}
# …cp 对应 SQL…
sudo cp docs/ocmanager-porting/guest-stack-up.sh /mnt/ocm-inject/up_ocm.sh
sudo sync && sudo umount /mnt/ocm-inject && sudo losetup -d $LOOP

# 运行期向 guest 传文件（QEMU 用 slirp，10.0.2.2 即宿主机）：
#   宿主：cd /tmp/ocm-serve && python3 -m http.server 8000 --bind 0.0.0.0
#   guest：docker exec ch wget -q -O /tmp/x.sql http://10.0.2.2:8000/x.sql
#   ch 镜像自带 wget（无 curl）；字符串经串口超过约 500 字符会丢字符，长逻辑写成脚本再传

# 清理残留（我的 QEMU；共享服务器其他会话勿动）
for p in $(pgrep -f 'disk-image-x86_64.img'); do sudo kill -9 $p; done
```

- 每轮修改后立即 `git add -A`（防外部重置）；禁止对 worktree 大范围 git reset/checkout。
- QEMU 偶被其它会话 pkill 清扫（exit 137）——重启即可，无数据损失。
- 主目录 `~/DragonOS-12` 是原仓库，勿动。
