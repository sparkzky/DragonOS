//! uprobe 用户态异常分发（计划步骤 5/6/8）。
//!
//! `do_int3`/`do_debug` 按 [`TrapFrame::is_from_user`] 二分：用户态走本模块，
//! 内核态走现有 kprobe 分发（[`crate::exception::ebreak::EBreak`] /
//! [`crate::exception::debug::DebugException`]）。
//!
//! # 命中路径约束（关中断，entry.S `cli`）
//!
//! - 仅 `lock_irqsave` + 查表 + 改 trapframe + 跑 BPF（callback）；
//! - 绝不改页表、绝不睡眠、绝不取 `AddressSpace` 的 `RwSem`（会睡眠）；
//! - XOL slot 内容通过物理页的内核 direct-map 写入（`phys_2_virt`），无需
//!   `PageMapper`/`RwSem`——这是 `XolPage::page_paddr` 的设计意图。
//!
//! # F5：BPF 透明性
//!
//! event callback 入口 rip 必须是
//! 原探针语境（`probe_vaddr` 或 `probe_vaddr + insn_len`），XOL slot 用户地址
//! **绝不**暴露给 BPF。

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal::{force_kernel_signal_to_current, force_sig_fault_to_current};
use crate::mm::ucontext::UprobeParticipantNode;
use crate::mm::VirtAddr;
use crate::process::uprobe::{
    ActiveXol, ReturnInstance, TaskXolPhase, UretAliveCheck, MAX_URETPROBE_DEPTH,
};
use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};
use crate::syscall::user_access::{copy_from_user_protected, write_one_to_user_protected};
use kprobe::ProbeArgs;
use log::{debug, warn};
use system_error::SystemError;

/// `SIGTRAP` 的 `si_code`：breakpoint trap（镜像 Linux `TRAP_BRKPT`，用于未消费的
/// 用户态 #BP）。
const TRAP_BRKPT: i32 = 1;

/// `SIGTRAP` 的 `si_code`：single-step trap。
///
/// Linux 在用户态 TF 单步完成后使用该 code。DragonOS 已在 #DB 入口区分
/// DR6.BS 与 B0-B3，但尚未实现完整 ptrace virtual_dr6。
const TRAP_TRACE: i32 = 2;

/// `SIGTRAP` 的 `si_code`：硬件断点/观察点。
const TRAP_HWBKPT: i32 = 4;

/// x86 DR6 cause bits。
pub const DR6_TRAP_BITS: u64 = 0xf;
pub const DR6_SINGLE_STEP: u64 = 1 << 14;

/// RFLAGS 的 TF（Trap Flag）位——置位后每条指令执行完触发 #DB，用于 XOL 单步。
const RFLAGS_TF: u64 = 1 << 8;

