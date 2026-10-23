# OCManager → DragonOS 移植：修复清单与遗留问题（会话快照）

> 更新时间：2026-10-18 16:40
> 工作区：`~/DragonOS-ocmanager`（origin/master@a4013617 worktree，全部改动已 `git add`，55 文件，未提交）
> 详细根因链/判据/复现命令见 `assessment-2026-10-16.md` §1-17（本文为索引层，按编号交叉引用报告章节）

---

## 一、已修复并实测验证（内核缺陷）

| # | 缺陷 | 修复方案 | 位置 | 验证 |
|---|------|---------|------|------|
| A | overlayfs 合并目录 getdents 枚举损坏（`OvlInode::find` 不处理 `.`/`..`），阻塞所有 entrypoint 工作负载 | `find` 补齐 `.`/`..` 命中路径 | `kernel/src/filesystems/overlayfs/` | QEMU 实测 docker 容器 entrypoint 正常执行 |
| B | 容器 netns 内 iptables-nft `xt_udp` match 编程失败 → embedded DNS 死亡 | 新增 `NftXtUdp` 表达式（对齐 nf_tables xt 兼容层语义） | `kernel/src/net/netfilter/` | compose 服务名解析/DNS 全链路通过 |
| G | open 未知 flag 位被拒（容器运行时传高位标志） | `OpenFlags::from_bits_truncate` 替代严格解析 | `kernel/src/filesystem/vfs/syscall/open_utils.rs` | docker create/start 路径通过 |
| — | `times()` 系统调用缺失（mysqld 启动期） | 按 Linux 语义实现 | `kernel/src/time/syscall/sys_times.rs` | mysqld 过Times |
| — | `do_kern_addr_fault` oops：内核地址缺页按用户语义处理 | 区分访问上下文，内核态走 fixup/oops 路径 | `kernel/src/arch/x86_64/mm/fault.rs` | 实测无 oops |
| H | tick 毒化：一次 kvm-clock 单次倒置读数经 wrapping_sub 变巨大前跳，毒化 tick 绝对期限 | `clocksource_delta` 倒置按 0 elapsed 处理（对齐 Linux timekeeping 语义）+ 倒置计数升级 watchdog | `kernel/src/time/timekeeping.rs:246-268` | 单次倒置自愈，tick 恢复 |
| H' | cfs 1250Hz 风暴引擎（带宽欠费域武装死循环） | `set_next_task` 去武装 + `ARMED_REMAIN` 预算域门控 | `kernel/src/sched/fair.rs`、`kernel/src/sched/cfs_bandwidth.rs` | ftrace 注入率从 1756/s 回落 |
| F5 | ext4 journal 重放 4KB 栈帧堆直建（栈溢出） | 重放链堆内存化 | `crates/another_ext4/` | 重放不再溢栈 |
| F6 | KMSG 锁内零堆操作死锁（printk 级联） | 锁内禁分配路径 | `kernel/src/filesystem/procfs/kmsg.rs:53-69` | 静默死亡形态消灭 |
| F7 | write_block 范围校验缺失 + PCB 守卫 hexdump 无条件刷屏 | 范围校验 + hexdump 条件化 | `kernel/src/arch/x86_64/process/mod.rs:603-632` | 启动循环修复 |
| C | OOM 全链 panic：`try_direct_reclaim` 缺失、分配器 panic、printk 递归 | ① `mm/page.rs` 增加 try_direct_reclaim ② `kernel_allocator.rs` 停车不 panic ③ `oom.rs` 零分配 SIGKILL 兜底 ④ `printk.rs` emergency_print_noalloc ⑤ `debug/panic/mod.rs` PANIC_IN_PROGRESS 嵌套守卫 | `kernel/src/mm/`、`kernel/src/debug/` | 2G 内存 docker load 实测 `[alloc-exhausted] parking task, kernel alive`；嵌套 panic 打印 `parking` 替代级联（F6 终验实战确认） |

## 二、本会话新增修复（R9-R11，编译验证 #95+）

