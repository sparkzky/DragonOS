//! Per-task uprobe XOL execution state.
//!
//! This module owns only the task-local lifecycle of an instruction executing
//! out of line. Architecture exception code remains responsible for trap-frame
//! interpretation, DR6 handling, and signal delivery.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::libs::spinlock::SpinLock;
use crate::mm::ucontext::XolSlotLease;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskXolPhase {
    Idle = 0,
    Running = 1,
    Trapped = 2,
}

impl TaskXolPhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Idle,
            1 => Self::Running,
            2 => Self::Trapped,
            _ => unreachable!("invalid task XOL phase"),
        }
    }
}

/// The resources and original execution context retained while one task is
/// executing an instruction from an XOL slot.
pub struct ActiveXol {
    pub probe_vaddr: usize,
    pub return_addr: usize,
    pub orig_tf: bool,
    pub slot_end: usize,
    pub xol_lease: Arc<XolSlotLease>,
}

impl core::fmt::Debug for ActiveXol {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActiveXol")
            .field("probe_vaddr", &self.probe_vaddr)
            .field("return_addr", &self.return_addr)
            .field("orig_tf", &self.orig_tf)
            .field("slot_end", &self.slot_end)
            .field("xol_slot_offset", &self.xol_lease.offset())
            .finish_non_exhaustive()
    }
}

/// Single source of truth for one task's XOL state.
///
/// `phase` is the lock-free exception-routing discriminator. `payload` owns
/// the lease and context. Publication stores the payload before releasing the
/// Running phase; consumers acquire/swap the phase before taking the payload.
pub struct TaskXolState {
    phase: AtomicU8,
    payload: SpinLock<Option<ActiveXol>>,
}

impl core::fmt::Debug for TaskXolState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaskXolState")
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}

impl TaskXolState {
    pub const fn new() -> Self {
        Self {
            phase: AtomicU8::new(TaskXolPhase::Idle as u8),
            payload: SpinLock::new(None),
        }
    }

    #[inline]
    pub fn phase(&self) -> TaskXolPhase {
        TaskXolPhase::from_raw(self.phase.load(Ordering::Acquire))
    }

    /// Return the original userspace instruction address represented by an
    /// active XOL operation.
    ///
    /// Architecture code uses this logical address when a subsystem such as
    /// rseq must reason about the interrupted userspace instruction rather
    /// than the private XOL slot currently stored in the trap frame.
    pub fn active_probe_vaddr(&self) -> Option<usize> {
        if self.phase() == TaskXolPhase::Idle {
            return None;
        }

        self.payload
            .lock_irqsave()
            .as_ref()
            .map(|active| active.probe_vaddr)
    }

    /// Publish a newly active XOL operation. Refuses to overwrite an existing
    /// lease, which would otherwise make the earlier instruction unrecoverable.
    pub fn publish_running(&self, active: ActiveXol) -> Result<(), ActiveXol> {
        let mut payload = self.payload.lock_irqsave();
        if self.phase.load(Ordering::Acquire) != TaskXolPhase::Idle as u8 || payload.is_some() {
            return Err(active);
        }
        *payload = Some(active);
        drop(payload);
        self.phase
            .store(TaskXolPhase::Running as u8, Ordering::Release);
        Ok(())
    }