/// 用户态 #BP（int3）分发：uprobe 命中则 XOL 单步原指令，否则投递
/// `SIGTRAP(TRAP_BRKPT)`。
///
/// 调用方（`do_int3`）已保证 `is_from_user()` 为真。
pub fn uprobe_breakpoint_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let break_addr = frame.break_address(); // = rip - 1 = probe_vaddr（raw rip 保留，
                                            // 供 BPF 回调经 break_address() 取得原探针址——batch3↔batch4 运行时契约）。

    // F5：回调执行期间 rip 必须保持 raw（probe_vaddr+1），绝不在此预设为原探针址，
    // 否则回调内 `break_address()=rip-1` 会得到 probe_vaddr-1 而非 probe_vaddr。
    // XOL slot 用户址仅在所有回调返回后才写入 rip，绝不暴露给 BPF。

    let pcb = ProcessManager::current_pcb();
    let mm = match pcb.basic().user_vm() {
        Some(mm) => mm,
        None => {
            // 用户态 #BP 但无 user_vm（不应发生）：防御性投递 SIGTRAP。
            return send_sigtrap_brkpt(frame, break_addr);
        }
    };

    // ── uretprobe trampoline dispatch (mirrors the first-line compare in
    // Linux handle_swbp) ──
    // The trampoline is a permanent int3 slot inside the XOL area and is not
    // in the uprobe_list hit table, so it must be dispatched before the
    // lookup and must never fall into the probe's XOL single-step path. When
    // the address has not been created it is 0, which differs from any legal
    // break_addr.
    if mm.xol_pool.trampoline_vaddr() == break_addr {
        return uprobe_trampoline_handler(frame);
    }

    // ── Phase 1：RCU 命中快照内复制 IRQ-safe 运行值 ──
    let captured = mm
        .uprobe_list
        .with_hit(break_addr, |site| -> Result<_, SystemError> {
            let xol_lease = site.xol_lease.clone();
            let slot_vaddr = xol_lease.slot_vaddr().data();
            let slot_end = slot_vaddr
                .checked_add(site.insn_analysis.insn_len)
                .ok_or(SystemError::EINVAL)?;
            let return_addr = site
                .probe_vaddr
                .checked_add(site.insn_analysis.insn_len)
                .ok_or(SystemError::EINVAL)?;

            let participants = site.participant_snapshot();
            Ok((xol_lease, slot_vaddr, slot_end, return_addr, participants))
        });
    let Some(captured) = captured else {
        return send_sigtrap_brkpt(frame, break_addr);
    };
    let (xol_lease, slot_vaddr, slot_end, return_addr, participants) = captured?;

    // #BP uses a DPL=3 interrupt gate so teardown cannot observe this CPU's
    // shootdown acknowledgement before the hit-table lookup and slot lease
    // capture are complete. The XOL VMA is immutable, so callbacks may run
    // from this point with normal user-exception interrupt semantics.
    unsafe { CurrentIrqArch::interrupt_enable() };

    // ── Phase 1.5：锁外跑 event callback（评审 R12）──
    // rip 保持 raw（probe_vaddr+1）：BPF 回调经 break_address()=rip-1 取得原探针址。
    // Entry callbacks are delivered only to !is_return participants
    // (mirroring Linux handler_chain calling only uc->handler); if the
    // snapshot contains an active is_return participant, the return address
    // is hijacked right after.
    let mut has_return_participant = false;
    if let Some(participants) = participants.as_ref() {
        participants.for_each_active(|participant| {
            if participant.is_return() {
                has_return_participant = true;
            } else {
                participant.deliver(&pcb, frame);
            }
        });
    }

    // ── Phase 1.6: uretprobe return-address hijack (Linux: uc->handler
    // first, then prepare_uretprobe). Must happen after interrupts are
    // re-enabled (user-stack reads/writes may fault) and before the XOL
    // publication; any failure silently gives up this hijack (the entry
    // callbacks have already run as usual). ──
    if has_return_participant {
        prepare_uretprobe(
            &pcb,
            mm.xol_pool.trampoline_vaddr(),
            participants,
            break_addr,
            frame,
        );
    }

    // ── Phase 2：保存 per-thread 活跃状态（评审 R2/R5）→ 重定向 rip → 置 TF ──
    let orig_tf = frame.rflags & RFLAGS_TF != 0;
    let active = ActiveXol {
        probe_vaddr: break_addr,
        return_addr,
        orig_tf,
        slot_end,
        xol_lease,
    };
    if let Err(active) = pcb.uprobe.publish_running(active) {
        warn!(
            "nested uprobe XOL state at probe {:#x}; preserving existing lease",
            active.probe_vaddr
        );
        return send_sigtrap_brkpt(frame, break_addr);
    }
    frame.set_rip(slot_vaddr);
    frame.rflags |= RFLAGS_TF;

    Ok(())
}

