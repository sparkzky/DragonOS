// SPDX-License-Identifier: MPL-2.0
extern crate alloc;

use crate::prelude::*;

/// Ext4Error number.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    /// Operation not permitted.
    EPERM = 1,
    /// No such file or directory.
    ENOENT = 2,
    /// I/O error.
    EIO = 5,
    /// Resource temporarily unavailable.
    EAGAIN = 11,
    /// No such device or address.
    ENXIO = 6,
    /// Argument list too long.
    E2BIG = 7,
    /// Out of memory.
    ENOMEM = 12,
    /// Permission denied.
    EACCES = 13,
    /// Bad address.
    EFAULT = 14,
    /// File exists.
    EEXIST = 17,
    /// No such device.
    ENODEV = 19,
    /// Not a directory.
    ENOTDIR = 20,
    /// Is a directory.
    EISDIR = 21,
    /// Invalid argument.
    EINVAL = 22,
    /// File too large.
    EFBIG = 27,
    /// No space left on device.
    ENOSPC = 28,
    /// Read-only file system.
    EROFS = 30,
    /// Too many links.
    EMLINK = 31,
    /// Math result not representable.
    ERANGE = 34,
    /// File name too long.
    ENAMETOOLONG = 36,
    /// Directory not empty.
    ENOTEMPTY = 39,
    /// No data available.
    ENODATA = 61,
    /// Not supported.
    ENOTSUP = 95,
}

/// error used in this crate
pub struct Ext4Error {
    code: ErrCode,
    message: Option<String>,
}

impl Debug for Ext4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(message) = &self.message {
            write!(
                f,
                "Ext4Error {{ code: {:?}, message: {:?} }}",
                self.code, message
            )
        } else {
            write!(f, "Ext4Error {{ code: {:?} }}", self.code)
        }
    }
}

impl Ext4Error {
    #[track_caller]
    pub fn new(code: ErrCode) -> Self {
        if code == ErrCode::ENOTSUP {
            // 诊断：ENOTSUP 在转换为内核 SystemError 时只剩 errno 95，
            // 这里记录一次，用于定位"operation not supported"的来源。
            static REPORTED: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !REPORTED.swap(true, core::sync::atomic::Ordering::AcqRel) {
                log::error!("another_ext4: ENOTSUP created at {}", core::panic::Location::caller());
            }
        }
        Ext4Error {
            code,
            message: None,
        }
    }

    /// 构造带原因说明的错误。
    ///
    /// 命中 `ENOTSUP` 时先打印原因：该错误码在转换成内核 `SystemError`
    /// 时会丢弃 message，只剩下 errno 95，无法定位具体是哪个"不支持"路径。
    pub fn with_message(code: ErrCode, message: String) -> Self {
        if code == ErrCode::ENOTSUP {
            log::warn!("another_ext4: ENOTSUP: {}", message);
        }
        Ext4Error {
            code,
            message: Some(message),
        }
    }

    pub const fn code(&self) -> ErrCode {
        self.code
    }
}

#[macro_export]
macro_rules! format_error {
    ($code: expr, $message: expr) => {
        $crate::error::Ext4Error::with_message($code, format!($message))
    };
    ($code: expr, $fmt: expr,  $($args:tt)*) => {
        $crate::error::Ext4Error::with_message($code, format!($fmt, $($args)*))
    };
}

#[macro_export]
macro_rules! return_error {
    ($code: expr, $message: expr) => {
        return Err($crate::format_error!($code, $message));
    };
    ($code: expr, $fmt: expr,  $($args:tt)*) => {
        return Err($crate::format_error!($code, $fmt, $($args)*));
    }
}