| # | 缺陷 | 修复方案 | 位置 | 验证 |
|---|------|---------|------|------|
| R9-1 | clocksource watchdog **长间隔失明**：watchdog tick 被延迟（系统降级自身就会延迟它）时长间隔分支 `continue` 跳过偏差检测，慢速时钟源恰好在此场景漏检 | 移除跳过：长间隔仅告警，比对必须继续（Linux 6.6 无此豁免） | `clocksource.rs:1116-1130` | 启动期实测捕获 kvm-clock 慢 5 倍（cs=504ms vs wd=1504ms）进入判定分支 |
| R9-2 | `active_clocksource_regressed` 只检测冻结/倒退，对**慢速**时钟失明 | 增加 <75% TSC 参考速率判不可信 | `clocksource.rs:1157-1161` | 编译在环（注意：自旋窗实为 1s 而非注释的 1ms，待改） |
| R10-1 | **acpi_pm 条件初始化缺陷**：`!kvmclock_ok && !hpet_ok` 才注册 → kvm-clock 存在时 PMTMR 永不初始化 → watchdog 无参考源、系统无备用时钟源 | 无条件注册（对齐 Linux：kvmclock/hpet/pmtmr 并存） | `init/mod.rs:138-153` | 实测 `pmtmr port=0x608 registered` ✓ |
| R10-2 | **watchdog 参考源反向归责**：参考=rating 最高者=kvm-clock（慢速）→ 健康 acpi_pm 反被判"偏快"标 unstable | 独立硬件时基（hpet/acpi_pm）优先作参考；同类内按 rating | `clocksource.rs`（independent_watchdog_preferred/watchdog_reference_better/select_watchdog_replacement/enqueue_watchdog） | 实测 `wdtick run=true ref=acpi_pm` ✓ |
| R10-3 | **APIC 计数率标定形态不一致**：启动标定（周期+LVT屏蔽+u32::MAX）与运行形态（单次+解屏蔽）在 KVM 平台语义不一致 → 中断风暴 | `recalibrate_apic_count_runtime`：PMTMR 独立参考、运行形态武装（与 program_deadline_delta 完全一致）重测并刷新 pcount；BSP 于 setup_arch_post、AP 于自身 init 尾部调用 | `apic_timer.rs:159-207`、`init/mod.rs:154-157`、`init_ap_apic_timer` | 实测 `aprecal rate=250.2MHz`，**fire 率归一 ~245/s（512-2048Hz 风暴消失）** ✓ |

## 二·补10：R13 系列修复正式入表（2026-10-20/21，#129-#137，此前仅散于叙事节）

| # | 缺陷 | 修复方案 | 位置 | 验证 |
|---|------|---------|------|------|
| R13-1 | **kvm-clock 读数冻结**：futex 压测负载下单调钳制把读数永久锁死（rawrate=0.000），tick/调度唤醒全停 | `KvmClock::read` 加冻结检测：连续 3×20ms 读数同值而独立 TSC 前行 → 自贬 rating 0 + MarkUnstable + 唤醒 watchdog 重选 acpi_pm（对齐 Linux pvclock unstable 处理） | `kvm_clock.rs:108-163` | 编入 #131+；压测曾触发时钟冻结路径（FROZEN 打印待实战场捕获） |
| R13-2 | **idle CPU 入队不 kick**：远端唤醒任务入 idle CPU 队列后仅置 NEED_SCHEDULE，halt 中的 CPU 无法感知（对齐 Linux ttwu 的 kick 不变式） | `enqueue_task_on_cpu`：was_idle 且入队后非 idle → 无条件 `send_resched_ipi(target_cpu)` | `sched/mod.rs:1559-1569` | 编入 #133（未解主诉 STUCK——见补9，但语义修复保留） |
| R13-3 | 任务转储缺 syscall 号（TrapFrame 被 Go SIGURG 信号帧污染，rax=垃圾） | PCB 新增 `last_syscall`（syscall 入口原子记录），转储打印 LSYSCALL+errcode+参数 | `task.rs:159-161`、`syscall/mod.rs:70-72`、`task_dump.rs:121-133` | **实战定罪**：docker 全家 LSYSCALL=202(futex) |
| R13-4 | B 态检测被空闲 workqueue worker 误触发（is_kthread 分类缺陷）+ 对 Blocked(true) 形态失明 | 检测改为：用户态（pid>100 非 kthread）Blocked 计数 ≥25 + 连续窗口（USER BLOCK STORM） | `task_dump.rs:57-88` | 干净复测通过（无误报；docker 楔死形态 43 任务可触发） |
| R13-5 | 探针自伤：place 探针 edit 误删 `se.vruntime = vruntime.wrapping_sub(lag)` 赋值 | 恢复赋值语句（#137） | `fair.rs:1258` | 恢复后 vr==avg==min 一致性验证 ✓（二·补7/8 的 vr≠avg 数据作废） |

### R13 阶段终局定性（经五层证伪）
futex 层→wakeup 吞错→idle-kick→vruntime 放置 **全部无罪**；饥饿机制收敛到：**worker 被唤醒入队（空队列、最优先 vruntime）后，其所在 CPU（疑 CPU1）在 STUCK 期间从未执行 pick** —— 下轮用 pick 时间窗探针 + enqueue target_cpu 打印一步定罪（零 pick ⇒ 查 idle halt 的 resched IPI 送达/处理链）。

## 二·补2：R12 后续修复（2026-10-18 21:30-22:45，编译验证 #110-#118）

