# DragonOS ProcFS 重构总结

## 概览

本次重构基于 Asterinas 的 procfs 实现，采用模板系统重新实现了 DragonOS 的 /proc 文件系统。新实现使用惰性加载（lazy loading）和缓存策略，提供了更好的性能和更清晰的代码结构。

## 文件结构

```
kernel/src/filesystem/procfs/
├── template/           # 模板系统核心
│   ├── mod.rs         # Common 结构和模块导出
│   ├── file.rs        # ProcFile<F> 和 FileOps trait
│   ├── dir.rs         # ProcDir<D> 和 DirOps trait
│   ├── sym.rs         # ProcSym<S> 和 SymOps trait
│   ├── builder.rs     # Builder 模式实现
│   ├── examples.rs    # 使用示例
│   └── README.md      # 模板系统文档
│
├── cmdline.rs         # /proc/cmdline
├── version.rs         # /proc/version
├── meminfo.rs         # /proc/meminfo
├── mounts.rs          # /proc/mounts
├── kmsg_file.rs       # /proc/kmsg
├── self_.rs           # /proc/self (符号链接)
├── root.rs            # /proc 根目录
│
├── pid/               # 进程相关文件
│   ├── mod.rs         # PidDirOps 实现
│   ├── status.rs      # /proc/[pid]/status
│   └── exe.rs         # /proc/[pid]/exe (符号链接)
│
└── mod.rs             # 主模块（待整合）
```

## 核心组件

### 1. 模板系统 (template/)

模板系统提供了三种基本类型：

#### ProcFile<F: FileOps>
- 用于普通文件
- 实现 `FileOps` trait 来自定义读写行为
- 支持 Builder 模式构建

#### ProcDir<D: DirOps>
- 用于目录
- 实现 `DirOps` trait 来自定义查找和列举行为
- 内置 `BTreeMap` 缓存子节点
- 支持惰性加载

#### ProcSym<S: SymOps>
- 用于符号链接
- 实现 `SymOps` trait 来自定义链接目标

### 2. 根目录实现 (root.rs)

`RootDirOps` 实现了 `/proc` 的根目录：

```rust
const STATIC_ENTRIES: &'static [(&'static str, fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>)] = &[
    ("cmdline", CmdlineFileOps::new_inode),
    ("cpuinfo", CpuInfoFileOps::new_inode),
    ("kmsg", KmsgFileOps::new_inode),
    ("meminfo", MeminfoFileOps::new_inode),
    ("mounts", MountsFileOps::new_inode),
    ("self", SelfSymOps::new_inode),
    ("version", VersionFileOps::new_inode),
];
```

**特性：**
- 静态条目表：声明式定义所有静态文件
- 动态 PID 目录：根据进程是否存在动态创建 `/proc/[pid]` 目录
- 惰性加载：只在首次访问时创建 inode
- 缓存机制：避免重复创建

### 3. 进程目录实现 (pid/)

`PidDirOps` 实现了 `/proc/[pid]` 目录：

```rust
const STATIC_ENTRIES: &'static [(&'static str, fn(&PidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>)] = &[
    ("status", |ops, parent| StatusFileOps::new_inode(ops.process_ref.clone(), parent)),
    ("exe", |ops, parent| ExeSymOps::new_inode(ops.process_ref.clone(), parent)),
    // TODO: 添加更多条目
];
```

**当前支持的文件：**
- `status`: 进程状态详细信息
- `exe`: 指向可执行文件的符号链接

**待添加：**
- `fd/`: 文件描述符目录
- `cmdline`: 进程命令行参数
- `environ`: 环境变量
- 其他...

## 已实现的文件

### 静态文件

| 文件 | 类型 | 描述 |
|------|------|------|
| `/proc/cmdline` | 文件 | 内核启动参数 |
| `/proc/version` | 文件 | 内核版本信息 |
| `/proc/meminfo` | 文件 | 系统内存信息 |
| `/proc/mounts` | 文件 | 挂载点列表 |
| `/proc/kmsg` | 文件 | 内核消息缓冲区 |
| `/proc/cpuinfo` | 文件 | CPU 信息（已有实现） |

### 符号链接

| 链接 | 目标 | 描述 |
|------|------|------|
| `/proc/self` | `[pid]` | 指向当前进程的 PID 目录 |

### 进程文件

| 文件 | 类型 | 描述 |
|------|------|------|
| `/proc/[pid]/status` | 文件 | 进程状态信息 |
| `/proc/[pid]/exe` | 符号链接 | 指向可执行文件 |

## 设计特点

### 1. 惰性加载（Lazy Loading）

- **根目录层面：**
  - 静态文件（cmdline, version等）：首次访问时创建，之后从缓存读取
  - PID 目录：根据进程存在性动态创建

- **PID 目录层面：**
  - 进程文件（status, exe等）：首次访问时创建

### 2. 缓存策略

```rust
pub struct ProcDir<D: DirOps> {
    cached_children: RwLock<BTreeMap<String, Arc<dyn IndexNode>>>,
    // ...
}
```

- 使用 `BTreeMap` 存储已创建的子节点
- `RwLock` 保证并发安全
- `lookup_child()`: 查找单个子节点，未找到则创建
- `populate_children()`: 填充所有子节点

