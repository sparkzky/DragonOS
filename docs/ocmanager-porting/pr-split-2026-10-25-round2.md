# PR 拆分记录（2026-10-25 第二轮）

## 1. 范围与基线

- **来源分支**：`ocmanager-porting-2026-10`（主 worktree `/home/sparkzky/DragonOS-ocmanager`）
- **上一轮拆分记录**：`pr-split-2026-10-25.md`（#2558/#2559/#2560/#2561，基线 `3120463d`）
- **本次范围**：`3120463d..` 之后新增的内核改动
- **拆分基线**：`origin/master` = `45754bdc`
- **独立 worktree**：`/home/sparkzky/DragonOS-pr-r19`（每个拆分从 `origin/master` 新建分支）
- **要求**：每个提交带 `Signed-off-by`；每个分支 `make kernel` 与
  `cargo +nightly-2026-02-24 fmt --all -- --check` 均通过后才 push + `gh pr create`
- 主 worktree 只读：未改动其工作树与提交历史

## 2. 拆分结果

| # | 修复 | 源提交 | PR | 分支 | 状态 |
|---|---|---|---|---|---|
| 1 | ext4 `stat` 的 delalloc 全表扫描改为按 inode 索引 | `9c7040b1`（ext4 部分） | [#2563](https://github.com/DragonOS-Community/DragonOS/pull/2563) | `fix/ext4-stat-delalloc-scan` | Open |
| 2 | CSD 发布无锁化 + 任意关中断自旋点服务入站 shootdown | `162d51c5` + `713fd588` + `85cf41cf` | [#2564](https://github.com/DragonOS-Community/DragonOS/pull/2564) | `fix/mm-tlb-shootdown-lock-free-csd` | **Draft**（依赖 #2560 / #2562） |

各分支头（本地 = `sparkzky` 远端）：

| 分支 | 提交 |
|---|---|
| `fix/ext4-stat-delalloc-scan` | `4ab93b27` |
| `fix/mm-tlb-shootdown-lock-free-csd` | `de699122` |

## 3. 逐项说明

### 3.1 #2563 ext4 delalloc 按 inode 索引

`stat_file_attr()` 求文件的延迟分配字节数时遍历**全部**活跃 claim 再按 inode 过滤，
复杂度 O(claims)；容器启动（镜像解包 + 大量 stat）期间 claim 数上万，单次 stat 被拖到
毫秒级。

改动：在 claim 表旁维护 `(inode, generation, reservation) -> data_blocks` 索引
（`AllocationState::inode_delalloc_data`，`BTreeMap`），在 reserve / consume / rollback /
release 共 6 处同步（新增 `forget_claim_owner`、`sync_claim_owner_blocks`）；stat 改为对
该 inode 的键区间做 `range(start..=end)` 查询。未绑定 inode 的 claim 仍不计入，保持原
过滤语义。

验证：`make kernel` 通过；fmt 通过；容器栈启动不再出现分钟级停顿。

### 3.2 #2564 CSD 无锁化 + 自旋点服务入站 flush

**问题一（协议）**：`CsdFlushTlb.info` 用 `SpinLock` 保存进行中请求，IPI handler 与
关中断服务路径都必须先取锁才能应答。给 `SpinLock` 争用循环加「记录争用锁地址」诊断后，
实测死锁现场争用地址落在 per-CPU/CSD 堆区（`0xffff800340007020`）、累计 831 万次争用，
而持锁者字段恒为 0——持有者不可归属（guard 泄漏或相邻内存越界）。对一个绝不能阻塞的
协议而言，锁本身就是错误的媒介。

**问题二（liveness）**：在任意关中断自旋锁上等待的 CPU 无法应答发往它的 shootdown；
若发起方恰好持有该等待者需要的锁，就形成整机挂死。

改动：

- `CsdFlushTlb` 改为 `AtomicPtr<FlushTlbInfo>` + `AtomicBool done`；
- 发布方**先置 `done=false`，再写指针**（顺序不可颠倒：若先写指针，迟到 IPI 可能取走
  指针并 ack，随后 `done` 被清零 → 本次请求永久丢 ack），然后 `compiler_fence(SeqCst)`、
  发 IPI；
- 接收方 `swap(null)` 一次性取走指针，**只有取到的一方**执行失效并置 `done`；嵌套的
  IPI/服务路径见到 null 直接返回，不会解引用发起方可能已释放的栈帧；
- 新增 `libs::spinlock_hook`：`SpinLock::lock()` / `lock_irqsave()` 的争用循环每 1024 次
  调用回调（无争用不进快路径），mm 注册的回调就地服务入站 shootdown，并带递归保护
  （服务路径自身可能再次进入锁等待）。

相对分支实现的适配（并入 PR 时已处理）：

| 分支上的内容 | 处理 |
|---|---|
| `TLB_CSD_HOLDER`、`SPIN_LOCK_ADDR`、`SPIN_CONTENDED` 诊断静态量 | **不进 PR**（纯取证设施） |
| `mm::tlb::service_inbound_flush_from_spin()`（给 sched 的 rq 自锁用） | **不进 PR**：上游 `CpuRunQueue::self_lock()` 用的就是通用 `SpinLock::lock_irqsave()`，已被本次钩子覆盖，无需专用入口 |
| handler 内的 `mm_selftest` observer 回调 | **保留上游逻辑**，把 `take()`/`done` 协议套在 observer 之外，不改变 selftest 观测点 |
| `flush_tlb_multi` 发布循环后的 `compiler_fence(SeqCst)` | 保留（发布与 IPI 之间禁止编译器重排） |

依赖链：本 PR 基于 #2560（其本身基于 #2562），分支上后 3 个提交属于 #2560/#2562，
**只有 `de699122` 是新增内容**；故先以 Draft 提交，等 #2560 合入后 rebase 即可。

验证：`make kernel` 通过；fmt 通过；16 容器 OCManager 全栈 + 120 次并发 `docker exec`
压测无挂死、无 `CpuRunQueue::self_lock spinout` panic、无 TLB ack 超时（内核 #290）。

## 4. 跳过项及原因

| 来源提交 | 内容 | 原因 |
|---|---|---|
| `9c7040b1`（sched 部分） | `rq_nr_running` 改用 `try_nr_running()` 非阻塞读 | **上游已等价**：`CpuRunQueue::nr_running` 已是 `AtomicUsize`，`nr_running()` 为无锁读，`rq_nr_running()` 直接调用它；原本的自旋 panic 前提不存在 |
| `85cf41cf`（sched 部分） | 在 `CpuRunQueue::self_lock` 的自定义 `try_lock` 循环里服务入站 | 上游 `self_lock()` 走通用 `SpinLock::lock_irqsave()`，已被 #2564 的争用钩子覆盖；分支的自定义循环只存在于 vendor 分支 |
| `c622dbc7` | 记录返回 EOPNOTSUPP 的系统调用（前 200 条） | 纯诊断设施，按上一轮口径留在分支 |
| `162d51c5` / `713fd588` 的诊断字段 | `TLB_CSD_HOLDER` / `SPIN_LOCK_ADDR` / `SPIN_CONTENDED` | 同上；其**功能性**部分（spinlock_hook）已并入 #2564 |
| `82c57ba3`、`d31142c1`、`9be5ba16`、`bd876cae`、`bc224c5e` | 移植过程文档 | 不属于上游代码修复 |

## 5. 遗留与注意事项

1. **合并顺序**：#2564 依赖 #2560（→ #2562），合入后需 rebase 并去掉 Draft；#2563 独立。
2. **上一轮仍有 4 个 PR 未合入**（#2558/#2559/#2560/#2561），本轮未触碰它们的分支。
3. **主 worktree 后续提交**属于下一轮拆分范围，本文档不处理；本轮未在主 worktree 上新增
   任何提交。
4. 本记录文件提交在 `docs/pr-split-2026-10-25-round2` 分支（基于 `origin/master`），
   未开 PR、未改动主 worktree。
