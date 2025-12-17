# DragonOS ProcFS Template 系统

本 template 系统参考了 Asterinas 的设计，为 DragonOS 的 procfs 提供了一套基于泛型和 trait 的模板框架，大幅简化 procfs 文件、目录和符号链接的实现。

## 设计目标

1. **减少代码重复**：所有 procfs inode 共享相同的框架代码
2. **类型安全**：使用泛型和 trait，编译时检查
3. **灵活定制**：每个文件/目录只需实现特定的 Ops trait
4. **统一接口**：对 VFS 层暴露统一的 IndexNode trait 实现
5. **懒加载 + 缓存**：inode 在第一次访问时创建并缓存

## 核心架构

```
┌──────────────────────────────────────────────────────────────┐
│  Template Layer (泛型框架)                                    │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ ProcFile<F>  │  │ ProcDir<D>   │  │ ProcSym<S>   │      │
│  │              │  │              │  │              │      │
│  │ inner: F     │  │ inner: D     │  │ inner: S     │      │
│  │ common       │  │ common       │  │ common       │      │
│  │ ...          │  │ cached_...   │  │ ...          │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                 │                 │              │
│         ↓                 ↓                 ↓              │
│  ┌──────────────────────────────────────────────────┐      │
│  │ Common (共享元数据和行为)                         │      │
│  │ - metadata: RwLock<Metadata>                    │      │
│  │ - fs: Weak<dyn FileSystem>                      │      │
│  │ - is_volatile: bool                             │      │
│  └──────────────────────────────────────────────────┘      │
└──────────────────────────────────────────────────────────────┘
         ↑                 ↑                 ↑
         requires          requires          requires
         │                 │                 │
┌────────┴────────┐ ┌──────┴───────┐ ┌──────┴───────┐
│ FileOps trait   │ │ DirOps trait │ │ SymOps trait │
├─────────────────┤ ├──────────────┤ ├──────────────┤
│ read_at()       │ │ lookup_child │ │ read_link()  │
│ write_at()      │ │ populate_... │ │              │
└─────────────────┘ │ validate_... │ └──────────────┘
                    └──────────────┘
```

## 核心组件

### 1. Common - 共享基础设施

所有 procfs inode 通过 `Common` 结构体共享：
- 元数据管理（inode 号、权限、所有者、时间戳）
- 文件系统引用
- Volatile 标志（控制 VFS 层 dentry cache）

### 2. 泛型包装器

#### ProcFile<F: FileOps>
- 文件的泛型包装器
- 将 FileOps trait 的实现适配到 IndexNode trait

#### ProcDir<D: DirOps>
- 目录的泛型包装器
- 内置子节点缓存（BTreeMap）
- 支持懒加载和静态表

#### ProcSym<S: SymOps>
- 符号链接的泛型包装器
- 实现符号链接的读取功能

### 3. Ops Trait - 定制点

#### FileOps
```rust
pub trait FileOps: Sync + Send + Sized + Debug {
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8], ...) -> Result<usize, SystemError>;
    fn write_at(&self, offset: usize, len: usize, buf: &[u8], ...) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)  // 默认只读
    }
}
```

#### DirOps
```rust
pub trait DirOps: Sync + Send + Sized + Debug {
    fn lookup_child(&self, dir: &ProcDir<Self>, name: &str) -> Result<Arc<dyn IndexNode>, SystemError>;
    fn populate_children<'a>(&self, dir: &'a ProcDir<Self>) -> RwLockReadGuard<'a, BTreeMap<...>>;
    fn validate_child(&self, _child: &dyn IndexNode) -> bool { true }
}
```

#### SymOps
```rust
pub trait SymOps: Sync + Send + Sized + Debug {
    fn read_link(&self) -> Result<String, SystemError>;
}
```

### 4. Builder 模式

提供链式调用构造 inode：

```rust
ProcFileBuilder::new(MyFileOps::new(), ModeType::S_IRUGO)
    .parent(parent_weak)
    .volatile()  // 可选：标记为 volatile
    .build()?
```

### 5. 静态表模式

声明式定义目录结构：

```rust
impl MyDirOps {
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("file1", File1Ops::new_inode),
        ("file2", File2Ops::new_inode),
        ("subdir", SubDirOps::new_inode),
    ];
}
```

## 使用示例

### 简单只读文件