| # | 缺陷 | 修复方案 | 位置 | 验证 |
|---|------|---------|------|------|
| R12-1 | AP 重标定 **RefCell 双重借用 panic**（AP1 上线即炸） | `init_ap_apic_timer` 先 `drop(local_apic_timer)` 再调 `recalibrate_apic_count_runtime` | `apic_timer.rs:144-147` | cpu=1 aprecal 250.3MHz 正常输出，panic 消灭 |
| R12-2 | 'D' 热键在 tty 积压时永不可达（检查在 enqueue 失败 break 之后） | 热键检查移到 enqueue 前 + room==0 时旁通丢弃非 'D' 字节保热键 | `virtio_console.rs:523-553` | 编译在环（本平台 stdin 实际不走此驱动，见下） |
| R12-3 | stdin 实际经 QEMU mux 路由到 8250（非 virtconsole）——8250 路径无热键 | `pump_rx` 闭包捕获字节，Enqueued/NoRoom 两分支后统一判定 'D' | `serial8250_pio.rs:797-829` | 编译在环（仍未命中，触发路径之谜未解，被 R12-4 取代） |
| R12-4 | **dockerd B+ 卡死无法取证**（'D' 热键不可靠 + 系统不停摆导致停摆看门狗也不触发） | stall_watchdog 新增 **B 态持续检测**：连续窗口扫描 `Blocked(false)` 任务，命中自动转储（触发不依赖任何输入路径） | `task_dump.rs:32-76` | **实战触发**：`UNINTERRUPTIBLE DETECTED: 14 tasks blocked for >20s` 自动产出全任务转储 |
| R12-5 | 任务转储只有 pid/名称/状态，无法定位卡死 syscall | dump_all_tasks 对用户态 Blocked 任务打印**内核栈顶 TrapFrame 的 rip/rsp**（离线 nm 反解符号） | `task_dump.rs:109-130` | 编译 #118 在环（本轮因脏盘 park 未跑完 mysql，待实战） |

### R12 后续关键认知
- **时钟链 100% 修复确认**：uw 探针（update_wall_time 每 4096 次更新节流打印 d_raw/d_base）证明 base 与 raw 精确同步（差 20ns/更新）、每更新恰 4.0ms（tick 间隔）、mysql 负载下 886.2s@885s wall = 100.1% 真实速率
- **"同源双速率"彻底定性为伪象**：串口相邻行产出时间可差 6 分钟（guest 控制台积压批量 flush）+ PMTMR 24 位探针窗口 >4.69s 双 wrap 低估 Δ（tsc_hz=9.1GHz/rawrate=2337 伪象来源）
- **dockerd 链状态图谱**（B 态转储实测）：43×Blocked(true)（9+ dockerd/containerd 线程可中断睡眠永不醒）+ 13×Blocked(false)（12 正常内核线程 + **pid 267 containerd 卡真不可中断 IO**）+ 8 Runnable
- busybox ps 显示的"B+"是显示语义，内核真实区分 Blocked(interruptible)
- QEMU stdin 路由：`-chardev stdio,mux=on` 同时接 `-serial`（8250）、`-monitor`、`-device virtconsole` 三前端，默认焦点 8250

## 二·补3：dockerd 楔死根因钉死（2026-10-19 深夜，#126-#127）

**卡死系统调用 = `futex`(202)**——LSYSCALL 探针（PCB 记录 syscall 入口号，免疫 Go SIGURG 栈帧重建）实测：dockerd/containerd/docker 几乎全部线程 LSYSCALL=202，仅个别 247；dockerd **自启动时刻起**即 Blocked 于 futex_wait。

### 推理链闭环
Go runtime 的 M 线程 park 依赖 futex_wait/futex_wake 对；唤醒丢失 → 全部 goroutine 调度停摆 → 43×Blocked(true) 永不醒 + daemon 0% CPU + 容器死亡 + CLI 挂起（等 daemon 的 futex/pipe）。**嫌疑路径 = 内核 futex 子系统的 wake 匹配/wait 入队（特定 op：PRIVATE_FLAG/WAIT_BITSET/requeue）**

### 本轮新增探针（全部在环验证）
| 探针 | 位置 | 说明 |
|------|------|------|
| **LSYSCALL**（决定性） | `task.rs:last_syscall` + `syscall/mod.rs:70` 入口 store + dump 打印 | 每任务最近 syscall 号，信号免疫 |
| 内核栈 kret 扫描 | `task_dump.rs` 自 TrapFrame 向下扫内核文本地址 | nm 反解内核侧链 |
| USER BLOCK STORM 检测 | 阈值 ≥25 用户态 Blocked + 连续窗口 | 替代 Blocked(false) 检测（docker 链是可中断态，原检测盲） |
| trapframe errcode/参数 | rdi/rsi/rdx/r10 | errcode 被 Go 信号帧污染为 -1，故需 LSYSCALL |

### 已证伪的支线（勿重走）
- 栈顶 TrapFrame 法：Go SIGURG 抢占重建帧，rax/errcode 全是垃圾（rax 曾现 ASCII "USER=roo"）
- 用户态二进制符号化：PIE 基址未知 + Go pclntab 与 ELF 符号错位（containerd/dockerd 已从盘内提取至 /tmp/guest-*）
- Blocked(false) 检测：docker 链是 Blocked(true)，不可中断检测对此形态失明；唯一 Blocked(false) 用户态是空闲 workqueue worker（is_kthread 分类缺陷：WorkQueue worker 无 kthread 标记）

### 下轮一步修复
审 `kernel/src/ipc/?` 或 `sched/futex` 实现的 futex_wake 匹配逻辑与 futex_wait 入队/唤醒（重点：wake 返回值、hash 桶匹配、uaddr 比较、WAIT_PRIVATE 标志路径）；写 C 单测：两线程 futex wait/wake 循环 1e6 次找丢失率——单测可在用户态直接复现。

