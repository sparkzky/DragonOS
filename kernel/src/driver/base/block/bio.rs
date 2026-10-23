use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, mm::dma::DmaBuffer, sched::completion::Completion, time::clocksource::HZ};

use super::block_device::{BlockId, LBA_SIZE};

/// BIO操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioType {
    Read,
    Write,
    Flush,
}

/// BIO请求状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BioState {
    Init,
    Submitted,
    Completed,
    Failed,
}

/// 单个BIO请求
pub struct BioRequest {
    inner: SpinLock<InnerBioRequest>,
}

struct InnerBioRequest {
    bio_type: BioType,
    lba_start: BlockId,
    count: usize,
    buffer: DmaBuffer,
    state: BioState,
    completion: Arc<Completion>,
    result: Option<Result<usize, SystemError>>,
    complete_callbacks: Vec<BioCompleteCallback>,
    /// virtio-drivers返回的token，用于中断时匹配
    token: Option<u16>,
    stats_submit_cycle: usize,
}

type BioCompleteCallback = Box<dyn Fn(Result<usize, SystemError>) + Send + Sync>;

impl BioRequest {
    /// 创建一个读请求
    pub fn new_read(lba_start: BlockId, count: usize) -> Arc<Self> {
        Self::try_new_read(lba_start, count).expect("bio read allocation failed")
    }

