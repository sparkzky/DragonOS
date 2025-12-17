# ProcFS 迁移指南

本文档说明如何将新的模板化 procfs 实现集成到 DragonOS 中。

## 迁移步骤

### 1. 更新模块声明

在 `kernel/src/filesystem/procfs/mod.rs` 中添加新模块：

```rust
// 新增模块
pub mod cmdline;
pub mod version;
pub mod meminfo;
pub mod mounts;
pub mod kmsg_file;
pub mod self_;
pub mod root;
pub mod pid;

// template 模块已经存在
pub mod template;
```

### 2. 简化 ProcFS 结构

删除或注释掉旧的实现，创建新的 ProcFS 结构：

```rust
use super::vfs::{FileSystem, FsInfo, IndexNode, Magic, SuperBlock};
use crate::libs::rwlock::RwLock;
use alloc::sync::Arc;

/// procfs 文件系统
pub struct ProcFS {
    /// procfs 的 root inode
    root_inode: Arc<dyn IndexNode>,
    super_block: RwLock<SuperBlock>,
}

/// procfs 的 magic number
const PROC_MAGIC: u64 = 0x9fa0;
/// procfs 的块大小
const PROCFS_BLOCK_SIZE: u64 = 512;
/// procfs inode 名称的最大长度
const PROCFS_MAX_NAMELEN: usize = 64;

impl ProcFS {
    pub fn new() -> Arc<Self> {
        let root = root::RootDirOps::new_inode(None);

        Arc::new(Self {
            root_inode: root,
            super_block: RwLock::new(SuperBlock::new(
                Magic::PROC_MAGIC,
                PROCFS_BLOCK_SIZE,
                PROCFS_MAX_NAMELEN as u64,
            )),
        })
    }
}

impl FileSystem for ProcFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: PROCFS_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "procfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }
}
```

### 3. 简化初始化函数

旧的 `procfs_init()` 函数会创建所有文件，新实现中这是不需要的：

```rust
pub fn procfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        info!("Initializing ProcFS...");
        // 创建 procfs 实例
        let procfs: Arc<ProcFS> = ProcFS::new();
        let root_inode = ProcessManager::current_mntns().root_inode();

        // procfs 挂载
        let mntfs = root_inode
            .mkdir("proc", ModeType::from_bits_truncate(0o755))
            .expect("Unable to find /proc")
            .mount(procfs, MountFlags::empty())
            .expect("Failed to mount at /proc");

        let ino = root_inode.metadata().unwrap().inode_id;
        let mount_path = Arc::new(MountPath::from("/proc"));
        ProcessManager::current_mntns()
            .add_mount(Some(ino), mount_path, mntfs)
            .expect("Failed to add mount for /proc");

        info!("ProcFS mounted.");
        result = Some(Ok(()));
    });

    result.unwrap()
}
```

**注意：** 不再需要在初始化时调用 `create_proc_file()` 创建文件，文件会在首次访问时自动创建。

### 4. 删除不再需要的代码

以下代码可以安全删除：

#### a. 删除 ProcFileType enum

```rust
// ❌ 不再需要
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcFileType {
    ProcStatus = 0,
    ProcMeminfo = 1,
    // ...
}
```

#### b. 删除 ProcFileCreationParams

```rust
// ❌ 不再需要
#[derive(Debug, Clone)]
pub struct ProcFileCreationParams<'a> {
    // ...
}
```

#### c. 删除 InodeInfo

```rust
// ❌ 不再需要
#[derive(Debug)]
pub struct InodeInfo {
    pid: Option<RawPid>,
    ftype: ProcFileType,
    fd: i32,
}
```

#### d. 删除旧的 ProcFSInode 实现

```rust
// ❌ 整个 LockedProcFSInode 和 ProcFSInode 实现都可以删除
```

#### e. 删除 register_pid/unregister_pid

```rust
// ❌ 不再需要这些函数
pub fn procfs_register_pid(pid: RawPid) -> Result<(), SystemError> { ... }
pub fn procfs_unregister_pid(pid: RawPid) -> Result<(), SystemError> { ... }
```

新实现中，进程目录会在首次访问时动态创建，不需要显式注册。

### 5. 更新其他模块的调用

#### 进程创建时

**旧代码：**
```rust
// 在进程创建时
procfs_register_pid(pid)?;
```

