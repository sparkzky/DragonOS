//! 系统停顿看门狗与任务状态转储（Linux SysRQ-T 的等价物）。
//!
//! 触发条件：连续多个观察窗口内上下文切换计数零增长（系统级停顿——
//! 所有任务被阻塞、CPU 空闲），且系统内任务数达到阈值。
//! 输出走关键通道（`println_critical!`），在控制台背压下也保证可见。
//! 用于捕获"静默楔死"类缺陷的现场（哪些任务、什么状态、阻塞在哪）。

use alloc::string::ToString;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::init::initcall::INITCALL_DEVICE;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::ProcessManager;
use crate::time::{sleep::nanosleep, PosixTimeSpec};


/// 观察窗口长度（秒）
const WATCH_INTERVAL_S: i64 = 10;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicBool;
/// 连续零增长窗口数达到该值视为停顿
const STALL_WINDOWS: u32 = 2;
/// 系统内任务数低于该值不转储（避免启动早期误报）
const MIN_TASKS: usize = 8;

fn stall_watchdog_thread() -> i32 {
    let mut stalled_windows: u32 = 0;
    let mut last_switches = crate::process::nr_context_switches();
    // 转储后需重新观察到进度才会再次武装，防止刷屏
    let mut armed = true;
    // B+（不可中断阻塞）持续检测：dockerd/containerd 类 daemon 卡死后系统
    // 整体仍健康（ctxsw 正常增长），停摆检测抓不到——但被卡任务会长期停留
    // 在 uninterruptible 状态。连续多个窗口观察到即转储一次（触发不依赖
    // 任何控制台输入路径）。
    let mut uninterruptible_windows: u32 = 0;
    let mut uninterruptible_armed = true;
    // 延迟诊断基线：TLB shootdown 无法在自己的现场打印（那里可能关中断/持锁），
    // 只递增计数器，由本线程（进程上下文）每窗口转述一次。
    let mut last_tlb_irqoff = 0usize;
    let mut last_tlb_stalls = 0usize;
    loop {
        let _ = nanosleep(PosixTimeSpec::new(WATCH_INTERVAL_S, 0));

        let tlb = &crate::mm::tlb::TLB_DIAG;
        let irqoff = tlb.irqoff_events.load(core::sync::atomic::Ordering::Relaxed);
        if irqoff != last_tlb_irqoff {
            let origin = tlb.irqoff_origin.load(core::sync::atomic::Ordering::Relaxed);
            let entry = match origin & 0xff {
                crate::mm::tlb::SHOOTDOWN_ENTRY_KERNEL_RANGE => "flush_tlb_kernel_range",
                _ => "flush_tlb_mm_range",
            };
            // 记录的是调用点（#[track_caller]），用于直接定位"带锁/关中断
            // 发起 shootdown"的那个调用者。
            let fptr = tlb.irqoff_file_ptr.load(core::sync::atomic::Ordering::Relaxed);
            let flen = tlb.irqoff_file_len.load(core::sync::atomic::Ordering::Relaxed);
            let line = tlb.irqoff_line.load(core::sync::atomic::Ordering::Relaxed);
            let file: &str = if fptr != 0 && flen < 256 {
                unsafe {
                    core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                        fptr as *const u8,
                        flen,
                    ))
                }
            } else {
                "?"
            };
            println_critical!(
                "tlb: {} shootdown(s) initiated with interrupts disabled; last on cpu {} via {} at {}:{}",
                irqoff - last_tlb_irqoff,
                origin >> 8,
                entry,
                file,
                line
            );
            last_tlb_irqoff = irqoff;
        }
        let stalls = tlb.csd_stalls.load(core::sync::atomic::Ordering::Relaxed);
        if stalls != last_tlb_stalls {
            println_critical!(
                "tlb: {} CSD wait(s) exceeded the stall bound; last target cpu {}",
                stalls - last_tlb_stalls,
                tlb.csd_stall_cpu.load(core::sync::atomic::Ordering::Relaxed)
            );
            last_tlb_stalls = stalls;
        }

        let now_switches = crate::process::nr_context_switches();
        if now_switches == last_switches {
            stalled_windows = stalled_windows.saturating_add(1);
        } else {
            stalled_windows = 0;
            armed = true;
        }
        last_switches = now_switches;

        if stalled_windows >= STALL_WINDOWS && armed {
            if crate::process::ProcessManager::get_all_processes().len() >= MIN_TASKS {
                dump_all_tasks();
                armed = false;
            }
        }

        // B+ 持续检测（与停摆检测互补）
        let unint_count = crate::process::snapshot_all_processes()
            .map(|procs| {
                procs
                    .iter()
                    .filter(|p| {
                        !p.is_kthread()
                            && p.raw_pid().data() > 100
                            && matches!(
                                p.sched_info().state(),
                                crate::process::ProcessState::Blocked(_)
                            )
                    })
                    .count()
            })
            .unwrap_or(0);
        // 用户态阻塞计数（含可中断阻塞）：docker 链楔死时 43 个、空闲
        // 系统 ~15 个以下；阈值 25 + 连续窗口判定，避免空闲误触发。
        if unint_count >= 25 {
            uninterruptible_windows = uninterruptible_windows.saturating_add(1);
        } else {
            uninterruptible_windows = 0;
            uninterruptible_armed = true;
        }
        if uninterruptible_windows >= STALL_WINDOWS && uninterruptible_armed {
            println_critical!(
                "==== USER BLOCK STORM: {} user tasks blocked for >{}s ====",
                unint_count,
                uninterruptible_windows * (WATCH_INTERVAL_S as u32)
            );
            dump_all_tasks();
            uninterruptible_armed = false;
        }
    }
}