/// uretprobe return-address hijack (mirrors Linux `prepare_uretprobe` +
/// `arch_uretprobe_hijack_return_addr`).
///
/// Called after the entry callbacks and before the XOL single-step
/// publication, and necessarily **after interrupts are re-enabled**: reads
/// and writes of the user stack may fault (`copy_*_protected` goes through
/// exception-table fixup). Every failure mode silently gives up this hijack
/// (entry callbacks are unaffected; the function returns to its original
/// return address as usual):
///
/// - depth cap reached: give up after a ratelimited warning (DragonOS has no
///   ratelimit facility yet, so a plain `warn!` stands in; see impl-notes);
/// - reading `*(u64*)rsp` fails: the return-address slot was just written by
///   the call, so it should always be resident in theory;
/// - chained but the chain is empty: treated as user forgery (same as Linux
///   uprobe_warn);
/// - writing the trampoline fails: a single 8-byte exception-table write
///   cannot be partial (review F5, no SIGSEGV path).
///
/// In the chained case (tail call / recursion reusing the same stack slot),
/// dead frames are cleared first and the chain-head orig is reused, leaving
/// the original return address from the first hijack on the stack so the
/// assertion unwrapping stays symmetric (same as Linux).
fn prepare_uretprobe(
    pcb: &ProcessControlBlock,
    trampoline_vaddr: usize,
    participants: Option<alloc::sync::Arc<UprobeParticipantNode>>,
    probe_vaddr: usize,
    frame: &TrapFrame,
) {
    // The depth cap comes before any stack write (mirroring Linux: exceeding
    // the limit simply gives up the hijack, leaving the old chain untouched).
    if pcb.uret.depth() >= MAX_URETPROBE_DEPTH {
        warn!(
            "uprobe: omit uretprobe due to nestedness limit pid/tgid={:?}",
            ProcessManager::current_pid()
        );
        return;
    }

    // Defensive: the registration path for is_return sites guarantees the
    // trampoline exists; if a race reads 0 (should not happen), give up the
    // hijack instead of writing 0 onto the user stack.
    if trampoline_vaddr == 0 {
        return;
    }

    // Reserve chain capacity before reading/writing the stack (maintainer
    // review revision): guarantees that after the trampoline is written into
    // the return-address slot, the later push cannot fail on allocation —
    // otherwise the function return would hit a trampoline with no return
    // instance (a spurious hit that unfairly kills the task with SIGILL).
    // Mirrors Linux's "kmalloc ri first, then hijack the stack" ordering.
    // A failed reservation (OOM / depth cap re-check) silently gives up.
    if !pcb.uret.reserve_one() {
        return;
    }
    let rsp = frame.stack_pointer();
    // Read the return-address slot (rasize = 8 bytes). Failure silently
    // gives up.
    let mut orig_ret_vaddr: usize = 0;
    let read = unsafe {
        copy_from_user_protected(
            core::slice::from_raw_parts_mut(
                (&mut orig_ret_vaddr as *mut usize).cast::<u8>(),
                core::mem::size_of::<usize>(),
            ),
            VirtAddr::new(rsp),
        )
    };
    if read.is_err() {
        return;
    }

    let chained = orig_ret_vaddr == trampoline_vaddr;
    // Drop dead frames left by stack jumps such as longjmp (Linux
    // cleanup_return_instances; context difference: chained re-entry uses RET
    // semantics, a plain entry uses CALL semantics).
    pcb.uret.cleanup_dead(
        rsp,
        if chained {
            UretAliveCheck::Ret
        } else {
            UretAliveCheck::Call
        },
    );

    if chained {
        // The stack already holds the trampoline: reuse the chain head's
        // (newest frame) original return address.
        let Some(orig) = pcb.uret.head_orig_ret_vaddr() else {
            // Trampoline read but the chain is empty: a fork leftover or user
            // forgery; give up the hijack.
            warn!(
                "uprobe: handle tail call failed (empty return chain) pid {:?}",
                ProcessManager::current_pid()
            );
            return;
        };
        orig_ret_vaddr = orig;
    } else {
        // Hijack: write the trampoline into the return-address slot. Failure
        // silently gives up (review F5).
        let written = unsafe { write_one_to_user_protected(VirtAddr::new(rsp), &trampoline_vaddr) };
        if written.is_err() {
            return;
        }
    }

    let pushed = pcb.uret.push(ReturnInstance {
        func: probe_vaddr,
        stack: rsp,
        orig_ret_vaddr,
        chained,
        participants,
    });
    debug_assert!(pushed, "uretprobe depth was pre-checked but push failed");
}

