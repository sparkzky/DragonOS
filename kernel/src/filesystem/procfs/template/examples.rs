//! 示例：如何使用 ProcFS Template 系统
//!
//! 这个文件展示了如何使用 template 系统创建 procfs 文件、目录和符号链接。

use crate::filesystem::{
    procfs::template::{
        DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFile, ProcFileBuilder, ProcSym,
        ProcSymBuilder, SymOps,
    },
    vfs::{
        syscall::ModeType, vcore::generate_inode_id, FilePrivateData, FileSystem, IndexNode,
        Metadata,
    },
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

// ============================================================================
// 示例 1: 简单的只读文件 (/proc/example_file)
// ============================================================================

/// ExampleFileOps 实现一个简单的只读文件
#[derive(Debug)]
struct ExampleFileOps {
    content: &'static str,
}

impl ExampleFileOps {
    fn new(content: &'static str) -> Self {
        Self { content }
    }

    /// 创建 inode 的辅助函数
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(
            Self::new("Hello from ProcFS template system!\n"),
            ModeType::S_IRUGO, // 所有用户可读
        )
        .parent(parent)
        .build()
        .unwrap()
    }
}

impl FileOps for ExampleFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content_bytes = self.content.as_bytes();

        // 如果偏移量超出内容长度，返回 0（EOF）
        if offset >= content_bytes.len() {
            return Ok(0);
        }

        // 计算实际要复制的字节数
        let remaining = content_bytes.len() - offset;
        let copy_len = core::cmp::min(len, remaining);
        let copy_len = core::cmp::min(copy_len, buf.len());

        // 复制数据到缓冲区
        buf[..copy_len].copy_from_slice(&content_bytes[offset..offset + copy_len]);

        Ok(copy_len)
    }

    // write_at 使用默认实现（返回 EPERM）
}

// ============================================================================
// 示例 2: 可写文件（使用内部状态）
// ============================================================================

use crate::libs::spinlock::SpinLock;

/// WritableFileOps 实现一个可读写的文件
#[derive(Debug)]
struct WritableFileOps {
    data: SpinLock<Vec<u8>>,
}

impl WritableFileOps {
    fn new() -> Self {
        Self {
            data: SpinLock::new(Vec::new()),
        }
    }

    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(
            Self::new(),
            ModeType::S_IRUGO | ModeType::S_IWUSR, // 所有用户可读，owner 可写
        )
        .parent(parent)
        .build()
        .unwrap()
    }
}

impl FileOps for WritableFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let data = self.data.lock();

        if offset >= data.len() {
            return Ok(0);
        }

        let remaining = data.len() - offset;
        let copy_len = core::cmp::min(len, remaining);
        let copy_len = core::cmp::min(copy_len, buf.len());

        buf[..copy_len].copy_from_slice(&data[offset..offset + copy_len]);

        Ok(copy_len)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mut data = self.data.lock();

        // 如果需要，扩展缓冲区
        if offset + len > data.len() {
            data.resize(offset + len, 0);
        }

        let copy_len = core::cmp::min(len, buf.len());
        data[offset..offset + copy_len].copy_from_slice(&buf[..copy_len]);

        Ok(copy_len)
    }
}

// ============================================================================
// 示例 3: 符号链接 (/proc/self)
// ============================================================================

/// SelfSymOps 实现 /proc/self 符号链接
#[derive(Debug)]
struct SelfSymOps;

impl SelfSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self, ModeType::S_IRUGO | ModeType::S_IWUGO | ModeType::S_IXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for SelfSymOps {
    fn read_link(&self) -> Result<String, SystemError> {
        // 返回当前进程的 PID
        use crate::process::ProcessManager;
        let pid = ProcessManager::current_pcb().pid();
        Ok(pid.to_string())
    }
}

// ============================================================================
// 示例 4: 目录（带静态表）
// ============================================================================

use crate::libs::rwlock::RwLockReadGuard;

/// ExampleDirOps 实现一个简单的目录
#[derive(Debug)]
struct ExampleDirOps;

impl ExampleDirOps {
    /// 静态条目表 - 定义目录中的所有静态文件
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("example_file", ExampleFileOps::new_inode),
        ("writable_file", WritableFileOps::new_inode),
        ("self", SelfSymOps::new_inode),
    ];

    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, ModeType::S_IRUGO | ModeType::S_IXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for ExampleDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        use crate::filesystem::procfs::template::lookup_child_from_table;

        let mut cached_children = dir.cached_children().write();

        // 在静态表中查找
        if let Some(child) =
            lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
                (f)(dir.self_ref_weak().clone())
            })
        {
            return Ok(child);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children<'a>(
        &self,
        dir: &'a ProcDir<Self>,
    ) -> RwLockReadGuard<'a, BTreeMap<String, Arc<dyn IndexNode>>> {
        use crate::filesystem::procfs::template::populate_children_from_table;

        let mut cached_children = dir.cached_children().write();

        // 填充所有静态条目
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        });

        cached_children.downgrade()
    }
}

// ============================================================================
// 使用示例
// ============================================================================

/// 如何在 procfs 根目录中注册这些示例
///
/// ```rust,ignore
/// // 在 RootDirOps 的 STATIC_ENTRIES 中添加：
/// const STATIC_ENTRIES: &'static [(
///     &'static str,
///     fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
/// )] = &[
///     // ... 其他条目 ...
///     ("examples", ExampleDirOps::new_inode),
/// ];
/// ```