/// 经关键通道转储所有任务的 pid / 名称 / 调度状态
pub(crate) fn dump_all_tasks() {
    // 诊断：打印最近一次进入公共等待入口的调用点，直接指出停摆时“谁在等”。
    {
        use core::sync::atomic::Ordering as O;
        let file_ptr = crate::libs::wait_queue::LAST_WAIT_SITE_FILE.load(O::Relaxed);
        let len = crate::libs::wait_queue::LAST_WAIT_SITE_LEN.load(O::Relaxed);
        let ec = crate::ipc::process_signal::exec::EXEC_WAIT_COUNT.load(O::Relaxed);
        if ec >= 0 {
            crate::println_critical!(
                "exec wait state: notify_count={} leader_phase={}\n",
                ec,
                crate::ipc::process_signal::exec::EXEC_WAIT_PHASE.load(O::Relaxed)
            );
        }
        if !file_ptr.is_null() && len > 0 {
            let bytes = unsafe { core::slice::from_raw_parts(file_ptr, len) };
            let file = core::str::from_utf8(bytes).unwrap_or("<invalid>");
            crate::println_critical!(
                "last wait site: {}:{} (pid={})\n",
                file,
                crate::libs::wait_queue::LAST_WAIT_SITE_LINE.load(O::Relaxed),
                crate::libs::wait_queue::LAST_WAIT_SITE_PID.load(O::Relaxed)
            );
        }
    }
    let procs = match crate::process::snapshot_all_processes() {
        Ok(p) => p,
        Err(_) => {
            println_critical!("TASK DUMP: failed to snapshot processes");
            return;
        }
    };
    println_critical!(
        "==== TASK DUMP: kernel stall detected, {} tasks ====",
        procs.len()
    );
    for pcb in procs.iter() {
        let state = pcb.sched_info().state();
        // 打印 tgid：exec 的 group-exec 等待需要兄弟线程退出，按 tgid 分组
        // 可直接看出哪个进程组卡住。
        // 无 Pid 身份的任务（如尚未完成 exit 解挂的线程）不能走
        // active_pid_ns()，否则 pid() 的 unwrap 会 panic。
        let tgid = if pcb
            .task_pid_ptr(crate::process::pid::PidType::PID)
            .is_some()
        {
            pcb.task_tgid_vnr().map(|p| p.data()).unwrap_or(0)
        } else {
            pcb.raw_pid().data()
        };
        let ppid = pcb
            .real_parent_pcb()
            .map(|p| p.raw_pid().data())
            .unwrap_or(0);
        println_critical!(
            "[pid {:>6}] tgid {:>6} ppid {:>6} {:<16} state={:?}",
            pcb.raw_pid().data(),
            tgid,
            ppid,
            pcb.basic().name(),
            state
        );
        // 所有 Blocked 任务（含内核线程）都打印最近等待点，便于定位
        // 跨线程的锁/等待依赖链。
        if matches!(state, crate::process::ProcessState::Blocked(_))
            && pcb.basic().name() != "idle"
        {
            let wp = pcb.last_wait_file.load(Ordering::Relaxed);
            let wl = pcb.last_wait_line.load(Ordering::Relaxed);
            let wlen = pcb.last_wait_len.load(Ordering::Relaxed);
            if !wp.is_null() && wlen > 0 && wlen < 200 {
                let bytes = unsafe { core::slice::from_raw_parts(wp, wlen) };
                let file = core::str::from_utf8(bytes).unwrap_or("<invalid>");
                println_critical!(
                    "[pid {:>6}] {:<16} wait={}:{}",
                    pcb.raw_pid().data(),
                    pcb.basic().name(),
                    file,
                    wl
                );
            }
        }
        // 用户态 Blocked 任务：打印其内核栈顶 TrapFrame 的关键寄存器，
        // 离线用 nm 反解符号定位卡死的系统调用路径（dockerd B+ 取证）。
        if matches!(state, crate::process::ProcessState::Blocked(_))
            && pcb.raw_pid().data() > 100
            && pcb.basic().name() != "idle"
        {
            unsafe {
                let kstack = pcb.kernel_stack_force_ref();
                let sp = kstack.stack_max_address().data();
                let tf = sp - core::mem::size_of::<crate::arch::interrupt::TrapFrame>();
                let tf_ptr = tf as *const crate::arch::interrupt::TrapFrame;
                if tf_ptr as usize >= kstack.start_address().data() {
                    let tfv = &*tf_ptr;
                    println_critical!(
                        "[pid {:>6}] {:<16} rip={:#x} LSYSCALL={} errcode={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}\n",
                        pcb.raw_pid().data(),
                        pcb.basic().name(),
                        core::ptr::read_volatile(&tfv.rip),
                        pcb.last_syscall.load(Ordering::Relaxed),
                        core::ptr::read_volatile(&tfv.errcode),
                        core::ptr::read_volatile(&tfv.rdi),
                        core::ptr::read_volatile(&tfv.rsi),
                        core::ptr::read_volatile(&tfv.rdx),
                        core::ptr::read_volatile(&tfv.r10)
                    );
                    // 自 TrapFrame 向下扫描内核栈，打印内核文本区间的
                    // 返回地址（离线 nm 反解得内核侧卡点调用链）。
                    let base = kstack.start_address().data();
                    let mut a = tf - 8;
                    let mut found = 0;
                    while a >= base && found < 12 {
                        let v = core::ptr::read_volatile(a as *const u64);
                        if (0xffff8000_0000_0000..0xffff8000_0200_0000).contains(&v) {
                            println_critical!("    kret -{:#x}: {:#x}", tf - a, v);
                            found += 1;
                        }
                        a -= 8;
                    }
                }
            }
        }
    }
    println_critical!("==== END TASK DUMP ====");
}

