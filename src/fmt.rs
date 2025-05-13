#![macro_use]
#![allow(unused_macros)]
#![allow(unused)]

use core::fmt::{Debug, Display, LowerHex};

#[cfg(all(feature = "defmt", feature = "log"))]
compile_error!("The `defmt` and `log` features may not be enabled at the same time");

macro_rules! assert_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::assert!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::assert!($($x)*);
        }
    };
}

macro_rules! assert_eq_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::assert_eq!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::assert_eq!($($x)*);
        }
    };
}

macro_rules! assert_ne_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::assert_ne!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::assert_ne!($($x)*);
        }
    };
}

macro_rules! debug_assert_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::debug_assert!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::debug_assert!($($x)*);
        }
    };
}

macro_rules! debug_assert_eq_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::debug_assert_eq!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::debug_assert_eq!($($x)*);
        }
    };
}

macro_rules! debug_assert_ne_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::debug_assert_ne!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::debug_assert_ne!($($x)*);
        }
    };
}

macro_rules! todo_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::todo!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::todo!($($x)*);
        }
    };
}

macro_rules! unreachable_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::unreachable!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::unreachable!($($x)*);
        }
    };
}

macro_rules! panic_ {
    ($($x:tt)*) => {
        {
            #[cfg(not(feature = "defmt"))]
            ::core::panic!($($x)*);
            #[cfg(feature = "defmt")]
            ::defmt::panic!($($x)*);
        }
    };
}

// 定义宏，可以配置打印的库或者不打印
macro_rules! trace_ {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "log")]
            ::log::trace!($s $(, $x)*);
            #[cfg(feature = "defmt")]
            ::defmt::trace!($s $(, $x)*);
            #[cfg(not(any(feature = "log", feature="defmt")))]
            let _ = ($( & $x ),*);
        }
    };
}

// 依据不同的编译特性，输出调试级别的日志信息
macro_rules! debug_ {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "log")]
            ::log::debug!($s $(, $x)*);
            #[cfg(feature = "defmt")]
            ::defmt::debug!($s $(, $x)*);
            #[cfg(not(any(feature = "log", feature="defmt")))]
            let _ = ($( & $x ),*);
        }
    };
}

macro_rules! info_ {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "log")]
            ::log::info!($s $(, $x)*);
            #[cfg(feature = "defmt")]
            ::defmt::info!($s $(, $x)*);
            #[cfg(not(any(feature = "log", feature="defmt")))]
            let _ = ($( & $x ),*);
        }
    };
}

macro_rules! warn_ {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "log")]
            ::log::warn!($s $(, $x)*);
            #[cfg(feature = "defmt")]
            ::defmt::warn!($s $(, $x)*);
            #[cfg(not(any(feature = "log", feature="defmt")))]
            let _ = ($( & $x ),*);
        }
    };
}

macro_rules! error_ {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "log")]
            ::log::error!($s $(, $x)*);
            #[cfg(feature = "defmt")]
            ::defmt::error!($s $(, $x)*);
            #[cfg(not(any(feature = "log", feature="defmt")))]
            let _ = ($( & $x ),*);
        }
    };
}

#[cfg(feature = "defmt")]
macro_rules! unwrap_ {
    ($($x:tt)*) => {
        ::defmt::unwrap!($($x)*)
    };
}

#[cfg(not(feature = "defmt"))]
// 对 Result 类型的值进行解包操作。若 Result 是 Ok 变体，宏会返回其中的值；若为 Err 变体，则触发 panic
macro_rules! unwrap_ {
    ($arg:expr) => {
        match $arg {
            ::core::result::Result::Ok(t) => t,
            ::core::result::Result::Err(e) => {
                ::core::panic!("unwrap of `{}` failed: {:?}", ::core::stringify!($arg), e);
            }
        }
    };
    ($arg:expr, $($msg:expr),+ $(,)? ) => {
        match $arg {
            ::core::result::Result::Ok(t) => t,
            ::core::result::Result::Err(e) => {
                ::core::panic!("unwrap of `{}` failed: {}: {:?}", ::core::stringify!($arg), ::core::format_args!($($msg,)*), e);
            }
        }
    }
}

#[cfg(feature = "defmt")]
macro_rules! unwrap_opt_ {
    ($($x:tt)*) => {
        ::defmt::unwrap!($($x)*)
    };
}

// 对 Option 类型的值进行解包操作，若 Option 为 Some 则返回其中的值，若为 None 则触发 panic
#[cfg(not(feature = "defmt"))] //该宏仅在未启用 defmt 特性时生效
macro_rules! unwrap_opt_ {
    // 单参数匹配规则
    // 接受一个表达式作为参数，$arg 代表传入的 Option 类型表达式
    ($arg:expr) => {
        match $arg {
            ::core::option::Option::Some(t) => t,
            ::core::option::Option::None => {
                ::core::panic!("unwrap of `{}` failed", ::core::stringify!($arg));
            }
        }
    };
    // 多参数匹配规则
    // 接受一个 Option 类型表达式 $arg 和至少一个额外的消息表达式 $($msg:expr),+
    // $(,)? 表示最后一个参数后的逗号是可选的
    ($arg:expr, $($msg:expr),+ $(,)? ) => {
        match $arg {
            ::core::option::Option::Some(t) => t,
            ::core::option::Option::None => {
                // 错误信息除了包含尝试解包的表达式名称，还会包含用户传入的额外消息
                ::core::panic!("unwrap of `{}` failed: {}", ::core::stringify!($arg), ::core::format_args!($($msg,)*));
            }
        }
    }
}

pub(crate) use assert_ as assert;
pub(crate) use assert_eq_ as assert_eq;
pub(crate) use assert_ne_ as assert_ne;
pub(crate) use debug_ as debug;
pub(crate) use debug_assert_ as debug_assert;
pub(crate) use debug_assert_eq_ as debug_assert_eq;
pub(crate) use debug_assert_ne_ as debug_assert_ne;
pub(crate) use error_ as error;
pub(crate) use info_ as info;
pub(crate) use panic_ as panic;
pub(crate) use todo_ as todo;
pub(crate) use trace_ as trace;
pub(crate) use unreachable_ as unreachable;
pub(crate) use unwrap_ as unwrap;
pub(crate) use unwrap_opt_ as unwrap_opt;
pub(crate) use warn_ as warn;