```rust
#[derive(Debug)]
struct MyCmdlineOps;

impl FileOps for MyCmdlineOps {
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8], _data: ...) -> Result<usize, SystemError> {
        let content = "kernel cmdline here\n";
        let bytes = content.as_bytes();

        if offset >= bytes.len() { return Ok(0); }

        let copy_len = core::cmp::min(len, bytes.len() - offset);
        buf[..copy_len].copy_from_slice(&bytes[offset..offset + copy_len]);
        Ok(copy_len)
    }
}

impl MyCmdlineOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, ModeType::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}
```

### 符号链接

```rust
#[derive(Debug)]
struct MySelfSymOps;

impl SymOps for MySelfSymOps {
    fn read_link(&self) -> Result<String, SystemError> {
        use crate::process::ProcessManager;
        let pid = ProcessManager::current_pcb().pid();
        Ok(pid.to_string())
    }
}

impl MySelfSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self, ModeType::S_IRWXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}
```

### 目录（带静态表）

```rust
#[derive(Debug)]
struct MyRootDirOps;

impl MyRootDirOps {
    const STATIC_ENTRIES: &'static [(...)] = &[
        ("cmdline", MyCmdlineOps::new_inode),
        ("self", MySelfSymOps::new_inode),
        // ... 更多条目
    ];
}

impl DirOps for MyRootDirOps {
    fn lookup_child(&self, dir: &ProcDir<Self>, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        use crate::filesystem::procfs::template::lookup_child_from_table;

        let mut cached = dir.cached_children().write();

        if let Some(child) = lookup_child_from_table(name, &mut cached, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        }) {
            return Ok(child);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children<'a>(&self, dir: &'a ProcDir<Self>) -> RwLockReadGuard<'a, ...> {
        use crate::filesystem::procfs::template::populate_children_from_table;

        let mut cached = dir.cached_children().write();
        populate_children_from_table(&mut cached, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        });

        cached.downgrade()
    }
}
```

## 懒加载机制

### 工作原理

1. **lookup 时**：
   - 先查缓存（`cached_children`）
   - 缓存未命中时调用 `DirOps::lookup_child()`
   - 在 `lookup_child` 中使用 `lookup_child_from_table()`
   - `lookup_child_from_table()` 内部使用 `BTreeMap::entry().or_insert_with()`
   - 只有在缓存中找不到时才调用构造函数

2. **readdir 时**：
   - 调用 `DirOps::populate_children()`
   - 使用 `populate_children_from_table()` 填充所有静态条目
   - 同样只在缓存中不存在时才创建

### 缓存失效

对于动态内容（如 `/proc/[pid]`），可以：
- 使用 `validate_child()` 检查缓存有效性
- 标记为 `volatile`，避免 VFS 层缓存
- 实现 Observer 模式，在进程退出时清理缓存

## 文件列表

```
template/
├── mod.rs           # 模块入口，定义 Common，导出所有类型
├── builder.rs       # Builder 模式实现
├── dir.rs           # ProcDir 和 DirOps 实现
├── file.rs          # ProcFile 和 FileOps 实现
├── sym.rs           # ProcSym 和 SymOps 实现
├── util.rs          # SlotVec 工具（从 Asterinas 移植）
├── examples.rs      # 使用示例
└── README.md        # 本文档
```

## 优势

1. **代码复用率极高**
   - 元数据管理：100% 复用（通过 Common）
   - 缓存逻辑：100% 复用（通过 ProcDir）
   - 静态表处理：100% 复用（通过辅助函数）
   - IndexNode trait 实现：90%+ 复用（通过 `#[inherit_methods]`）

2. **类型安全**
   - 泛型确保每个 inode 都有对应的 Ops 实现
   - 编译时检查，不会出现运行时类型错误

3. **易于扩展**
   - 添加新的 procfs 文件只需：
     1. 实现 FileOps/DirOps/SymOps trait
     2. 在静态表中添加一行
     3. 不需要修改框架代码

4. **性能优化**
   - 懒加载：避免不必要的对象创建
   - 缓存：避免重复的查找和创建
   - 静态表：编译时确定，无运行时开销

## 下一步

1. 将现有的 procfs 文件迁移到 template 系统
2. 实现进程相关的动态目录（`/proc/[pid]`）
3. 添加 Observer 模式支持进程退出时的缓存清理
4. 性能测试和优化

## 参考

- Asterinas ProcFS 实现：`/home/sparkzky/asterinas/kernel/src/fs/procfs/`
- Asterinas 设计文档：`/home/sparkzky/asterinas/.claude/proc.md`