## 二·补4：futex 压测复现与根因反转（2026-10-19 23:25，#129）

### 压测复现（传输盘 /tmp/futex_stress，双线程 FUTEX_WAIT/WAKE_PRIVATE ping-pong 10⁶ 次）
- **78 万/58 万次迭代后 STUCK**（~10⁻⁶ 概率）——与 dockerd 数分钟停摆的频率吻合
- **WKFAIL 探针零输出**：`let _ = ProcessManager::wakeup()` 吞错假设证伪（wakeup 从未 Err）

### 根因大反转（两线合一）
STUCK 现场串口显示 **`rawrate=0.000` + now/cl 冻结在低位 + kreg=1** —— kvm-clock 读数在**高频 syscall/线程切换负载下再次完全冻结**！tick 停 → 调度定时唤醒全停 → futex_wait 永不醒 → STUCK//dockerd 楔死。
- **futex 层无罪**（wake/wait/wakeup 状态机审查 + WKFAIL 零输出）
- mysql 负载 15 分钟时钟 100.1% ✓ vs futex 压测几分钟即冻结：**触发条件 = 超高频上下文切换/vCPU 颠簸**（怀疑 vCPU 在 pCPU 间迁移 + pvti 抖动 → `KvmClock::read` 的 fetch_max ratchet / pvclock LAST_VALUE ratchet 锁死在旧值 → 永久冻结）
- **watchdog 未救场**：ref=acpi_pm 已就位，但冻结态下 watchdog 自身的 timer 采样可能也停（待查 cs_watchdog timer 是否依赖同源 tick）

### 下轮修复路径（一步可判）
1. kvm-clock read 加"冻结检测"：连续 N 次 read 返回相同 max 值而 TSC 前进 > 阈值 → 标记 kvm-clock unstable（对齐 Linux pvclock unstable 处理），强制重选 acpi_pm
2. 或根治：查 KVM pvti 冻结的触发（vCPU 迁移时 MSR_KVM_SYSTEM_TIME 重写？）——QEMU 加 `-drdttsc` 对照
3. 压测二进制：/tmp/futex_stress{,2}（v2 带唤醒探测模式，STUCK 后保现场 pause）；注入用 base64 串口分块（传输盘写入会被 guest QEMU 页缓存覆盖——已实证）

## 二·补5：probe 二分定罪——调度层丢失（2026-10-20 00:00，#131）

**futex_stress2 的 wake-probe 模式实测定罪**（注入方式：停机 debugfs 写主盘 p1 分区根目录，运行 `/futex_stress2`）：
```
STUCK at iter 1 — entering wake-probe mode
probe[0] wake=1 errno=0 ftx=1   ← FUTEX_WAKE 返回 1（waiter 出队+wakeup"成功"），但 worker 1 秒内没跑
probe[1] wake=1 errno=0 ftx=0   ← 第二次 wake 后 worker 才实际运行
FINAL: ok=2 — worker alive
```

### 三层二分结论
| 层 | 判据 | 结论 |
|----|------|------|
| futex 层 | wake 返回 1 = waiter 在队、匹配、出队、waker.wake() 执行 | **无罪**（终审） |
| ProcessManager::wakeup | WKFAIL 探针零输出（无 Err） | **无罪** |
| **调度层（wakeup→runqueue/kick）** | wakeup"成功"返回但目标线程不运行；第二次 wake 才跑 | **丢失点** |

### 下轮修复目标（一步可判）
审 `ProcessManager::wakeup` → `sched_info` 入队 → `reschedule/kick` 链：① 入 runqueue 与目标 CPU 正在 schedule() 的竞态（enqueue 被 dequeue 窗口吞）；② idle CPU 缺 reschedule IPI（任务入队但无人被踢醒，等任意下次调度点才跑——与本探针"第二次才跑"吻合）；③ kick IPI 丢失。判据工具现成：wake-probe 模式 + 打印 wakeup 后目标 CPU 的 runqueue 长度/need_resched。

注：本轮 STUCK at iter 1（首次循环即丢）远早于上轮的 58 万次——丢失率随启动状态波动大，与 kvm-clock 冻结可能是两个独立诱因共同作用（时钟冻结时 timer 唤醒也走同一条调度丢失路径）。

## 二·补6：kick 修复证伪 + CFS 选择性饥饿定性（2026-10-20 22:00，#133）

**idle-kick 修复后 probe 完全复现同形态**（`wake=1,1 / RECOVERED at probe 1`）——enqueue 后补发 resched IPI 没有解决问题，"idle CPU 缺 kick"假设证伪。

### 证据链重排（三层证伪后的第四层定性）
```
probe[0] wake=1 ftx=1     ← waiter 出队+wakeup 成功，worker 在 runqueue
（main 自旋 ~0.5-1s，worker 不被调度）
probe[1] wake=1 ftx=0     ← main sleep(1) 让出后 worker 立即运行、完成本轮、又挂回 futex
```
- worker **从未丢失**：一直在 runqueue、状态健康（FINAL: worker alive, ok 计数正常推进）
- 卡点重定性：**main 用户态自旋期间 CFS 永不选中 worker**；main 一让出立即选中——非"唤醒丢失"，是**唤醒任务的选择性饥饿**
- 4ms tick 理应轮转（自旋 2e8 次远超）——tick 抢占也没换上 worker

