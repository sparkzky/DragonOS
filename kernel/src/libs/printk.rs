use core::fmt::{self, Write};

use alloc::string::ToString;
use log::{info, Level, Log};

use super::lib_ui::textui::{textui_putstr, FontColor};

use crate::{
    debug::klog::loglevel::{LogLevel, KERNEL_LOG_LEVEL},
    driver::tty::{tty_driver::TtyOperation, virtual_terminal::vc_manager},
    filesystem::procfs::{klog::LogMessage, kmsg::KMSG},
    time::PosixTimeSpec,
};

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::libs::printk::__printk(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n");
    };
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}


/// 关键路径打印：绕过 tty 背压，直接同步送入控制台设备。
#[macro_export]
macro_rules! print_critical {
    ($($arg:tt)*) => ($crate::libs::printk::__printk_critical(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println_critical {
    () => ($crate::print_critical!("\n"));
    ($($arg:tt)*) => ($crate::print_critical!("{}\n", format_args!($($arg)*)));
}

pub struct PrintkWriter;

impl PrintkWriter {
    #[inline]
    pub fn __write_fmt(&mut self, args: fmt::Arguments) {
        self.write_fmt(args).ok();
    }

    /// 并输出白底黑字
    /// @param str: 要写入的字符
    pub fn __write_string(&mut self, s: &str) {
        if let Some(current_vc) = vc_manager().current_vc() {
            // tty已经初始化了之后才输出到屏幕
            let port = current_vc.port();
            let tty = port.port_data().internal_tty();
            if let Some(tty) = tty {
                let _ = tty.write(tty.core(), s.as_bytes(), s.len());
            } else {
                let _ = textui_putstr(s, FontColor::WHITE, FontColor::BLACK);
            }
        } else {
            let _ = textui_putstr(s, FontColor::WHITE, FontColor::BLACK);
        }
    }
}

/// 为Printk Writer实现core::fmt::Write, 使得能够借助Rust自带的格式化组件，格式化字符并输出
impl fmt::Write for PrintkWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.__write_string(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn __printk(args: fmt::Arguments) {
    PrintkWriter.write_fmt(args).unwrap();
}

/// 关键路径写入器：优先使用不受 tty 背压影响的同步控制台通道；
/// 通道不存在（如 virtio-console 尚未探测、或使用其他控制台）时退回
/// 普通 printk 路径。用于 error/warn/panic 输出，保证消息体不被丢弃。
pub struct CriticalPrintkWriter;

impl CriticalPrintkWriter {
    pub fn write_critical_string(&mut self, s: &str) {
        if crate::driver::char::virtio_console::critical_output_available() {
            crate::driver::char::virtio_console::virtio_console_critical_output(s.as_bytes());
        } else {
            PrintkWriter.__write_string(s);
        }
    }
}

impl fmt::Write for CriticalPrintkWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_critical_string(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn __printk_critical(args: fmt::Arguments) {
    CriticalPrintkWriter.write_fmt(args).ok();
}

// ---------------------------------------------------------------------------
// 零分配紧急输出通道
//
// 供内核堆耗尽、嵌套 panic 等无法再依赖任何堆分配的路径使用。
// 与 CriticalPrintkWriter（关键路径、免背压）不同，这里连设备驱动的锁与
// 缓冲都不碰：x86_64 上直接轮询 COM1 (0x3F8) 的 LSR 后逐字节写端口，
// 无锁、无堆、有限自旋，任何上下文安全。
// ---------------------------------------------------------------------------

/// 直接向 COM1 写一个字节（不含任何锁与堆分配）。
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn emergency_com1_putc(byte: u8) {
    const COM1: u16 = 0x3f8;
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") COM1,
            in("al") byte,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// 读取 COM1 的 LSR（线状态寄存器）。
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn emergency_com1_lsr() -> u8 {
    const LSR: u16 = 0x3f8 + 5;
    let value: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") LSR,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// 零分配地输出一段静态字符串。
///
/// 每个字节最多自旋等待 THR 空位有限次；无串口时直接丢弃字节，
/// 保证任何环境下都有限返回。
#[cfg(target_arch = "x86_64")]
pub fn emergency_print_noalloc(s: &str) {
    const LSR_THRE: u8 = 0x20;
    for &byte in s.as_bytes() {
        let mut spins: u32 = 0;
        while unsafe { emergency_com1_lsr() } & LSR_THRE == 0 && spins < 100_000 {
            spins += 1;
        }
        unsafe { emergency_com1_putc(byte) };
    }
}

/// 零分配地输出一个 u64 的十进制表示（栈上格式化）。
#[cfg(target_arch = "x86_64")]
pub fn emergency_print_u64_noalloc(mut value: u64) {
    let mut buf = [0u8; 20];
    let mut end = buf.len();
    if value == 0 {
        end -= 1;
        buf[end] = b'0';
    }
    while value > 0 {
        end -= 1;
        buf[end] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    let s = core::str::from_utf8(&buf[end..]).unwrap_or("?");
    emergency_print_noalloc(s);
}

/// 非 x86_64 架构暂无可用的零分配通道，退化为空实现。
#[cfg(not(target_arch = "x86_64"))]
pub fn emergency_print_noalloc(_s: &str) {}

/// 非 x86_64 架构暂无可用的零分配通道，退化为空实现。
#[cfg(not(target_arch = "x86_64"))]
pub fn emergency_print_u64_noalloc(_value: u64) {}
pub struct Logger;

impl Logger {
    pub fn log(&self, log_level: LogLevel, message: fmt::Arguments) {
        if unsafe { KMSG.is_some() } {
            let timestamp: PosixTimeSpec = PosixTimeSpec::now_cpu_time();

            // to_string 的分配在锁外完成；push 返回的驱逐消息也必须
            // 在 KMSG 锁释放后再 drop，锁内只做无分配的槽位操作。
            let log_message = LogMessage::new(timestamp, log_level, message.to_string());

            let evicted =
                unsafe { KMSG.as_ref().unwrap().lock_irqsave().push(log_message) };
            drop(evicted);
        }
    }
}

/// 内核自定义日志器
///
/// todo: https://github.com/DragonOS-Community/DragonOS/issues/762
struct KernelLogger;

impl Log for KernelLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // 根据全局日志级别过滤日志
        KERNEL_LOG_LEVEL.should_print(metadata.level().into())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // 先输出到控制台再记录 kmsg：记录路径（锁+缓冲驱逐）一旦
            // 异常，现场仍然已经上了串口，不会被记录路径吞掉。
            Self::iodisplay(record);
            Self::kernel_log(record);
        }
    }

    fn flush(&self) {
        // 如果需要的话，可以在这里实现缓冲区刷新逻辑
    }
}

impl KernelLogger {
    fn iodisplay(record: &log::Record) {
        // Error/Warn 走关键同步通道：消息体不得被 tty 背压丢弃，否则
        // panic/IO 错误只剩级别前缀，现场无法排障。关键路径经
        // CriticalPrintkWriter 流式输出，不额外分配内存。
        if matches!(record.level(), Level::Error | Level::Warn) {
            let prefix = if matches!(record.level(), Level::Error) {
                "\x1B[41m[ ERROR ] \x1B[0m"
            } else {
                "\x1B[1;33m[ WARN ] \x1B[0m"
            };
            CriticalPrintkWriter.write_critical_string(prefix);
            let _ = CriticalPrintkWriter.write_fmt(format_args!(
                "({}:{})\t {}\n",
                record.file().unwrap_or(""),
                record.line().unwrap_or(0),
                record.args()
            ));
            return;
        }
        write!(PrintkWriter, "[ {} ] ", record.level(),).unwrap();
        writeln!(
            PrintkWriter,
            "({}:{})\t {}",
            record.file().unwrap_or(""),
            record.line().unwrap_or(0),
            record.args()
        )
        .unwrap();
    }

    fn kernel_log(record: &log::Record) {
        match record.level() {
            Level::Debug | Level::Trace => Logger.log(
                LogLevel::DEBUG,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Error => Logger.log(
                LogLevel::ERR,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Info => Logger.log(
                LogLevel::INFO,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Warn => Logger.log(
                LogLevel::WARN,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
        }
    }
}

pub fn early_init_logging() {
    log::set_logger(&KernelLogger).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    info!("Logging initialized");
}