#[unified_init(INITCALL_DEVICE)]
fn stall_watchdog_init() -> Result<(), SystemError> {
    let closure = KernelThreadClosure::StaticEmptyClosure((
        &(stall_watchdog_thread as fn() -> i32),
        (),
    ));
    KernelThreadMechanism::create_and_run(closure, "stall_watchdog".to_string())
        .ok_or(SystemError::ENOMEM)
        .expect("create stall_watchdog thread failed");
    Ok(())
}

// ===== 串口输入触发的停摆取证 =====
// 定时器死亡时，依赖定时唤醒的看门狗也是受害者；而设备 IRQ（如控制台输入）
// 仍会触发。此探针在控制台输入 IRQ 中检测"jiffies 停滞但 TSC 仍在走"，
// 命中即转储定时器硬件状态与任务现场（经关键通道，IRQ 上下文安全）。

use core::arch::x86_64::_rdtsc;
use core::sync::atomic::Ordering;

use x86::msr::rdmsr;

/// x2APIC 定时器相关 MSR 地址
const MSR_X2APIC_INIT_COUNT: u32 = 0x838;
const MSR_X2APIC_CUR_COUNT: u32 = 0x839;
const MSR_X2APIC_DIV_CONF: u32 = 0x83d;
const MSR_X2APIC_LVT_TIMER: u32 = 0x83e;

