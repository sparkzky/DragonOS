use crate::{
    filesystem::{
        procfs::{
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            Builder,
        },
        vfs::{syscall::ModeType, IndexNode},
    },
    libs::rwlock::RwLockReadGuard,
    process::{ProcessControlBlock, RawPid},
};
use alloc::{
    collections::BTreeMap,
    string::{String,ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

mod exe;
mod fd;
mod status;

use exe::ExeSymOps;
use fd::FdDirOps;
use status::StatusFileOps;

/// /proc/[pid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct PidDirOps {
    // 使用弱引用，避免循环引用
    process_ref: Weak<ProcessControlBlock>,
    // 缓存 PID，以备弱引用失效时使用
    pid: RawPid,
}

impl PidDirOps {
    pub fn new_inode(
        process_ref: Arc<ProcessControlBlock>,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        let pid = process_ref.raw_pid();
        ProcDirBuilder::new(
            Self {
                process_ref: Arc::downgrade(&process_ref),
                pid,
            },
            ModeType::from_bits_truncate(0o555),
        )
        .parent(parent)
        .volatile() // PID 目录是易失的，因为它们与特定进程关联
        .build()
        .unwrap()
    }

    /// 获取进程引用，如果进程已经退出则返回 None
    fn get_process(&self) -> Option<Arc<ProcessControlBlock>> {
        self.process_ref.upgrade()
    }

    /// 静态条目表
    /// 包含 /proc/[pid] 目录下的所有静态文件和目录
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(&PidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        // fd 目录现在通过 lookup_child 懒加载，不在这里列出
        ("status", |ops, parent| {
            // 尝试获取进程引用，如果进程已退出则创建空文件
            if let Some(process) = ops.get_process() {
                StatusFileOps::new_inode(process, parent)
            } else {
                // 进程已退出，返回一个空的占位符 inode
                // 这里可以返回一个错误文件或空文件
                use crate::filesystem::procfs::template::{FileOps, ProcFileBuilder};

                #[derive(Debug)]
                struct EmptyFileOps;
                impl FileOps for EmptyFileOps {
                    fn read_at(
                        &self,
                        _offset: usize,
                        _len: usize,
                        _buf: &mut [u8],
                        _data: crate::libs::spinlock::SpinLockGuard<
                            crate::filesystem::vfs::FilePrivateData,
                        >,
                    ) -> Result<usize, SystemError> {
                        Ok(0) // 返回空内容
                    }
                }

                ProcFileBuilder::new(EmptyFileOps, ModeType::S_IRUGO)
                    .parent(parent)
                    .build()
                    .unwrap()
            }
        }),
        ("exe", |ops, parent| {
            if let Some(process) = ops.get_process() {
                ExeSymOps::new_inode(process, parent)
            } else {
                // 进程已退出，返回空符号链接
                use crate::filesystem::procfs::template::{ProcSymBuilder, SymOps};

                #[derive(Debug)]
                struct EmptySymOps;
                impl SymOps for EmptySymOps {
                    fn read_link(&self, _buf: &mut [u8]) -> Result<usize, SystemError> {
                        Ok(0)
                    }
                }

                ProcSymBuilder::new(EmptySymOps, ModeType::S_IRWXUGO)
                    .parent(parent)
                    .build()
                    .unwrap()
            }
        }),
        // TODO: 添加其他条目如 fd, cmdline, environ 等
    ];
}

impl DirOps for PidDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut cached_children = dir.cached_children().write();

        // 特殊处理 fd 目录 - 使用懒加载避免循环引用
        if name == "fd" {
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            // fd 目录需要进程引用
            if let Some(process) = self.get_process() {
                let inode = FdDirOps::new_inode(process, dir.self_ref_weak().clone());
                cached_children.insert(name.to_string(), inode.clone());
                return Ok(inode);
            } else {
                return Err(SystemError::ESRCH);
            }
        }

        // 处理其他静态条目
        if let Some(child) =
            lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
                (f)(self, dir.self_ref_weak().clone())
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
        // 先收集需要创建的条目，避免在持有锁时进行复杂操作
        let process_opt = self.get_process();

        let mut cached_children = dir.cached_children().write();

        // 填充静态条目
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(self, dir.self_ref_weak().clone())
        });

        // 添加 fd 目录（如果进程还存在）
        if !cached_children.contains_key("fd") {
            if let Some(process) = process_opt {
                let fd_inode = FdDirOps::new_inode(process, dir.self_ref_weak().clone());
                cached_children.insert("fd".to_string(), fd_inode);
            }
        }

        cached_children.downgrade()
    }
}