### 新主嫌疑（下轮首查）：CFS/EEVDF 唤醒任务的 vruntime 放置缺陷
`enqueue_task_on_cpu→activate_task`（含 ENQUEUE_WAKEUP）→ `place_entity` 语义：Linux 唤醒时给 `vr = min_vruntime - sleep 补偿`（让它更"优先"）。若 DragonOS 实现为**正向偏移**（唤醒惩罚/初值异常），worker 的 vruntime 持续劣于自旋中的 main，EEVDF `pick_eevdf_entity` 永不选它；直到 main sleep 退出 rq，worker 成唯一候选。与全部 probe 数据自洽。
- 检查点：`fair.rs place_entity/初始 vruntime`、`activate_task ENQUEUE_WAKEUP 路径`、`pick_eevdf_entity` 的 lag 计算、`rq.min_vruntime` 基准（唤醒任务的 cfs_rq 可能错挂到别的 CPU 的 cfs_rq，min_vruntime 基准错位 → vruntime 天文数字劣势）
- 判据：dump worker 的 se.vruntime 与 rq.min_vruntime（probe 现场加打印）

### docker/mysql 楔死与此完全同构
dockerd 多线程 futex 频繁睡眠/唤醒 = 大量"唤醒任务"，同样被选择性饿死 → 43×Blocked 永不跑 → 容器死。**时钟冻结可能是并行的第二诱因或同根下游**（timer 中断处理线程也可能被饿死）。

## 二·补7：place_entity 终审数据与反常（2026-10-20 22:15，#134）

```
place: n=1     vr=0x0        avg_vr=0x100000  lag=0 vlag=0 nr_run=0   （boot 首唤醒）
place: n=20001 vr=0x172ce3fb avg_vr=0xab2711dd lag=0 vlag=0 nr_run=0   （稳态唤醒）
```

### 判读
1. **lag=0 / vlag=0 恒定**：nr_running==0（空 cfs_rq）时 PLACE_LAG 跳过（对齐 Linux）——但 vlag 恒 0 说明睡眠出队时的 `update_entity_lag` 值从未在唤醒时用到（空队列场景无补偿，本身合法）
2. **反常：vr ≠ avg_vr**（lag=0 时 `se.vruntime = avg_vruntime − 0` 后打印却不等，差 2.5G）——赋值链存在未知干预：打印参数求值顺序 / force_mut 借用重入 / 或唤醒目标 cfs_rq 与 se 原属 cfs_rq 的 min_vruntime 基准错位（跨 CPU 迁移唤醒，avg_vruntime 是新队列的而打印读到旧值窗口）。
3. 空队列唤醒实体入队后应是唯一候选必被 pick——与 probe"在 rq 不被选"矛盾 → **worker 唤醒所选 CPU 上从未发生 pick**（该 CPU 的 current 非空闲且 tick 未强制重调度），或 se 挂入的 cfs_rq 与 pick 扫描的 cfs_rq 不一致（per-cpu 实体挂错队列）

### 下轮双探针（一步定罪）
1. place 打印加 `self` 的 cpu id + `se.cfs_rq` 的 cpu id + 打印前重读 vruntime（消除求值顺序疑点）
2. pick_next_task 处打印每 cpu 的 pick 频率（节流）——确认 worker 所在 CPU 在 STUCK 期间是否根本没 pick 过


## 二·补8：双探针终审（2026-10-20 22:25，#136）

```
place: n=20001 rq_cpu=0 se_rq_cpu=0 vr=0x16bf76e5 reread=同 avg_vr=0x35a588b0 lag=0 vlag=0 nr_run=0 min_vr=0x35a588b0
pickcnt: 零输出（280s 内全系统 pick < 100000 次）
futex_stress2: 无任何输出（本轮未跑起来）
```

### 三要点
1. **挂错队证伪**：place 的 cfs_rq 与 se 所属 cfs_rq 同 CPU
2. **vr 落后 avg/min 518M**（赋值 lag=0 时 vr 应=avg 实测不等）——赋值用的 vruntime 基准（1230 行 avg_vruntime() 采样）早于打印时的 min/avg → **采样与入队间 min_vruntime 被推进（漂移）**；worker 的 vr 相对新基准领先 518M → eligible 判定 `avg >= key*load` 的 key 为负可能翻转 → 选择性饥饿的候选机制
3. **pick < 10 万次/280s**：系统整体调度活动异常低（压测本轮也未产出）

### 下轮
- 修复方向（对齐 Linux）：place_entity 的 vruntime 基准采样紧邻入队/入队前重算，消除采样-入队漂移
- 压测复跑（先查 guest 状态）

## 二·补9：探针伪象修复 + 干净代码复测（2026-10-20 22:45，#137）