/// 控制台输入 IRQ 的停摆探测入口（由 virtio-console 的 handle_irq 调用）
pub fn console_input_stall_probe() {
    static LAST_JIFFIES: AtomicU64 = AtomicU64::new(0);
    static LAST_TSC: AtomicU64 = AtomicU64::new(0);
    static FIRED: AtomicBool = AtomicBool::new(false);

    let jiffies = crate::time::timer::timer_jiffies_value();
    let Some(now_ns) = crate::driver::clocksource::kvm_clock::kvm_clock_read() else {
        return; // pvclock 不可用
    };
    let last_j = LAST_JIFFIES.swap(jiffies, Ordering::Relaxed);
    let last_ns = LAST_TSC.swap(now_ns, Ordering::Relaxed);
    if last_ns == 0 {
        return; // 首次调用，仅建立基准
    }
    if FIRED.load(Ordering::Relaxed) {
        return; // 只取证一次
    }
    let delta_ms = now_ns.saturating_sub(last_ns) / 1_000_000;
    if delta_ms > 3000 && jiffies == last_j {
        FIRED.store(true, Ordering::Relaxed);
        dump_stall_forensics(jiffies, now_ns, delta_ms);
        dump_all_tasks();
    }
}

/// 转储定时器硬件与软件状态（当前 CPU 的 x2APIC 视角）
fn dump_stall_forensics(jiffies: u64, now_ns: u64, delta_ms: u64) {
    println_critical!("==== STALL FORENSICS: jiffies frozen at {}, now_ns={} gap_ms={} ====",
        jiffies, now_ns, delta_ms);
    unsafe {
        let init_count = rdmsr(MSR_X2APIC_INIT_COUNT);
        let cur_count = rdmsr(MSR_X2APIC_CUR_COUNT);
        let div_conf = rdmsr(MSR_X2APIC_DIV_CONF);
        let lvt_timer = rdmsr(MSR_X2APIC_LVT_TIMER);
        println_critical!("[x2apic timer] LVT={:#x} DIV={:#x} INIT_COUNT={:#x} CUR_COUNT={:#x}",
            lvt_timer, div_conf, init_count, cur_count);
    }
    // deadline 队列软件状态原始内存转储（离线对照符号解析）
    let queues_ptr = crate::time::deadline::queues_raw_addr() as *const u64;
    unsafe {
        println_critical!(
            "[deadline queues raw] {:#x} {:#x} {:#x} {:#x}",
            queues_ptr.read_volatile(),
            queues_ptr.add(1).read_volatile(),
            queues_ptr.add(2).read_volatile(),
            queues_ptr.add(3).read_volatile()
        );
    }
    println_critical!("==== END STALL FORENSICS ====");
}