**新代码：**
```rust
// 不需要任何操作！
// 进程目录会在首次访问 /proc/[pid] 时自动创建
```

#### 进程退出时

**旧代码：**
```rust
// 在进程退出时
procfs_unregister_pid(pid)?;
```

**新代码：**
```rust
// 不需要任何操作！
// 缓存会在下次 readdir 时自动清理
// 如果需要立即清理，可以实现 Observer 模式（参考 Asterinas）
```

### 6. 保留的模块

以下模块仍然需要保留和更新：

#### a. kmsg 模块 (kmsg.rs)

保留 `Kmsg` 结构和 `KMSG` 全局变量，但使用新的 `kmsg_file.rs` 作为文件接口。

#### b. log 模块 (log.rs)

保留日志相关功能。

#### c. proc_cpuinfo (proc_cpuinfo.rs)

已经实现了 `FileOps` trait，可以直接使用。

### 7. 可选：实现进程退出观察者

如果需要在进程退出时立即清理缓存，可以在 `root.rs` 中实现：

```rust
// 在 Asterinas 中的实现方式
use crate::events::Observer;
use crate::process::{process_table::PidEvent};

impl Observer<PidEvent> for ProcDir<RootDirOps> {
    fn on_events(&self, events: &PidEvent) {
        let PidEvent::Exit(pid) = events;

        let mut cached_children = self.cached_children().write();
        cached_children.remove(&pid.to_string());
    }
}

// 在 RootDirOps::new_inode() 中注册观察者
let weak_ptr = Arc::downgrade(&root_inode);
process_exit_events().register_observer(weak_ptr);
```

### 8. 编译检查

完成迁移后，运行以下命令检查：

```bash
cd kernel
cargo check
cargo clippy
```

### 9. 功能测试

测试以下功能：

```bash
# 在 DragonOS 中
cat /proc/version
cat /proc/meminfo
cat /proc/cmdline
cat /proc/mounts
cat /proc/self  # 应该是一个符号链接
ls /proc/  # 应该列出所有进程和静态文件
cat /proc/1/status  # 查看进程 1 的状态
ls -l /proc/1/exe  # 应该显示符号链接
```

## 迁移清单

- [ ] 添加新模块声明
- [ ] 创建新的 ProcFS 结构
- [ ] 简化 procfs_init()
- [ ] 删除 ProcFileType enum
- [ ] 删除 ProcFileCreationParams
- [ ] 删除 InodeInfo
- [ ] 删除旧的 ProcFSInode 实现
- [ ] 删除 register_pid/unregister_pid
- [ ] 移除进程创建/退出时的 procfs 调用
- [ ] （可选）实现进程退出观察者
- [ ] 运行编译检查
- [ ] 进行功能测试

## 常见问题

### Q: 旧代码可以删除吗？

A: 建议先保留旧代码（通过条件编译或注释），在新实现完全稳定后再删除：

```rust
#[cfg(not(feature = "new_procfs"))]
mod old_impl {
    // 旧的实现
}

#[cfg(feature = "new_procfs")]
mod new_impl {
    // 新的实现
}
```

### Q: 性能有提升吗？

A: 是的，新实现有以下优势：
- **启动更快：** 不在启动时创建所有 inode
- **内存占用更少：** 惰性加载，只创建访问过的 inode
- **更好的缓存：** BTreeMap 缓存避免重复创建

### Q: 如何添加新的 proc 文件？

A: 参考 `cmdline.rs` 或 `version.rs` 的实现：

1. 创建新文件，例如 `myfile.rs`
2. 实现 `FileOps` trait
3. 在 `root.rs` 的 `STATIC_ENTRIES` 中添加条目
4. 在 `mod.rs` 中添加模块声明

### Q: 如何调试？

A: 在 `template/dir.rs` 中添加日志：

```rust
fn lookup_child(&self, dir: &ProcDir<Self>, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    info!("ProcFS: Looking up child: {}", name);
    // ...
}
```

## 参考资料

- [Asterinas procfs 实现](https://github.com/asterinas/asterinas/tree/main/kernel/src/fs/procfs)
- [Linux procfs 文档](https://www.kernel.org/doc/html/latest/filesystems/proc.html)
- `kernel/src/filesystem/procfs/template/README.md` - 模板系统文档
- `kernel/src/filesystem/procfs/REFACTOR_SUMMARY.md` - 重构总结