### 重大澄清：#134/#136 的 vr≠avg 是探针自伤
首次加 place 探针时 `PUT 1258.=1258` **误删了赋值语句 `se.vruntime = vruntime.wrapping_sub(lag as u64)`**——vr 打印的是睡眠前残留值。恢复赋值后：
```
place: n=20001 rq_cpu=0 se_rq_cpu=0 vr=0x7ab29457 reread=同 avg_vr=同 lag=0 vlag=0 nr_run=0 min_vr=同
```
**vr == avg_vr == reread == min_vr 完全一致**——place_entity 的 vruntime 放置（含基准采样）**无罪**，二·补8 的"基准漂移"假说作废。

### 干净代码复测：STUCK 依旧（iter 1）
```
probe[0] wake=1 ftx=1 / probe[1] wake=1 ftx=0 / FINAL worker alive
```
- futex/wakeup/vruntime 放置全部无罪后，**饥饿机制最终收敛到：唤醒到某 CPU（疑 CPU1）的任务在该 CPU 上从未被 pick**（pickcnt 只见 cpu=0 打印；worker 由 select_task_rq 选往 prev_cpu/idle CPU）
- nr_run=0 + vr==min_vr（队列最优先位置）+ EEVDF 必选 ⇒ 若 CPU1 执行了 pick 必选中 worker ⇒ **CPU1 在 STUCK 期间根本没 pick**（idle halt 未被唤醒？resched IPI 未送达/未处理？或 pick 前的调度入口被卡）

### 下轮一步定罪
pick 探针改**时间窗打印**（每秒一条 per-cpu 计数，而非 10 万阈值）+ 打印 enqueue 的 target_cpu：STUCK 期间直接观察 worker 所在 CPU 的 pick 活性——零 pick 则查 idle halt 唤醒链（resched IPI 送达/处理），有 pick 则查 pick 与 worker 入队顺序竞态（enqueue 后于 pick 又被跳过一轮）。
（本轮上下文极限，数据已固化；61→62 文件 staged）

## 三、调试基建（探针资产，随修复保留）

| 探针 | 位置 | 用途 |
|------|------|------|
| critical 输出通道 | `virtio_console.rs` CRITICAL_OUTPUT_DEVICE 同步轮询 | 任何降级场景下 printk 死亡时的可见性兜底 |
| IRQ_VECTOR_COUNTS | `arch/x86_64/interrupt/handle.rs:15` | 中断注入计数 |
| tmict 四元组（全量 64 位） | `apic_timer.rs` tmict_probe | now/base/clast/raw + kreg/klast + dcr 回读 + PMTMR×TSC 定频 + REGRESS 内核侧全量单调判定 |
| pvti 四元组 | `kvm_clock.rs:kvm_pvti_debug` + tmict | system_time/tsc_timestamp/mul/shift 原始值 |
| dlprobe | `time/deadline.rs` | tick/deadline 服务分类 |
| wdtick/wdcmp | `clocksource.rs:1024/1110` | watchdog 心跳+参考名 / 比对原始 delta |
| apcal/aprecal | `apic_timer.rs` | 启动标定取证 / 运行形态重标定结果 |
| place 探针（唤醒 vruntime 放置） | `fair.rs:1260-1281` | 唤醒入队时 vr/avg/lag/vlag/min_vr/rq_cpu 对（%20000 节流）；终审已证放置无罪 |
| pick 计数探针 | `sched/mod.rs:1013-1026` | 每 CPU pick_next_task 计数（%100000 节流，下轮改时间窗） |
| LSYSCALL/TrapFrame/kret 全家桶 | `task.rs:159` + `syscall/mod.rs:70` + `task_dump.rs:121+` | 每任务最近 syscall 号（信号免疫）+ 栈帧寄存器 + 内核栈返回地址链 |
| tksetup/wdkth | `timekeeping.rs:189` / `clocksource.rs:1174` | timekeeper 安装审计 / watchdog kthread 心跳 |
| invdet | `timekeeping.rs:265` | clocksource_delta 倒退分支全量值打印 |
| 任务转储看门狗 + 'D' 热键 | `debug/task_dump.rs` | 楔死现场任务状态 |
| BIO 卡死探针 | `driver/base/block/bio.rs` | 块层提交管道活性 |

| uw（update_wall_time 审计） | `timekeeping.rs:540+` | 每 4096 次更新打印 d_raw/d_base/raw/cl/base/frac——时钟速率的内核侧权威判据（%4096 节流免疫串口缓冲伪象） |
| B 态检测+自动转储 | `task_dump.rs:32-76` | 连续窗口扫 Blocked(false)，自动产出全任务转储（dockerd 取证主武器） |
| TrapFrame rip/rsp | `task_dump.rs:109-130` | 用户态 Blocked 任务的内核栈顶帧寄存器（nm 反解得卡死 syscall） |
| pumpin 字节探针 | `virtio_console.rs:547` | 输入字节节流打印（判定 stdin 路由；实测证明本平台不走 virtio-console） |
## 四、重大认知修正（防止重走弯路，详见报告 §16/§17）

