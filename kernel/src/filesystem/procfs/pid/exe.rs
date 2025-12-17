//! /proc/[pid]/exe - 进程可执行文件的符号链接
//!
//! 这个符号链接指向进程的可执行文件路径

use crate::{
    filesystem::{
        procfs::template::{Builder, ProcSymBuilder, SymOps},
        vfs::{syscall::ModeType, IndexNode},
    },
    process::{ProcessControlBlock, ProcessManager},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/exe 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct ExeSymOps {
    process_ref: Arc<ProcessControlBlock>,
}

impl ExeSymOps {
    pub fn new(process_ref: Arc<ProcessControlBlock>) -> Self {
        Self { process_ref }
    }

    pub fn new_inode(
        process_ref: Arc<ProcessControlBlock>,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self::new(process_ref), ModeType::S_IRWXUGO) // 0777 - 符号链接权限
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for ExeSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // 先直接返回当前进程的可执行文件路径
        let pcb = ProcessManager::current_pcb();
        let exe = pcb.execute_path();
        let exe_bytes = exe.as_bytes();
        // if offset >= exe_bytes.len() {
        //     return Ok(0);
        // }
        let len = exe_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&exe_bytes[..len]);
        Ok(len)
    }
}