    /// Create a read request without panicking when the DMA buffer cannot be
    /// allocated or the request size overflows.
    pub fn try_new_read(lba_start: BlockId, count: usize) -> Result<Arc<Self>, SystemError> {
        let len = Self::validate_request(lba_start, count)?;
        let buffer = DmaBuffer::try_alloc_bytes(len, Default::default())?;
        Ok(Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Read,
                lba_start,
                count,
                buffer,
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        }))
    }

    /// 创建一个写请求
    pub fn new_write(lba_start: BlockId, count: usize, data: &[u8]) -> Arc<Self> {
        Self::try_new_write(lba_start, count, data).expect("bio write allocation failed")
    }

    /// Create a write request with exact-length validation and fallible DMA
    /// allocation. A mismatched payload is never silently truncated or padded.
    pub fn try_new_write(
        lba_start: BlockId,
        count: usize,
        data: &[u8],
    ) -> Result<Arc<Self>, SystemError> {
        let len = Self::validate_request(lba_start, count)?;
        if data.len() != len {
            return Err(SystemError::EINVAL);
        }
        let mut buffer = DmaBuffer::try_alloc_bytes(len, Default::default())?;
        buffer.as_mut_slice().copy_from_slice(data);

        Ok(Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Write,
                lba_start,
                count,
                buffer,
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        }))
    }

    /// Create a new flush request.
    pub fn new_flush() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Flush,
                lba_start: 0,
                count: 0,
                buffer: DmaBuffer::alloc_bytes(1, Default::default()),
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        })
    }

    /// 标记为已提交，设置token
    pub fn mark_submitted(&self, token: u16) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        if inner.state != BioState::Init {
            return Err(SystemError::EINVAL);
        }
        inner.state = BioState::Submitted;
        inner.token = Some(token);
        Ok(())
    }

    /// 获取缓冲区的可变指针，供块设备在 BIO 完成前提交 DMA 或回收结果。
    ///
    /// # Safety
    ///
    /// 调用者必须独占设备侧对缓冲区的访问，并保证所有访问在调用
    /// [`Self::complete`] 前结束。BIO 进入终态后，等待者可以无锁读取该缓冲区。
    pub unsafe fn buffer_mut(&self) -> *mut [u8] {
        let mut inner = self.inner.lock_irqsave();
        inner.buffer.as_mut_slice() as *mut [u8]
    }

    /// 获取缓冲区的不可变引用
    pub fn buffer(&self) -> *const [u8] {
        let inner = self.inner.lock_irqsave();
        inner.buffer.as_slice() as *const [u8]
    }

    /// 将数据写入 BIO 缓冲区（用于同步回退路径）。
    ///
    /// 提交状态把缓冲区所有权移交给设备，因此安全写入只允许发生在 `Init`；
    /// 等待者随后可以在终态发布后于锁外复制较大的 DMA 数据。
    pub fn write_buffer(&self, data: &[u8]) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        if inner.state != BioState::Init {
            return Err(SystemError::EINVAL);
        }
        let copy_len = data.len().min(inner.buffer.len());
        inner.buffer.as_mut_slice()[..copy_len].copy_from_slice(&data[..copy_len]);
        Ok(())
    }

    /// 获取BIO类型
    pub fn bio_type(&self) -> BioType {
        self.inner.lock_irqsave().bio_type
    }

    /// 获取起始LBA
    pub fn lba_start(&self) -> BlockId {
        self.inner.lock_irqsave().lba_start
    }

    /// 获取扇区数
    pub fn count(&self) -> usize {
        self.inner.lock_irqsave().count
    }

    pub fn set_stats_submit_cycle(&self, cycle: usize) {
        self.inner.lock_irqsave().stats_submit_cycle = cycle;
    }

    pub fn stats_submit_cycle(&self) -> usize {
        self.inner.lock_irqsave().stats_submit_cycle
    }

    /// 获取token
    #[allow(dead_code)]
    pub fn token(&self) -> Option<u16> {
        self.inner.lock_irqsave().token
    }

    /// 完成BIO请求
    pub fn complete(&self, result: Result<usize, SystemError>) {
        let (completion, callbacks) = {
            let mut inner = self.inner.lock_irqsave();
            if matches!(inner.state, BioState::Completed | BioState::Failed) {
                return;
            }
            inner.state = if result.is_ok() {
                BioState::Completed
            } else {
                BioState::Failed
            };
            inner.result = Some(result.clone());
            let callbacks = core::mem::take(&mut inner.complete_callbacks);
            (inner.completion.clone(), callbacks)
        };
        // Publish the completion predicate before invoking callbacks. A
        // callback may re-enter `wait*()` on this BIO; signaling afterwards
        // would deadlock that callback and every callback behind it.
        completion.complete();
        for cb in callbacks {
            cb(result.clone());
        }
    }

    pub fn on_complete<F>(&self, callback: F)
    where
        F: Fn(Result<usize, SystemError>) + Send + Sync + 'static,
    {
        let mut callback = Some(Box::new(callback) as BioCompleteCallback);
        let completed = {
            let mut inner = self.inner.lock_irqsave();
            if let Some(result) = inner.result.clone() {
                Some(result)
            } else {
                inner.complete_callbacks.push(
                    callback
                        .take()
                        .expect("callback is owned until registration"),
                );
                None
            }
        };
        // A callback is allowed to inspect the BIO. Never invoke it while
        // holding the BIO spin lock, including the already-completed race.
        if let Some(result) = completed {
            callback
                .take()
                .expect("completed callback was not registered")(result);
        }
    }

    /// 等待完成，并在长时间无进展时输出有限的诊断日志。
    ///
    /// 探针只观测、不改变等待语义：完成/中断的返回值与直接睡眠完全一致。
    /// 以 1 秒为粒度分段等待；超过阈值后按衰减节奏输出至多 3 条告警，
    /// 用于区分“慢”与“卡死”，并依据 `state`（Init=从未提交到设备、
    /// Submitted=已提交未完成）定位卡死发生在提交侧还是完成侧。
    /// 这是早期启动阶段唯一的现场信息来源。
    fn wait_completion_probed(&self) -> Result<(), SystemError> {
        const PROBE_TICKS: i64 = HZ as i64;
        let mut waited_seconds: u64 = 0;
        let mut reported: u32 = 0;
        loop {
            // 必须先克隆 completion 并释放 inner 锁再睡眠，
            // 否则完成路径（IRQ/tasklet 上下文）会在 inner 锁上自旋。
            let completion = self.inner.lock_irqsave().completion.clone();
            let result = completion.wait_for_completion_timeout(PROBE_TICKS);
            match result {
                // Ok(0) 表示本轮超时；Ok(>=1) 表示已完成。
                Ok(remaining) if remaining >= 1 => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            waited_seconds += 1;
            let threshold = 5 + 10 * reported as u64;
            if waited_seconds >= threshold && reported < 3 {
                reported += 1;
                let inner = self.inner.lock_irqsave();
                log::warn!(
                    "block bio stuck {}s: type={:?} lba={} sectors={} state={:?} token={:?}",
                    waited_seconds,
                    inner.bio_type,
                    inner.lba_start,
                    inner.count,
                    inner.state,
                    inner.token,
                );
            }
        }
    }

    /// 等待BIO完成并返回结果
    pub fn wait(&self) -> Result<Vec<u8>, SystemError> {
        self.wait_completion_probed()?;
        // 完成等待已由 wait_completion_probed 完成（含卡死诊断）。
        // 获取结果
        let inner = self.inner.lock_irqsave();
        match inner.result.as_ref() {
            Some(Ok(completed)) if *completed == Self::expected_len(&inner) => {
                Ok(inner.buffer.to_vec())
            }
            Some(Ok(_)) => Err(SystemError::EIO),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }

    /// Wait for an exact read completion and copy the DMA payload directly
    /// into the caller's buffer. This avoids the intermediate `Vec` allocated
    /// by [`Self::wait`] and never exposes the DMA buffer pointer after submit.
    pub fn wait_read_into(&self, dst: &mut [u8]) -> Result<usize, SystemError> {
        self.wait_completion_probed()?;

        let (source, expected) = {
            let inner = self.inner.lock_irqsave();
            let expected = Self::expected_len(&inner);
            if inner.bio_type != BioType::Read || dst.len() != expected {
                return Err(SystemError::EINVAL);
            }
            match inner.result.as_ref() {
                Some(Ok(completed)) if *completed == expected => {
                    (inner.buffer.as_slice().as_ptr(), expected)
                }
                Some(Ok(_)) => return Err(SystemError::EIO),
                Some(Err(error)) => return Err(error.clone()),
                None => return Err(SystemError::EIO),
            }
        };
        // Completion retires device ownership before it wakes us. BioRequest
        // owns a fixed-size DMA buffer for its whole lifetime, so the pointer
        // remains stable and read-only while `&self` is alive. Keep the 64 KiB
        // worst-case copy outside the IRQ-disabled spin-lock section.
        dst.copy_from_slice(unsafe { core::slice::from_raw_parts(source, expected) });
        Ok(expected)
    }

    /// Wait for completion without copying the DMA payload. This is the
    /// completion path for writes and flushes.
    pub fn wait_status(&self) -> Result<usize, SystemError> {
        self.wait_completion_probed()?;

        let inner = self.inner.lock_irqsave();
        match inner.result.as_ref() {
            Some(Ok(completed)) if *completed == Self::expected_len(&inner) => Ok(*completed),
            Some(Ok(_)) => Err(SystemError::EIO),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }

    fn validate_request(lba_start: BlockId, count: usize) -> Result<usize, SystemError> {
        if count == 0 {
            return Err(SystemError::EINVAL);
        }
        lba_start.checked_add(count).ok_or(SystemError::EOVERFLOW)?;
        count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)
    }

    fn expected_len(inner: &InnerBioRequest) -> usize {
        match inner.bio_type {
            BioType::Read | BioType::Write => inner.count * LBA_SIZE,
            BioType::Flush => 0,
        }
    }
}
