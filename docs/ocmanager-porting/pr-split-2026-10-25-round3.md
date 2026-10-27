# PR 拆分记录（2026-10-25 第三轮）

## 1. 范围与基线

- **来源分支**：`ocmanager-porting-2026-10`（主 worktree `/home/sparkzky/DragonOS-ocmanager`）
- **起点**：`7d937ec0`（上一轮拆分记录 `pr-split-2026-10-25-round2.md` 之后的第一个提交）
- **本次范围**：`7d937ec0..bf76301d`（即上次拆分之后新增的全部提交）
- **拆分基线**：`origin/master` = `45754bdc`
- **独立 worktree**：`/home/sparkzky/DragonOS-pr-r20`
  （每个拆分都从 `origin/master` 新建分支，未触碰主 worktree 的工作树与提交历史）
- **要求**：每个提交带 `Signed-off-by`；每条分支 `make kernel` 与
  `cargo +nightly-2026-02-24 fmt --all -- --check` 通过后才 push + `gh pr create`

## 2. 拆分结果

| # | 修复 | 源提交 | PR | 分支 | 分支提交 | 状态 |
|---|---|---|---|---|---|---|
| 1 | OverlayFS 实现 `map_pages`（按 backing inode 页缓存完成缺页） | `eb8f7582` | [#2568](https://github.com/DragonOS-Community/DragonOS/pull/2568) | `fix/overlayfs-map-pages` | `e1d6bd61` | Open |
| 2 | ext4 不再把"写路径新块"与"有效区间预分配块"标为 unwritten | `cf2ed782` | [#2569](https://github.com/DragonOS-Community/DragonOS/pull/2569) | `fix/ext4-unwritten-extent-write` | `b0ab5744` | Open |

两个 PR 基于 `origin/master` 且**互不依赖**，可独立评审/合入。

## 3. 逐项说明

### 3.1 #2568 OverlayFS `map_pages`

**问题**：OverlayFS 没有实现 `FileSystem::map_pages`，默认实现直接 `panic!`：

- 用户态缺页上下文 → 整机 park（`do_page_fault → handle_mm_fault → do_fault →
  OverlayFS::map_pages`）；
- syscall 上下文 → panic 被 `catch_unwind` 吞掉，上层表现为
  `disk I/O error: operation not supported`（sqlite `SQLITE_IOERR_WRITE` +
  errno 95；JVM/kafka 启动同样命中）。

**根因**：OverlayFS 自身不持有页缓存（`OvlInode::page_cache()` 恒为 `None`），
数据页属于 upper/lower 的 backing inode。`mmap_effective_file()` 通常会把 VMA
换成 backing 文件，但 shared 映射仍可能挂在 overlay 文件上（如 fd 已 close、
`private_data` 变为 `Unused`，而 POSIX 允许映射继续有效）。

**改动**：

- `mm/fault.rs`：把 `filemap_map_pages` 抽成按"页缓存归属 inode"参数化的实现，
  新增 `filemap_map_pages_for_inode`；
- `overlayfs/fs.rs`：实现 `map_pages`——从 VMA 文件剥掉 `MountFSInode` 投影拿到
  `OvlInode`，用 `current_realdata_inode()` 定位 backing inode，按其页缓存映射；
  无法解析时返回 SIGBUS 并打印诊断（不再 panic）；
- `vfs/mount/mod.rs`：`VfsDentry` 增加只读 `inode()` 访问器。

**分支适配**：无冲突，直接 apply。**验证**：编译/fmt 通过；DragonOS 16 容器下
容器内 sqlite（WAL 与 DELETE 两种模式）重复写入成功、kafka/msg-etl 正常启动、
不再出现该 panic。

### 3.2 #2569 ext4 unwritten extent

**问题**：写路径 `transaction_extent_query_or_create` 与
`preallocate_range_batch` 把所有新分配块标成 unwritten，而本实现没有
"未写→已写"转换：`extent_right_spine_projection` 一旦遇到尾部 unwritten extent
就返回 ENOTSUP，于是文件第一次成长后，后续 append 永久失败
（`write(2)` → `EOPNOTSUPP`）。sqlite 的 `posix_fallocate`+`pwrite`、
JVM/kafka 的预分配后写入都会命中；`cp` 出来的副本（新 inode、written extent）正常。

**改动**：

- 写路径：新块已由 `allocate_initialized_data` 写零、随即写入真实数据 →
  直接发布为 **written**；
- `fallocate`：新增 `preallocate_range_batch_initialized(…, initialized_upto)`——
  落在"本次操作后属于文件有效数据"区间内的块（已写零）标 written；
  超出该区间（`FALLOC_FL_KEEP_SIZE` 越过 EOF）仍保持 unwritten，
  维持 e2fsck 的 inode 尺寸约束；
- `ext4/inode.rs`：mode 0 传 `end`，KEEP_SIZE 传当前 size；
- 原 `preallocate_range_batch()` 保留（全 unwritten 语义）。

**分支适配**：`extent.rs` 冲突——上游用 `try_filled_bytes(0)?` 分配零块，
保留上游写法，只去掉 `mark_unwritten()`（注释改为英文）。
**验证**：编译/fmt 通过；DragonOS 16 容器下 api→asset-manager 连续三次
`assets/add` 全部 `code:OK`（id=1/2/3），重复 uuid 返回
`UNIQUE constraint failed`，不再出现 `operation not supported`。

## 4. 跳过项及原因

| 来源提交 | 内容 | 原因 |
|---|---|---|
| `ada1c760` | 定位 EOPNOTSUPP 来源的低噪诊断（Ext4Error 创建点、journal flush 拒绝点、PageCache 写回/Loading 归属、syscall 表分派打印） | 纯取证设施，按前两轮口径留在分支，不进上游 |
| `bc2ef7b8`、`7caa7d39`、`467d39d3`、`9c492de8`、`bf76301d` | 移植过程文档（OverlayFS 记录、根因分析、验收表） | 不属于上游代码修复 |

## 5. 遗留与注意事项

1. #2569 只覆盖 journaled 挂载使用的分配路径；非 journaled 的
   `fallocate/direct.rs::Edit::Allocate` 仍发布 unwritten extent，
   本次未改（便于评审，且未在测试路径上）。
2. 修复前创建、尾部已有 unwritten extent 的旧文件仍会失败（本实现无转换路径），
   删除/重建即可；这属于数据面历史状态，不影响新写入。
3. 主 worktree 之后的提交属于下一轮拆分范围，本文未处理；
   本次未在主 worktree 上新增任何提交。
4. 本记录文件提交在 `docs/pr-split-2026-10-25-round3` 分支（基于 `origin/master`），
   已 push 到 `sparkzky`，未开 PR。

## 6. 评审结论与 master 复核（2026-10-26）

两个 PR 均被维护者（fslongjin）**跳过并关闭**：

- **#2568**：master 的 `address_space.rs` 在建 VMA 前调用 `mmap_effective_file`，
  OverlayFS 返回真实 backing `Arc<File>`，VMA 在 fd 关闭后仍持有该文件
  （同 Linux `ovl_mmap` 的 `vma_set_file(vma, realfile)`）；因此"VMA 绑 overlay
  inode"的前提在 master 上不成立。另外本方案只处理 cache-hit 页、缺 cache-miss
  与 `page_mkwrite`，且按 `current_realdata_inode()` 每次取 backing，copy-up 会
  切换既有映射的 backing，架构上不完整。
- **#2569**：master 已具备 unwritten→written 转换（`low_level.rs::write_closed`、
  `fallocate/direct.rs::direct_publish_written_block_locked`），缓冲分配在尾部
  unwritten 不能用 append pool 时会回退 eager 分配。提前把 initialized extent
  发布为 written 会破坏 payload/extent/size 的有序发布不变量，并导致 CI 用例
  `Ext4DirectRead.UnwrittenExtentUsesZeroReadSemantics` 回归（-1 ≠ 37）。

**独立复核（在 master 上实测）**：用 master（`45754bdc`）内核 #3 + OCManager
磁盘以 QEMU snapshot 模式启动：

| 检查 | 结果 |
|---|---|
| `map_pages() has not yet been implemented for filesystem: OverlayFS` | **0 次**（与维护者判断一致） |
| 容器内 sqlite（mattn/go-sqlite3，WAL）| 连续 5 次插入全部成功 |
| 容器内 `fallocate(fd, 0, 0, 64K)` | 返回 `EOPNOTSUPP`（overlayfs 未实现 fallocate），但随后的 `pwrite@0/4096/61440` **全部成功** |
| OCManager 16 容器 | 仅 7 个 Up：clickhouse `Exited(139)`（用户态 SIGSEGV）、sysdiagnose `Exited(1)`（解析 `clickhouse` 失败）、其余 9 个停在 `Created` |

结论：

1. 这两个 PR 的**前提在 master 上不成立**，跳过不会影响"OCManager 跑在 master"
   这一目标；#2569 还会引入回归，已关闭是正确的。
2. master 上 OCManager 起不来的**真正阻塞点不在**这两个 PR：clickhouse 用户态
   崩溃、依赖服务 DNS 失败、多数容器未启动，属于端口分支上其余未合入的内核改动
   （#2560/#2562/#2564 等）或 master 自身的缺陷。
3. 端口分支带到 master 时，应先 rebase 再复测；若本文记录的两个现象仍在 master
   复现，需按维护者要求提供"VMA 绕过 `mmap_effective_file`"的复现器，以及
   保持有序发布的写入/转换路径修复。
4. 另记录一个独立缺口（与上述 PR 无关）：**overlayfs 未实现 `fallocate`**，
   容器内 `fallocate` 返回 `EOPNOTSUPP`；sqlite 可容忍，但值得单独跟进。

## 7. 重新打开后的调整与验证（2026-10-26 夜）

两个 PR 被重新打开（它们对 OCManager 的运转是必要的），按评审意见重做后各做了
独立验证：

### 7.1 #2568 重做（`1ad2ac7b`，已 force-push）

放弃"在 overlay inode 上做页映射"的方案，保留"VMA 绑定真实 backing 文件"的设计：

1. `mmap_effective_file()`：overlay 文件失去 private_data（共享映射比 fd 活得久 →
   `EBADF`）时，用**原始访问标志**打开当前真实 inode 并返回该 backing 文件；
   真实 inode 只在**建映射那一刻**解析一次，copy-up 不会切换已有映射。
2. `OverlayFS::map_pages()`：万一仍有映射落到这里，返回 `VM_FAULT_SIGBUS` 并打
   一次诊断，替代 trait 默认的 `panic!`（用户态缺页上下文会停机、syscall 上下文
   会变成 `EOPNOTSUPP`）。

验证（端口分支 + 该补丁，移除旧的 map_pages 实现）：16/16 容器 Up，无
`page fault on a mapping bound to the overlay inode` 诊断，sqlite 写入与三次
`assets/add` 均成功。

### 7.2 #2569 的调查结论（建议关闭，补丁不采用）

- 端口分支缺的是 **master 已有的回退**：
  `kernel/src/filesystem/ext4/inode.rs` 把
  `create_delalloc_extent_node_pool_authorized()` 的 `Err(ENOTSUP)` 转成
  `return Ok(None)`（回退 eager 分配）。端口分支写的是
  `Err(error) => return Err(SystemError::from(error).into())`，于是
  "尾部 unwritten extent"导致的池构建失败直接冒泡成 `EOPNOTSUPP`。
- 实验证明：在端口分支上**回退本 PR 的 ext4 补丁**、只加上述 7 行回退，
  启动 16 容器后连续三次 `assets/add` 全部 `code:OK`（id=1/2/3），
  无 `operation not supported`。即：应用侧故障由该回退完全修好，ext4 补丁不需要。
- master 侧复核：容器内 `fallocate(mode 0)` 在 ext4 bind mount 上成功、
  随后 `pwrite` 全部成功；sqlite 连续 5 次插入成功；未出现 overlay 缺页 panic。
- 遗留候选（需要可复现基线才动手）：提交时 `RetryableNotPublished(ENOTSUP)`
  （池建立时尾部干净、之后 `fallocate` 引入 unwritten 尾）仍需"退役池 + 回退 eager"，
  涉及 `DelallocAppendMapperAuthority` 的传递与有序发布（payload → extent → size）。

### 7.3 对端口分支的最小修复清单

1. 补 `Err(ENOTSUP) => return Ok(None)` 回退（7 行，直接对齐 master）；
2. 采用 #2568 的新写法（或直接 rebase 到含 #2446/#2449/#2452 的 master）；
3. 之后 OCManager 全栈可在不改动 ext4 语义的前提下运行。

### 7.4 重新开 PR（2026-10-26 深夜）

- #2569：确认非必要，已由维护者关闭；端口分支只需补 master 的 7 行回退。
- #2568：原 PR 在旧版本上被关闭，改版后另开 **#2572
  （`fix/overlayfs-map-pages`，`1ad2ac7b`，基于 `origin/master` = `0cf86938`）**，
  并在旧 PR 下留了指向评论。新 PR 保留"VMA 绑定真实 backing 文件"的设计，
  去掉在 overlay inode 上做页映射的方案，附带端口侧验证
  （16/16 容器、无 panic、sqlite 与三次资产写入成功）。