/// uretprobe trampoline hit handling (mirrors Linux `handle_trampoline`).
///
/// Entered via the dispatch in [`uprobe_breakpoint_handler`] once
/// `break_addr == trampoline_vaddr` is matched: the function return has
/// landed on the trampoline's int3, so pop the return-instance group, run the
/// return callbacks, and restore rip to the original return address.
/// **sp is never modified** (the return-address slot was already hijacked;
/// nothing to restore).
///
/// - Spurious hit (empty chain: a fork-leftover address / user forgery):
///   re-enable interrupts first, then force `SIGILL` (same as Linux
///   `uprobe_warn` + `force_sig(SIGILL)`);
/// - Normal hit: under the lock only classification + chain detach happen
///   (review F3); outside the lock, delivery uses a **cloned TrapFrame**
///   (rip = `orig_ret_vaddr + 1`, so that in the BPF context
///   `break_address()` resolves back to the original return address;
///   F5: never expose XOL/trampoline internal addresses) to deliver
///   `is_return` participants; when `!valid` (longjmp already skipped
///   shallower frames) the whole group is skipped and surviving groups
///   continue to be processed.
fn uprobe_trampoline_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();
    let rsp = frame.stack_pointer();

    // ── Zero-allocation detach under the lock: take the whole chain
    // (mem::take, capacity moves along, no allocation) ──
    let mut chain = pcb.uret.take_all();
    if chain.is_empty() {
        // Spurious hit. Same as send_sigtrap_brkpt: leave the IRQ-disabled
        // critical section before delivering the signal.
        warn!(
            "uprobe: unexpected uretprobe trampoline hit, pid {:?}, rip {:#x}; sending SIGILL",
            ProcessManager::current_pid(),
            frame.rip
        );
        unsafe { CurrentIrqArch::interrupt_enable() };
        if let Err(err) = force_kernel_signal_to_current(Signal::SIGILL) {
            warn!(
                "failed to send SIGILL for uretprobe trampoline, pid {:?}, err: {:?}",
                ProcessManager::current_pid(),
                err
            );
        }
        return Ok(());
    }

    // The trampoline path's own interrupts-on point: return callbacks
    // (BPF helpers included) may take locks and allocate.
    unsafe { CurrentIrqArch::interrupt_enable() };

    // ── Group classification and delivery outside the lock (index-based,
    //    allocation-free throughout; maintainer review revision: the original
    //    split_off ran under the lock / the first detach ran before
    //    interrupts were re-enabled, both violating hit-path discipline) ──
    //
    // Group = chain[anchor..] (anchor = the first non-chained anchor scanning
    // backward from the tail, anchor included), mirroring Linux
    // find_next_ret_chain; valid = the frame before the anchor (shallower)
    // does not exist, or its stack frame is still alive (RET semantics:
    // rsp <= prev.stack).
    let mut resume_ip: Option<usize> = None;
    while !chain.is_empty() {
        // The bottom-most (oldest) element always has chained == false
        // (the chain was empty when it was pushed), so rposition always
        // resolves; defensively treat an all-chained chain as one group.
        let anchor = chain.iter().rposition(|ri| !ri.chained).unwrap_or(0);
        let valid = match anchor.checked_sub(1) {
            Some(prev) => UretAliveCheck::Ret.is_alive(&chain[prev], rsp),
            None => true,
        };

        if valid {
            // Delivery order: newest → oldest (the Linux free_ret_instance
            // order).
            for ri in chain[anchor..].iter().rev() {
                if let Some(participants) = ri.participants.as_ref() {
                    // Clone the TrapFrame (TrapFrame: Copy) and set
                    // rip=orig+1: in the callback context
                    // break_address()=rip-1 resolves back to the original
                    // return address, while the real frame's rip is restored
                    // uniformly outside the loop (blind spot #3: the callback
                    // window never touches the real frame, so a synchronous
                    // exception's si_addr cannot leak the trampoline
                    // address).
                    let mut cb_frame = *frame;
                    cb_frame.set_rip(ri.orig_ret_vaddr + 1);
                    participants.for_each_active(|participant| {
                        if participant.is_return() {
                            participant.deliver(&pcb, &cb_frame);
                        }
                    });
                }
            }
            // Resume target = the newest (deepest) frame's orig within the
            // group (a chained group shares the same value).
            resume_ip = Some(chain[chain.len() - 1].orig_ret_vaddr);
        }
        // The group is discarded whether valid or not (!valid: longjmp
        // already skipped shallower frames, so the whole group gets no
        // delivery and the rip restore value is overwritten by the next
        // round — only the final valid group's value becomes visible to
        // users, as in Linux). truncate releases the group's participant
        // Arcs — dropped outside the lock (review F3).
        chain.truncate(anchor);

        if valid {
            break;
        }
        // !valid implies a shallower frame exists (anchor-1); the loop
        // continues with it as the anchor.
    }

    if let Some(ip) = resume_ip {
        // Put the unconsumed prefix back (no allocation). The chain is
        // task-private: this handler holds exclusive modification rights, so
        // no concurrent modification can happen during delivery.
        pcb.uret.restore_prefix(chain);
        frame.set_rip(ip);
        debug!("uprobe uretprobe return handled: resume {:#x}", ip);
        Ok(())
    } else {
        // The chain was drained group by group without any valid group
        // appearing (theoretically unreachable: the shallowest frame has
        // anchor==0 and therefore valid==true). Fail-safe handling: rip
        // still points to trampoline+1 and must never return to user mode
        // like this (it would re-execute int3 in an endless loop), so signal
        // SIGILL with spurious-hit semantics.
        debug_assert!(chain.is_empty());
        warn!(
            "uprobe: uretprobe return chain exhausted mid-handle, pid {:?}; sending SIGILL",
            ProcessManager::current_pid()
        );
        if let Err(err) = force_kernel_signal_to_current(Signal::SIGILL) {
            warn!(
                "failed to send SIGILL for uretprobe trampoline, pid {:?}, err: {:?}",
                ProcessManager::current_pid(),
                err
            );
        }
        Ok(())
    }
}