1. **R1-R8 的"kvm-clock 平滑倒走"叙事作废**：低 32 位截断伪象。全量 64 位探针证明 now_ns/base_ns 全程单调（内核侧 REGRESS 判定从未触发）。"−201ms/行"实为 +1.11s/行 经 mod 2³² 的假象。
2. **"APIC 运行期 8 倍改率"假说作废**：dcr=0x1（÷4）运行时回读恒定；标定与运行形态重测均 250MHz；cpu_khz 正确（PMTMR 交叉 3.89GHz=宿主 EPYC TSC）。
3. 已排除：RwLock 写互斥失效（CAS 正确）、除数编码错误（符合 SDM）、pvclock_scale_delta 数学错误（与 Linux 逐位一致）、mono 实例被替换（tksetup=2 仅启动两次）、timekeeper base_ns 被重置路径（全量单调）。

## 五、未解决问题

### 高优先级（2026-10-21 更新，反映五层证伪后的最新状态）

| # | 问题 | 已知证据 | 下一步 |
|---|------|---------|--------|
| **R14（三更 2026-10-21 10:10）** | **pkmain 零输出谜底**：双线程压测中 main 是 curr（`set_next_entity` 时已出队），worker 是队列唯一实体 → `nr_running==1` 恒成立，`nr>1` 探针条件注定静默（非主路径问题）。**终极嫌疑随之收敛**：pick_next_entity_with_curr 比较中 curr(main) 的 deadline 持续早于 worker → 300 次/s pick 全选 main → worker 饿死 1 秒；main sleep 后 curr 换人 worker 立即当选。指向 **EEVDF 对运行中 curr 的 deadline 刷新缺陷**（update_deadline 未在 tick/update_current 中推进，curr 永不失效） | pkmain 零输出 + pickstat 活跃 + probe 0/1 全链自洽 | 下轮：①去掉 nr>1 条件重打 winner（确认 300 pick 全是 main pid）②查 `update_current`→`update_deadline`（curr 的 deadline 是否随运行时间推进；对照 Linux fair.c update_deadline 的 `curr->deadline += ...` 语义）③修复后压测+mysql 终验 |
| — | `active_clocksource_regressed` 自旋窗 khz×1000=1s（注释写 1ms） | 代码审查 | 澄清注释并修正窗长为真 1ms |
| — | ftrace kvm_inj_virq 本环境 0 事件（tracepoint 存在、KVM 在用） | 多轮实测 | 不阻塞（uw 探针已替代），低优先 |
| — | kvm-clock 冻结检测（R13-1）的 FROZEN 实战触发未捕获（压测复测被调度饥饿掩盖） | R13-1 已编入 | R14 修复后重跑压测顺带验证 |

### dockerd 楔死链最终归因（2026-10-21 定稿）
dockerd/containerd 全家卡死 = **LSYSCALL=202(futex) 的唤醒依赖被 R14 调度饥饿锁死**：Go runtime M 线程 park 在 futex，唤醒入队后所在 CPU 不 pick → 永不运行 → 全部 goroutine 停摆。此前记录的 "dockerd B+" / tmpfs xattr 旁证 / TrapFrame 取证均为该根因的下游表象。时钟冻结（R13-1 场景）为并行的第二诱因（timer 中断处理同样可能被饥饿）。

### R12 已关闭 ✅（同源双速率之谜——真相：伪象）
"23% 慢走"系串口缓冲陈旧样本 + PMTMR 24 位双 wrap 低估的组合伪象。uw 探针（内核侧 %4096 更新节流计算）证明 base/raw/now 全程真实速率。详见"二·补"节。

### R12 终验数据（2026-10-18 21:30，mysql 负载下）

| 修复 | 内容 | 验证 |
|------|------|------|
| AP 重标定 RefCell 借用冲突 | `init_ap_apic_timer` 先 drop 借用再调 `recalibrate_apic_count_runtime` | cpu=1 aprecal 250.3MHz 正常，panic 消灭 |
| **时钟链综合终验** | uw/tmict/rawrate 三探针一致 | **886.2s@885s wall=100.1% 真实速率**；base/raw 精确同步（差 20ns/更新）；tick 250/s/CPU 稳定 15 分钟；rawrate=999.97%；tsc_hz=3.89GHz ✓ |

关键澄清：tmict 单行 `now` 在负载下可能是串口缓冲陈旧样本（判定须用 uw 探针）；TP_TOTAL 为全局计数（双 CPU 各 250/s ⇒ tot ~500/s）；PMTMR 24 位探针窗口 >4.69s 双 wrap 致 Δ 低估（tsc_hz=9.1GHz/rawrate=2337 伪象来源）。


### 独立问题

| # | 问题 | 状态 |
|---|------|------|
| F6 残留 | storm 超脏 journal 重放 panic-park（嵌套 panic 守卫生效输出 `parking`，但重放本身未通过；浅脏盘重放已通过：1 事务/5 块） | 待查重放路径 panic 源 |
| D | QEMU 内存 >2G 且非 6G 时启动挂死在 rootfs 挂载（2.5G/3.5G 实证，与脏盘无关的旧问题） | 未动，用 6G 绕过 |
| 结构性 | IRQ 独立栈（Linux IST 语义）——中断风暴时压制当前任务栈的根因之一 | 未动 |
| 次级缺口 | AIO/fallocate/sysctl/ptrace/FAT 长名/DAC 放水（报告 §4 代码证据） | 未动 |