### 3. 易失性（Volatility）

某些目录被标记为 `volatile`：

```rust
ProcDirBuilder::new(Self { process_ref }, mode)
    .parent(parent)
    .volatile()  // PID 目录是易失的
    .build()
```

**含义：**
- VFS 层不应长期缓存这些 inode
- 适用于内容动态变化的文件/目录（如进程目录）

### 4. Builder 模式

所有 inode 创建都使用 Builder 模式：

```rust
ProcFileBuilder::new(Self, ModeType::S_IRUGO)
    .parent(parent)
    .build()
    .unwrap()
```

**优点：**
- 链式调用，代码清晰
- 可选参数易于扩展
- 符合 Rust 惯用法

## 与旧实现的对比

### 旧实现 (mod.rs)

```rust
// 在文件系统初始化时创建所有文件
let meminfo_params = ProcFileCreationParams::builder()
    .parent(result.root_inode())
    .name("meminfo")
    .file_type(FileType::File)
    .mode(ModeType::S_IRUGO)
    .ftype(ProcFileType::ProcMeminfo)
    .build()
    .unwrap();
result.create_proc_file(meminfo_params).unwrap();
```

**缺点：**
- 启动时创建所有 inode，内存占用大
- 代码冗长，需要大量 builder 调用
- 文件类型通过 `ProcFileType` enum 区分，不够灵活

### 新实现 (template/)

```rust
// 声明式静态条目表
const STATIC_ENTRIES: &'static [(&'static str, fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>)] = &[
    ("meminfo", MeminfoFileOps::new_inode),
    // ...
];

// 首次访问时才创建
lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
    (f)(parent)
})
```

**优点：**
- 惰性加载，节省内存
- 声明式，代码简洁
- 每个文件独立模块，易于维护
- 类型安全，编译期检查

## 下一步工作

### 1. 完善 PID 目录

实现更多 `/proc/[pid]/` 下的文件：

- [ ] `fd/` - 文件描述符目录（动态）
- [ ] `cmdline` - 命令行参数
- [ ] `environ` - 环境变量
- [ ] `maps` - 内存映射
- [ ] `stat` - 进程统计信息

### 2. 整合到主模块

将新实现整合到 `mod.rs`，替换旧的 `ProcFS` 结构：

```rust
pub struct ProcFS {
    root_inode: Arc<dyn IndexNode>,
    super_block: RwLock<SuperBlock>,
}

impl ProcFS {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak_fs| {
            let root = RootDirOps::new_inode(None);

            Self {
                root_inode: root,
                super_block: RwLock::new(SuperBlock::new(
                    Magic::PROC_MAGIC,
                    PROCFS_BLOCK_SIZE,
                    PROCFS_MAX_NAMELEN as u64,
                )),
            }
        })
    }
}
```

### 3. 移除旧代码

清理旧的实现：

- 删除 `ProcFileType` enum
- 删除 `InodeInfo` 结构
- 删除 `ProcFSInode` 实现
- 移除 `register_pid()`/`unregister_pid()` 函数

### 4. 测试

- 测试文件读取
- 测试目录列举
- 测试符号链接
- 测试进程创建/销毁时的 PID 目录变化

## 使用示例

### 创建普通文件

```rust
#[derive(Debug)]
pub struct MyFileOps;

impl FileOps for MyFileOps {
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8],
               _data: SpinLockGuard<FilePrivateData>) -> Result<usize, SystemError> {
        let content = b"Hello from procfs!";
        // ... 拷贝数据到 buf
        Ok(bytes_read)
    }
}

// 创建 inode
let inode = ProcFileBuilder::new(MyFileOps, ModeType::S_IRUGO)
    .parent(parent)
    .build()
    .unwrap();
```

### 创建目录

```rust
#[derive(Debug)]
pub struct MyDirOps;

impl DirOps for MyDirOps {
    fn lookup_child(&self, dir: &ProcDir<Self>, name: &str)
        -> Result<Arc<dyn IndexNode>, SystemError> {
        // 查找逻辑
    }

    fn populate_children<'a>(&self, dir: &'a ProcDir<Self>)
        -> RwLockReadGuard<'a, BTreeMap<String, Arc<dyn IndexNode>>> {
        // 填充逻辑
    }
}
```

### 创建符号链接

```rust
#[derive(Debug)]
pub struct MySymOps;

impl SymOps for MySymOps {
    fn read_link(&self) -> Result<String, SystemError> {
        Ok("/path/to/target".to_string())
    }
}

let inode = ProcSymBuilder::new(MySymOps, ModeType::S_IRWXUGO)
    .parent(parent)
    .build()
    .unwrap();
```

## 总结

新的 procfs 实现具有以下优势：

1. **性能更好：** 惰性加载减少内存占用和启动时间
2. **代码更清晰：** 模板系统提供统一的代码结构
3. **易于扩展：** 添加新文件只需实现相应的 Ops trait
4. **类型安全：** 编译期类型检查，减少运行时错误
5. **符合惯例：** Builder 模式等符合 Rust 最佳实践

这为 DragonOS 提供了一个现代化、高效的 procfs 实现基础。