/// 用户态 #DB 分发：task XOL state 为 active → XOL 单步完成（消费）；否则本
/// 异常**不属于 uprobe**，返回 `false` 交由调用方路由到正常 debug 路径
/// （ptrace 单步 / 硬件断点 / SIGTRAP，评审 R4）。
///
/// 调用方（`do_debug`）已保证 `is_from_user()` 为真。
///
/// 返回值：`true` = 精确完成本次 XOL；`false` = 非 uprobe #DB，或异常 #DB
/// 已 abort 但仍需走正常用户 SIGTRAP 路径。
pub fn uprobe_debug_handler(frame: &mut TrapFrame, dr6: u64) -> Result<bool, SystemError> {
    let pcb = ProcessManager::current_pcb();

    if pcb.uprobe.phase() == TaskXolPhase::Idle {
        // 非 uprobe 单步 #DB：不吞掉（评审 R4），交还 do_debug 走正常
        // DebugException 路径（ptrace / 硬件断点 / SIGTRAP）。
        return Ok(false);
    }

    // Atomically return the task state to Idle and preserve the old phase for
    // exact Running/Trapped classification.
    let state = pcb.uprobe.take();
    let Some((phase, state)) = state else {
        warn!(
            "uprobe #DB: active phase without payload @ rip {:#x}",
            frame.rip
        );
        return Ok(false);
    };

    // 只有 Running 原指令在本租约的 slot 内恰好执行完毕，才是本次 XOL
    // 的完成 #DB。部分虚拟化环境不会为该精确完成事件报告 DR6.BS；归属
    // 由任务的 XOL phase 和精确 slot endpoint 决定，DR6.B0-B3 仍在完成后
    // 转交普通硬件断点语义。页范围判断会把异常改道后的 #DB 错认成完成。
    let rip = frame.rip as usize;
    if phase != TaskXolPhase::Running || rip != state.slot_end {
        warn!(
            "uprobe #DB abort: rip {:#x}, expected {:#x}, dr6 {:#x}, state {:?}, re-execute probe {:#x}",
            rip, state.slot_end, dr6, phase, state.probe_vaddr
        );
        restore_after_abort(frame, &state);
        pcb.recalc_sigpending();
        // 当前 #DB 不是 XOL 完成事件，交给用户 debug/SIGTRAP 路径。
        return Ok(false);
    }

    // ── XOL 完成：恢复 rip 到返回址 + 恢复原始 TF（评审 R5）──
    frame.set_rip(state.return_addr);
    if state.orig_tf {
        // 这次 XOL 就是调试器要求单步的那一条指令。Linux
        // arch_uprobe_post_xol() 在这里立即排队 SIGTRAP；若只保留 TF 等待
        // 下一次 #DB，会额外执行一条真实指令后才通知调试器。
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }

    debug!(
        "uprobe XOL single-step done: resume {:#x} (orig_tf={})",
        state.return_addr, state.orig_tf
    );

    // 重新发布 XOL 窗口内暂时延迟的普通 pending 信号。
    pcb.recalc_sigpending();

    // 回调与所有 irqsave 临界区结束后再开中断并排队调试信号。
    if state.orig_tf {
        send_sigtrap_trace(state.return_addr)?;
    } else if dr6 & DR6_TRAP_BITS != 0 {
        // uprobe 只消费自己置 TF 产生的 BS；同一 #DB 中并发出现的硬件断点
        // cause 仍必须对用户可见，不能随 XOL 完成一起吞掉。
        send_sigtrap_hwbkpt(state.return_addr)?;
    }

    Ok(true)
}