    /// Mark the active instruction as having raised a synchronous trap.
    /// Repeated calls are idempotent.
    pub fn mark_trapped(&self) -> Option<usize> {
        loop {
            match self.phase() {
                TaskXolPhase::Idle => return None,
                TaskXolPhase::Trapped => break,
                TaskXolPhase::Running => {
                    if self
                        .phase
                        .compare_exchange(
                            TaskXolPhase::Running as u8,
                            TaskXolPhase::Trapped as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
            }
        }
        self.payload
            .lock_irqsave()
            .as_ref()
            .map(|active| active.probe_vaddr)
    }

    /// Atomically return to Idle and take the active payload. The returned old
    /// phase is the authoritative classification for #DB completion.
    pub fn take(&self) -> Option<(TaskXolPhase, ActiveXol)> {
        let phase =
            TaskXolPhase::from_raw(self.phase.swap(TaskXolPhase::Idle as u8, Ordering::AcqRel));
        if phase == TaskXolPhase::Idle {
            return None;
        }
        let active = self.payload.lock_irqsave().take();
        debug_assert!(active.is_some(), "active XOL phase without payload");
        active.map(|active| (phase, active))
    }

    /// Drop an active XOL operation without modifying an obsolete trap frame,
    /// as required by successful exec and exit.
    pub fn discard(&self) -> bool {
        let active = self.take();
        let existed = active.is_some();
        drop(active);
        existed
    }
}

impl Default for TaskXolState {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────── uretprobe return-instance chain ────────────────────────

/// Depth cap of the uretprobe return-instance chain (mirrors Linux
/// `MAX_URETPROBE_DEPTH`).
///
/// When the cap is exceeded the new hijack is abandoned (ratelimited warning)
/// and the existing chain is unaffected — for a recursive function deeper than
/// 64 frames, returns from the 65th frame onward no longer fire callbacks
/// (same behavior as Linux).
pub const MAX_URETPROBE_DEPTH: usize = 64;

/// One hijacked function return (mirrors Linux `struct return_instance`).
///
/// Lifecycle: pushed on an entry hit (after the return address is
/// successfully hijacked) → popped in groups when the function return lands
/// on the trampoline (see `exception/uprobe.rs`).
pub struct ReturnInstance {
    /// Entry address of the probed function (= `probe_vaddr`, Linux `ri->func`).
    pub func: usize,
    /// User stack pointer at hijack time (the return-address slot address,
    /// Linux `ri->stack`).
    pub stack: usize,
    /// Original return address before hijacking. Chained instances share the
    /// same value as the chain head.
    pub orig_ret_vaddr: usize,
    /// The return-address slot already held the trampoline when pushed (tail
    /// call / recursion reusing the same stack slot). A chained instance's
    /// return must be processed as a group together with the non-chained
    /// anchor it is stacked on.
    pub chained: bool,
    /// Participant snapshot pinned at hijack time (review F4: the site may
    /// already be revoked by the time the function returns; the pinned
    /// snapshot keeps delivery targets stable. The remove direction relies on
    /// node active=false plus a closed gate to skip naturally; the add
    /// direction means later registrants do not receive this return event —
    /// a deliberate deviation).
    pub participants: Option<Arc<crate::mm::ucontext::UprobeParticipantNode>>,
}

impl core::fmt::Debug for ReturnInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReturnInstance")
            .field("func", &self.func)
            .field("stack", &self.stack)
            .field("orig_ret_vaddr", &self.orig_ret_vaddr)
            .field("chained", &self.chained)
            .finish_non_exhaustive()
    }
}

/// Liveness-check context for uretprobe frames (mirrors Linux `enum rp_check`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UretAliveCheck {
    /// Just executed a call (rsp just pushed down): alive iff `rsp < ri.stack`.
    Call,
    /// Trampoline hit / chained re-entry: alive iff `rsp <= ri.stack`.
    Ret,
}

impl UretAliveCheck {
    pub fn is_alive(self, ri: &ReturnInstance, rsp: usize) -> bool {
        match self {
            Self::Call => rsp < ri.stack,
            Self::Ret => rsp <= ri.stack,
        }
    }
}
/// The uretprobe return-instance chain of a single task (mirrors Linux
/// `utask->return_instances`).
///
/// The `Vec` tail = newest = deepest frame, equivalent to the head of Linux's
/// head-insertion list; decreasing the index walks toward shallower (older)
/// frames. Chain length ≤ [`MAX_URETPROBE_DEPTH`] and every trampoline hit
/// pops at least one entry, so processing is naturally bounded.
///
/// Return callbacks (BPF included) must run outside the lock; detached
/// participant `Arc`s are also dropped outside the lock.
pub struct TaskUretState {
    inner: SpinLock<Vec<ReturnInstance>>,
}

impl core::fmt::Debug for TaskUretState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // PCB formatting must not take the chain lock; report the depth only
        // via the lock-free convention used by TaskXolState (phase-like
        // advisory field), keeping diagnostics allocation- and lock-free.
        f.debug_struct("TaskUretState").finish_non_exhaustive()
    }
}