### 部署层（内核之外）

| # | 问题 |
|---|------|
| — | Go 服务镜像（11 个）构建与验证（Dockerfile.backend 预编译模式） |
| — | 部署收尾：自启、hostfwd、文件分发 |
| — | 大镜像 registry 拉取稳定卡死（当前旁路 save/load + 传输盘） |

## 六、环境与流程备忘

- 编译：worktree 根 `make kernel`（勿在 nix devshell 编 user 程序）
- 净盘：`sudo rm -f bin/disk-image-x86_64.img && ROOTFS_MANIFEST=docker SKIP_GRUB=1 make write_diskimage`（~90s）
- 启动：`cd tools && DRAGONOS_QEMU_MEMORY=6G ROOT_PATH=<绝对> bash run-qemu.sh --bios=legacy --display=nographic`（pty 服务，Enter 激活）
- "外部杀手"已查明：共享服务器其它会话（DragonOS-pynode / DragonOS-12 工作目录）的 `pkill -9 -f qemu-system-x86_64` 清扫（journalctl 可见）；磁盘写锁残留用 `sudo fuser -k <img路径>` 清理；查活进程用 `pgrep -fc`（comm 截断会漏计）
- 每轮 agent 交付后立即 `git add -A`（防外部重置）；禁止对 worktree 大范围 git reset/checkout
- 楔死判读：串口 mtime 冻结 + vCPU 0%；tmict 行节奏须用内核侧 TSC/PMTMR 门控或 ftrace 判定（宿主侧行计数受串口突发写干扰）

## 七、2026-10-23 增补（R16 阶段收尾，mysql 已跑通）

- **验收达成**：内核 #180 净盘 + `docker load`（`LOAD_RC=0`）+ mysqld `ready for connections`（3306）；`docker exec ... 'select version();'` → `8.0.35`。官方 entrypoint 仍被 bash `<(...)` 崩溃阻断，验收用 `--entrypoint bash -c 'mysqld --initialize-insecure; exec mysqld'` 绕过。
- **futex 持锁睡眠（真缺陷）**：`futex_wait` 持全局 `futex_map` 锁做可睡眠 `copy_from_user`；修复对齐 Linux `futex_wait_setup`（锁外读、`PageFaultDisabledGuard` 复查、缺页放锁重试）。修复后 load 不再出现“全栈 futex 挂死/块 IO 停顿”。
- **探针治理**：删除锁内/关中断区 `print_critical!`（sched/fair/ipi/futex/idle/manager-sched/timekeeping/clocksource/task_dump）；tmict/pvti/dlprobe 打印改编译期开关默认关；tmict 采样基线改 per-CPU（此前双核共用导致 tsc_hz=9.1GHz/rawrate 假象）+ PMTMR 24bit 扩展；新增放锁后 rq 长临界区哨兵。
- **TLB**：去“调用者关中断时临时开中断”hack；`check_shootdown_context` 延后告警；CSD 等待按目标核 Online 状态脱身；pinpoint 到 `mm/fault.rs:1206/1223`（待修 delayed flush）。
- **panic**：idle 上下文 panic 走 `stop_this_cpu()`（Online→Dying→Dead + 关 LAPIC + 纯 hlt），消除“僵尸核永久不 ack shootdown”。
- **tmpfs xattr**：默认 `ENOSYS`→`EOPNOTSUPP_OR_ENOTSUP`（Linux 语义）。
- **诊断**：fault 现场打印 comm + rax/rdi/rsi/rdx/rsp；virtio-blk 1s 超时自检（irq/sched/run/completed）；`AddressSpace::flush_tlb_range` 补 `#[track_caller]`；QEMU 端口选择避开本机临时端口。
- **bash `<(...)` 崩溃（未决）**：连 `echo B` 都未执行；PIE 基址 0x10000 ⇒ rip 0x65451 = `initialize_flags+0x71` 的 `mov 0x8(%rdi)`，寄存器全零。下一步：`bash -c 'echo B;/bin/true;echo A'` 区分 vfork 路径 + QEMU `-d int` 追最后一次 syscall/信号返回。
- **PR 拆分**：见 `pr-split-2026-10-23.md`（分支/PR/合入状态）。

## 八、工作流变更（2026-10-23 起）

- **主 worktree 改为原子提交**：每个独立修复完成即 `git commit`（不再只 `git add`）。目的：便于后续按提交 `git cherry-pick` / `git show <sha> -- <path>` 拆分 PR，并防止外部重置丢改动。
- 当前基线：`f0afd461`（12 个原子提交，工作区干净）；其中已合入上游的组别在提交信息里标注了 PR 号。
- 提交粒度按子系统/修复点；探针治理与功能性修复分开提交，便于独立评审与回滚。
- PR 拆分与覆盖审计见 `pr-split-2026-10-23.md`。