/// 向当前进程投递 `SIGTRAP(TRAP_BRKPT)`（未消费的用户态 #BP）。
///
/// 参照 `do_undefined_opcode` 发 `SIGILL` 的模式：先开中断再投递信号。
/// `si_addr` 取断点地址。
fn send_sigtrap_brkpt(frame: &mut TrapFrame, break_addr: usize) -> Result<(), SystemError> {
    // 若这个 #BP 来自 XOL 指令本身，它是确定会投递信号的同步异常。
    mark_current_xol_trapped();
    // #Safety: `interrupt_enable` 仅置位 RFLAGS.IF；此处已脱离 uprobe 命中路径的
    // irqsave 临界区（uprobe_list/XOL slot lock 均已释放），与 do_undefined_opcode 一致。
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) =
        force_sig_fault_to_current(Signal::SIGTRAP, TRAP_BRKPT, VirtAddr::new(break_addr))
    {
        abort_current_xol(frame);
        warn!(
            "failed to send SIGTRAP(TRAP_BRKPT) for user #BP, pid: {:?}, addr: {:#x}, err: {:?}",
            ProcessManager::current_pid(),
            break_addr,
            err
        );
    }
    Ok(())
}

fn restore_after_abort(frame: &mut TrapFrame, state: &ActiveXol) {
    frame.set_rip(state.probe_vaddr);
    if state.orig_tf {
        frame.rflags |= RFLAGS_TF;
    } else {
        frame.rflags &= !RFLAGS_TF;
    }
}

/// 把当前 XOL 标成已陷阱。幂等；只应在同步异常确定将投递信号后调用。
pub fn mark_current_xol_trapped() -> bool {
    mark_current_xol_trapped_and_get_probe_addr().is_some()
}

/// Mark the current XOL instruction as trapped and return the original
/// instruction address. Synchronous fault handlers use this to build siginfo
/// without exposing the private XOL slot address to userspace.
pub fn mark_current_xol_trapped_and_get_probe_addr() -> Option<usize> {
    let pcb = ProcessManager::current_pcb();
    pcb.uprobe.mark_trapped()
}

/// 幂等中止当前 XOL，并把 trapframe 恢复为可重试原指令的状态。
pub fn abort_current_xol(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    let state = pcb.uprobe.take();
    let Some((_phase, state)) = state else {
        return false;
    };
    restore_after_abort(frame, &state);
    drop(state);
    pcb.recalc_sigpending();
    true
}