impl TaskUretState {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(Vec::new()),
        }
    }

    /// Current chain depth (checked against the cap before a hijack).
    pub fn depth(&self) -> usize {
        self.inner.lock_irqsave().len()
    }

    /// Push a new return instance (equivalent to Linux's head insertion
    /// `ri->next = head`).
    ///
    /// Returns `false` (caller gives up this hijack and warns) when the depth
    /// already reached [`MAX_URETPROBE_DEPTH`]. The Vec's capacity reservation
    /// for growth happens outside the lock (interrupt-enabled task context,
    /// equivalent to Linux's GFP_KERNEL kmalloc at the same position); the
    /// `push` itself runs inside the lock and, once reserved, never allocates.
    pub fn push(&self, ri: ReturnInstance) -> bool {
        {
            // Reservation phase: plain spinlock (preemptible / interrupts on),
            // executed only when capacity is exhausted.
            let mut chain = self.inner.lock();
            if chain.len() >= MAX_URETPROBE_DEPTH || chain.len() < chain.capacity() {
                drop(chain);
            } else if chain.try_reserve(1).is_err() {
                return false;
            }
        }
        let mut chain = self.inner.lock_irqsave();
        if chain.len() >= MAX_URETPROBE_DEPTH {
            return false;
        }
        chain.push(ri);
        true
    }

    /// Drop dead return instances starting from the chain tail (newest frame)
    /// (mirrors Linux `cleanup_return_instances`: clearing dead frames after a
    /// stack jump such as longjmp), until a live frame is reached.
    pub fn cleanup_dead(&self, rsp: usize, ctx: UretAliveCheck) {
        let mut chain = self.inner.lock_irqsave();
        while chain.last().is_some_and(|ri| !ctx.is_alive(ri, rsp)) {
            chain.pop();
        }
    }

    /// Original return address of the chain head (newest frame) — reused for
    /// chained hijacks (mirrors Linux
    /// `utask->return_instances->orig_ret_vaddr`). Returns `None` if empty.
    pub fn head_orig_ret_vaddr(&self) -> Option<usize> {
        self.inner.lock_irqsave().last().map(|ri| ri.orig_ret_vaddr)
    }

    /// Detach the whole return-instance chain (`mem::take`, capacity moves
    /// along — **zero allocation inside the lock**).
    ///
    /// Used by the trampoline hit path: the chain is task-private (all
    /// modifiers — entry hijack / trampoline hit / exec / exit — run in the
    /// task's own context), and the lock only provides memory ordering under
    /// exception nesting. After finishing group classification and delivery
    /// outside the lock, the caller must put the unconsumed prefix back via
    /// [`Self::restore_prefix`]. Any detached participant `Arc` is dropped
    /// outside the lock (review F3).
    ///
    /// **Why not split_off under the lock**: `Vec::split_off` must allocate
    /// for the new Vec, and allocation failure in a no_std kernel means panic
    /// — unacceptable inside an irqsave critical section / exception path
    /// (maintainer review revision: the original implementation performed the
    /// first detach before re-enabling interrupts and allocated inside the
    /// lock). Group classification is instead done outside the lock by index
    /// (see exception/uprobe.rs), with no allocation at all.
    pub fn take_all(&self) -> Vec<ReturnInstance> {
        core::mem::take(&mut *self.inner.lock_irqsave())
    }

    /// Put the unconsumed prefix back into the chain (`mem::replace`, no
    /// allocation).
    ///
    /// Legal only while the chain is currently empty (guaranteed by the
    /// task-private chain plus the single detach point); a violation is a
    /// logic bug, pinned down by debug_assert.
    pub fn restore_prefix(&self, prefix: Vec<ReturnInstance>) {
        let mut chain = self.inner.lock_irqsave();
        debug_assert!(
            chain.is_empty(),
            "uretprobe chain must be empty while its group is being handled"
        );
        *chain = prefix;
    }

    /// Reserve capacity for one upcoming push (called **before writing the
    /// user stack**).
    ///
    /// Returning false means the depth cap was reached or the reservation
    /// failed (the caller gives up the hijack). Reserving before the stack
    /// write guarantees that once the trampoline is written into the
    /// return-address slot, the later push cannot fail on allocation —
    /// otherwise the function return would hit a trampoline with no return
    /// instance (a spurious hit that unfairly kills the task with SIGILL).
    /// Mirrors Linux's "kmalloc ri first, then hijack the stack" ordering.
    pub fn reserve_one(&self) -> bool {
        // Reservation phase uses a plain spinlock (preemptible, interrupts
        // on): try_reserve is allowed to fail.
        let mut chain = self.inner.lock();
        chain.len() < MAX_URETPROBE_DEPTH
            && (chain.len() < chain.capacity() || chain.try_reserve(1).is_ok())
    }

    /// Discard the whole chain (no return callbacks delivered). Called from
    /// paths that never return to the old user context, such as exec/exit;
    /// detached participant `Arc`s are dropped outside the lock.
    pub fn cleanup(&self) {
        let chain = core::mem::take(&mut *self.inner.lock_irqsave());
        drop(chain);
    }
}

/// exec/exit: discard all of the task's uretprobe return instances (called
/// alongside `cleanup_task_active_xol`; the XOL VMA is DONTCOPY, so the new
/// context after fork/exec never hits the old trampoline, and the endpoint/
/// site references pinned on the chain are released here).
pub fn cleanup_task_uret_instances(pcb: &crate::process::ProcessControlBlock) {
    pcb.uret.cleanup();
}
