//! Exec handoff and wait transactions.
use super::*;


/// 诊断：最近一次 exec group 等待时的 (notify_count, leader_phase)。
/// 0=无, 1=Pending, 2=Exiting, 3=Ready；dump 时打印以区分“计数不降”与“leader 卡住”。
pub static EXEC_WAIT_COUNT: core::sync::atomic::AtomicI32 =
    core::sync::atomic::AtomicI32::new(-1);
pub static EXEC_WAIT_PHASE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

impl ProcessSignalState {
    fn record_exec_wait_state(&self) {
        use core::sync::atomic::Ordering;
        let g = self.inner();
        let phase = match g.group_exec_leader_phase {
            None => 0u8,
            Some(GroupExecLeaderPhase::Pending) => 1,
            Some(GroupExecLeaderPhase::Exiting) => 2,
            Some(GroupExecLeaderPhase::Ready) => 3,
        };
        EXEC_WAIT_COUNT.store(g.group_exec_notify_count as i32, Ordering::Relaxed);
        EXEC_WAIT_PHASE.store(phase, Ordering::Relaxed);
    }
}

impl ProcessSignalState {
    fn group_exec_wait_queue(&self) -> &WaitQueue {
        &self.body.group_exec_wait_queue
    }

    pub fn wait_group_exec_event_interruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.record_exec_wait_state();
        self.group_exec_wait_queue()
            .wait_event_interruptible(cond, before_sleep)
    }

    pub fn wait_group_exec_event_killable<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.record_exec_wait_state();
        self.group_exec_wait_queue()
            .wait_event_killable(cond, before_sleep)
    }

    pub fn wait_group_exec_event_uninterruptible<F, B>(
        &self,
        cond: F,
        before_sleep: Option<B>,
    ) -> Result<(), SystemError>
    where
        F: FnMut() -> bool,
        B: FnMut(),
    {
        self.record_exec_wait_state();
        // 诊断：不可中断的 handoff 等待一旦进入就很难再观测。这里在入口打印
        // “事务是否属于当前 owner / 计数 / 阶段”，持续超过 5 次说明卡死。
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 8 {
                let owner = crate::process::ProcessManager::current_pcb();
                let g = self.inner();
                let matched = Self::weak_matches(&g.group_exec_task, &owner);
                crate::print_critical!(
                    "exec uninterruptible wait: pid={} matched={} count={} phase={:?} flags_exec={}\n",
                    owner.raw_pid().data(),
                    matched,
                    g.group_exec_notify_count,
                    g.group_exec_leader_phase,
                    g.flags.contains(SignalFlags::GROUP_EXEC)
                );
            }
        }
        self.group_exec_wait_queue()
            .wait_event_uninterruptible(cond, before_sleep)
    }

    /// Start group exec and collect the transaction's ordinary sibling tokens
    /// under the same lock that completion uses.
    ///
    /// The callback may acquire thread-info locks, establishing the fixed
    /// `ProcessSignalState -> thread-info` order. It must assign `generation` to every
    /// identity-incomplete ordinary sibling and return that count.
    pub fn start_group_exec_transaction<F, R>(
        &self,
        owner: &Arc<ProcessControlBlock>,
        old_leader: Option<&Arc<ProcessControlBlock>>,
        collect: F,
    ) -> Result<R, SystemError>
    where
        F: FnOnce(u64) -> (R, usize),
    {
        let mut g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || g.flags.contains(SignalFlags::GROUP_EXEC) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        debug_assert!(old_leader
            .map(|leader| !Arc::ptr_eq(leader, owner))
            .unwrap_or(true));
        if old_leader
            .map(|leader| {
                leader.is_dead() || (leader.exit_notify_complete() && !leader.is_zombie())
            })
            .unwrap_or(false)
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let mut generation = g.group_exec_generation.wrapping_add(1);
        if generation == 0 {
            generation = 1;
        }
        let (result, pending) = collect(generation);

        g.flags.insert(SignalFlags::GROUP_EXEC);
        g.group_exec_task = Some(Arc::downgrade(owner));
        g.group_exec_old_leader = old_leader.map(Arc::downgrade);
        g.group_exec_leader_phase = old_leader.map(|leader| {
            if leader.exit_notify_complete() {
                GroupExecLeaderPhase::Ready
            } else {
                GroupExecLeaderPhase::Pending
            }
        });
        g.group_exec_generation = generation;
        g.group_exec_notify_count = pending as isize;
        let leader_ready = g.group_exec_leader_phase == Some(GroupExecLeaderPhase::Ready);
        drop(g);

        if leader_ready {
            self.group_exec_wait_queue().wakeup_all(None);
        }
        Ok(result)
    }

    /// Finish only the caller's transaction. A non-leader exec can finish only
    /// after the old-leader producer reached Ready.
    pub fn finish_group_exec_owned(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let mut g = self.inner_mut();
        if !Self::weak_matches(&g.group_exec_task, owner)
            || g.group_exec_notify_count != 0
            || matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Pending | GroupExecLeaderPhase::Exiting)
            )
        {
            return false;
        }
        Self::clear_group_exec_locked(&mut g);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        true
    }

    /// Cancel before the old leader commits its dedicated exit. Once Exiting
    /// has been claimed, the identity handoff must complete uninterruptibly.
    pub fn try_cancel_group_exec(&self, owner: &Arc<ProcessControlBlock>) -> GroupExecCancelResult {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_task, owner)
        {
            return GroupExecCancelResult::NotOwner;
        }
        if matches!(
            g.group_exec_leader_phase,
            Some(GroupExecLeaderPhase::Exiting | GroupExecLeaderPhase::Ready)
        ) {
            return GroupExecCancelResult::Committed;
        }

        Self::clear_group_exec_locked(&mut g);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        GroupExecCancelResult::Canceled
    }

    /// Atomically claim the old leader's dedicated producer path.
    pub fn claim_group_exec_leader_exit(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_old_leader, candidate)
            || g.group_exec_leader_phase != Some(GroupExecLeaderPhase::Pending)
        {
            return false;
        }
        g.group_exec_leader_phase = Some(GroupExecLeaderPhase::Exiting);
        true
    }

    /// Publish producer completion before waking the exec waiter. This also
    /// covers a leader that entered ordinary exit before group exec started.
    pub fn complete_group_exec_leader_exit(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        if !candidate.exit_notify_complete() {
            return false;
        }
        let mut g = self.inner_mut();
        if !g.flags.contains(SignalFlags::GROUP_EXEC)
            || !Self::weak_matches(&g.group_exec_old_leader, candidate)
            || !matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Pending | GroupExecLeaderPhase::Exiting)
            )
        {
            return false;
        }
        g.group_exec_leader_phase = Some(GroupExecLeaderPhase::Ready);
        drop(g);
        self.group_exec_wait_queue().wakeup_all(None);
        true
    }

    pub fn group_exec_leader_phase(
        &self,
        owner: &Arc<ProcessControlBlock>,
    ) -> Option<GroupExecLeaderPhase> {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner).then_some(g.group_exec_leader_phase)?
    }

    pub fn group_exec_pending_complete(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner) && g.group_exec_notify_count == 0
    }

    pub fn group_exec_handoff_ready(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner)
            && g.group_exec_notify_count == 0
            && matches!(
                g.group_exec_leader_phase,
                None | Some(GroupExecLeaderPhase::Ready)
            )
    }

    pub fn group_exec_committed(&self, owner: &Arc<ProcessControlBlock>) -> bool {
        let g = self.inner();
        Self::weak_matches(&g.group_exec_task, owner)
            && matches!(
                g.group_exec_leader_phase,
                Some(GroupExecLeaderPhase::Exiting | GroupExecLeaderPhase::Ready)
            )
    }

    /// Complete one ordinary sibling's identity-unhash token in O(1).
    pub fn complete_group_exec_task(&self, candidate: &Arc<ProcessControlBlock>) -> bool {
        if !candidate.identity_unhash_complete() {
            return false;
        }
        let mut g = self.inner_mut();
        let generation = candidate.take_group_exec_generation();
        if generation == 0
            || !g.flags.contains(SignalFlags::GROUP_EXEC)
            || generation != g.group_exec_generation
        {
            return false;
        }
        assert!(
            g.group_exec_notify_count > 0,
            "group-exec pending count underflow"
        );
        g.group_exec_notify_count -= 1;
        let ready = g.group_exec_notify_count == 0;
        drop(g);
        if ready {
            self.group_exec_wait_queue().wakeup_all(None);
        }
        true
    }

    /// 在与 GROUP_EXEC/GROUP_EXIT 相同的锁下执行关键区，避免并发插入线程组。
    pub fn with_group_exec_check<F, R>(&self, f: F) -> Result<R, SystemError>
    where
        F: FnOnce() -> R,
    {
        let g = self.inner_mut();
        if g.flags.contains(SignalFlags::GROUP_EXIT) || g.flags.contains(SignalFlags::GROUP_EXEC) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let ret = f();
        drop(g);
        Ok(ret)
    }

    /// 获取当前 exec 线程（去线程化执行者）。
    pub fn group_exec_task(&self) -> Option<Arc<ProcessControlBlock>> {
        self.inner().group_exec_task.as_ref()?.upgrade()
    }
}