/// exec/exit 等不再返回旧用户上下文的路径可调用此函数释放 ActiveXol 的
/// site/slot/consumer 强引用。无需也不应修改一个即将废弃的 trapframe。
pub fn cleanup_task_active_xol(pcb: &ProcessControlBlock) {
    pcb.uprobe.discard();
    pcb.recalc_sigpending();
}

/// 信号递送门：返回 `false` 表示普通异步信号需要延迟到本条 XOL 指令完成。
/// fatal 或已标记 Trapped 的路径会先 abort，再允许信号构造用户 frame。
pub fn signal_gate(frame: &mut TrapFrame) -> bool {
    let pcb = ProcessManager::current_pcb();
    let state = pcb.uprobe.phase();
    if state == TaskXolPhase::Idle {
        return true;
    }

    // group-exit 与线程/共享 SIGKILL 都必须立刻终止 XOL。该 helper 虽因 OOM
    // 命名，但语义正是这里需要的完整 fatal-pending 查询。
    let fatal = Signal::oom_fatal_signal_pending(&pcb);
    if state == TaskXolPhase::Running && !fatal {
        // pending 队列保持不变；只暂时清 fast flag，避免 exit-to-user loop
        // 原地重入。XOL 的紧邻 #DB 完成/abort 会 recalc 并重新置位。
        pcb.flags().remove(ProcessFlags::HAS_PENDING_SIGNAL);
        // 与并发 signal sender 再校验一次：若 fatal 在首次查询和清 flag 之间
        // 到达，必须本次就 abort；若在本次查询之后到达，sender 会重新置
        // HAS_PENDING_SIGNAL，exit-to-user loop 会再次进入本 gate。
        if Signal::oom_fatal_signal_pending(&pcb) {
            mark_current_xol_trapped();
            abort_current_xol(frame);
            return true;
        }
        return false;
    }

    if fatal {
        mark_current_xol_trapped();
    }
    abort_current_xol(frame);
    true
}

/// 投递用户态单步产生的 `SIGTRAP(TRAP_TRACE)`。
///
/// 该入口只负责用户调试异常，不得路由到仅处理内核 kprobe 的
/// `DebugException`。
pub fn send_sigtrap_trace(addr: usize) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) = force_sig_fault_to_current(Signal::SIGTRAP, TRAP_TRACE, VirtAddr::new(addr)) {
        warn!(
            "failed to send SIGTRAP(TRAP_TRACE), pid: {:?}, addr: {:#x}, err: {:?}",
            ProcessManager::current_pid(),
            addr,
            err
        );
    }
    Ok(())
}

fn send_sigtrap_hwbkpt(addr: usize) -> Result<(), SystemError> {
    send_sigtrap_fault(TRAP_HWBKPT, addr, "TRAP_HWBKPT")
}

/// 投递未被 uprobe 消费的用户 #DB。DragonOS 尚无完整 ptrace virtual_dr6，
/// 但至少按 Linux get_si_code() 的优先级保留 single-step 与 hardware cause。
pub fn send_user_debug_sigtrap(addr: usize, dr6: u64) -> Result<(), SystemError> {
    if dr6 & DR6_SINGLE_STEP != 0 {
        send_sigtrap_trace(addr)
    } else if dr6 & DR6_TRAP_BITS != 0 {
        send_sigtrap_hwbkpt(addr)
    } else {
        send_sigtrap_fault(TRAP_BRKPT, addr, "TRAP_BRKPT")
    }
}

fn send_sigtrap_fault(code: i32, addr: usize, name: &str) -> Result<(), SystemError> {
    unsafe { CurrentIrqArch::interrupt_enable() };
    if let Err(err) = force_sig_fault_to_current(Signal::SIGTRAP, code, VirtAddr::new(addr)) {
        warn!(
            "failed to send SIGTRAP({}), pid: {:?}, addr: {:#x}, err: {:?}",
            name,
            ProcessManager::current_pid(),
            addr,
            err
        );
    }
    Ok(())
}
