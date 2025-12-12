//! Copied from SmolTCP's RawSocketDesc, with inspiration from
//! [https://github.com/embassy-rs/embassy](https://github.com/embassy-rs/embassy/blob/master/examples/std/src/tuntap.rs).

use crate::{
    ETHERCAT_ETHERTYPE,
    std::unix::{ifreq, ifreq_for},
};
use async_io::IoSafe;
use core::ptr::addr_of;
use std::{
    io, mem,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::io::{AsRawFd, RawFd},
    },
};

pub struct RawSocketDesc {
    lower: i32,   //套接字文件描述符
    ifreq: ifreq, //包含网卡名称
}

impl RawSocketDesc {
    //创建套接字，绑定到网卡，相当于SOEM的ecx_setupnic函数
    pub fn new(name: &str) -> io::Result<Self> {
        let protocol = ETHERCAT_ETHERTYPE as i16;

        //使用 unsafe 块调用 libc::socket 系统调用创建一个新的套接字
        let lower = unsafe {
            //创建一个原始套接字（raw socket），用于发送和接收原始的网络数据包。
            //AF_PACKET：指定了地址族（address family），用于指定底层网络协议。用于处理链路层数据包，如 Ethernet II 帧。
            //SOCK_RAW：指定了套接字类型（socket type）原始套接字，允许直接访问底层网络协议。
            //SOCK_NONBLOCK：指定了套接字标志（socket flag），用于非阻塞模式。
            //protocol.to_be() as i32：将协议号（protocol number）转换为大端字节序（big-endian byte order）的i32 类型
            let lower = libc::socket(
                // Ethernet II frames
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK,
                protocol.to_be() as i32,
            );
            //如果 libc::socket 调用失败（返回值为 -1）
            if lower == -1 {
                return Err(io::Error::last_os_error()); //返回 io::Error::last_os_error()，包含最后一次系统调用的错误信息。
            }
            lower
        };

        let mut self_ = RawSocketDesc {
            lower,
            ifreq: ifreq_for(name),
        };

        self_.bind_interface()?;

        Ok(self_)
    }

    //将套接字绑定到指定的网络接口上
    fn bind_interface(&mut self) -> io::Result<()> {
        let protocol = ETHERCAT_ETHERTYPE as i16;

        //创建一个 libc::sockaddr_ll 结构体，用于表示链路层套接字地址
        let sockaddr = libc::sockaddr_ll {
            sll_family: libc::AF_PACKET as u16, //指定地址族为 AF_PACKET，表示处理链路层数据包
            sll_protocol: protocol.to_be() as u16, //指定协议类型，使用大端字节序
            //调用 ifreq_ioctl 函数通过 SIOCGIFINDEX 命令获取网络接口的索引。
            sll_ifindex: ifreq_ioctl(self.lower, &mut self.ifreq, libc::SIOCGIFINDEX)?,
            sll_hatype: 1,    //指定硬件地址类型，1 通常代表以太网。
            sll_pkttype: 0,   //指定数据包类型，0 表示普通数据包。
            sll_halen: 6,     //指定硬件地址长度，以太网 MAC 地址长度为 6 字节。
            sll_addr: [0; 8], //初始化硬件地址为全 0。
        };

        //使用 unsafe 块调用 libc::bind 系统调用，将套接字绑定到指定的网络接口
        unsafe {
            #[allow(trivial_casts)]
            let res = libc::bind(
                self.lower,
                addr_of!(sockaddr).cast(),
                mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            );
            //若 libc::bind 调用失败（返回值为 -1），则返回 io::Error::last_os_error()，包含最后一次系统调用的错误信息。
            if res == -1 {
                return Err(io::Error::last_os_error()); //返回 io::Error::last_os_error()，包含最后一次系统调用的错误信息。
            }
        }

        Ok(())
    }

    //获取与该套接字关联的网络接口的最大传输单元（MTU）
    pub fn interface_mtu(&mut self) -> io::Result<usize> {
        //libc::SIOCGIFMTU：是一个 ioctl 命令，用于获取网络接口的 MTU（Maximum Transmission Unit）。
        ifreq_ioctl(self.lower, &mut self.ifreq, libc::SIOCGIFMTU).map(|mtu| mtu as usize)
    }
}

// 实现了 Unix 平台特有的 AsRawFd trait，用于获取底层原始文件描述符
// 允许 RawSocketDesc 类型与使用原始文件描述符的 C API（如 libc）交互
// 在代码中被 Read 和 Write 实现使用，通过 self.as_raw_fd() 获取底层文件描述符进行读写操作
// 提供了从 Rust 类型到操作系统级文件描述符的直接转换
impl AsRawFd for RawSocketDesc {
    fn as_raw_fd(&self) -> RawFd {
        self.lower
    }
}

// 实现了更现代的 AsFd trait，用于获取安全的借用文件描述符 (BorrowedFd)
// 提供了更安全的接口，返回 BorrowedFd<'_> 类型的引用，具有更好的生命周期管理
// 支持与 Rust 标准库中更现代的 I/O API 兼容，特别是那些期望接收 AsFd 实现的函数
// 使用 unsafe 块调用 BorrowedFd::borrow_raw 将原始文件描述符转换为安全的借用形式
impl AsFd for RawSocketDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.lower) }
    }
}

// 安全性：实现此特性可确保底层套接字资源不会被 `Read` 或 `Write` 实现丢弃。
// 更多信息请参阅
// [此处](https://docs.rs/async-io/latest/async_io/trait.IoSafe.html)。
// SAFETY: Implementing this trait pledges that the underlying socket resource will not be dropped
// by `Read` or `Write` impls. More information can be read
// [here](https://docs.rs/async-io/latest/async_io/trait.IoSafe.html).
unsafe impl IoSafe for RawSocketDesc {}

impl Drop for RawSocketDesc {
    fn drop(&mut self) {
        unsafe {
            // 调用 C 标准库函数关闭底层套接字文件描述符
            libc::close(self.lower);
        }
    }
}

/// 为 RawSocketDesc 实现标准库的 Read 特性
/// 允许通过标准的 Rust I/O 接口从 EtherCAT 原始套接字读取数据
impl io::Read for RawSocketDesc {
    /// 从套接字读取数据到指定缓冲区
    ///
    /// # 参数
    /// - `buf`: 用于存储读取数据的可变缓冲区
    ///
    /// # 返回值
    /// - `Ok(usize)`: 成功读取的字节数
    /// - `Err(io::Error)`: 读取失败时返回的错误信息
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // 使用 unsafe 块调用 libc::read 函数从原始套接字读取数据
        // 原因：涉及 FFI 调用和原始指针操作，Rust 编译器无法保证其安全性
        let len = unsafe {
            libc::read(
                self.as_raw_fd(),        // 获取底层原始文件描述符
                buf.as_mut_ptr().cast(), // 将 Rust 可变缓冲区指针转换为 C 指针
                buf.len(),               // 要读取的最大字节数
            )
        };

        // 检查读取是否失败（len == -1 表示失败）
        if len == -1 {
            Err(io::Error::last_os_error()) // 返回系统级错误信息
        } else {
            Ok(len as usize) // 成功读取，返回实际读取的字节数
        }
    }
}

/// 为 RawSocketDesc 实现标准库的 Write 特性
/// 允许通过标准的 Rust I/O 接口向 EtherCAT 原始套接字写入数据
impl io::Write for RawSocketDesc {
    /// 将数据从缓冲区写入套接字
    ///
    /// # 参数
    /// - `buf`: 包含要写入数据的缓冲区
    ///
    /// # 返回值
    /// - `Ok(usize)`: 成功写入的字节数
    /// - `Err(io::Error)`: 写入失败时返回的错误信息
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // 使用 unsafe 块调用 libc::write 函数向原始套接字写入数据
        // 原因：涉及 FFI 调用和原始指针操作，Rust 编译器无法保证其安全性
        let len = unsafe {
            libc::write(
                self.as_raw_fd(),    // 获取底层原始文件描述符
                buf.as_ptr().cast(), // 将 Rust 缓冲区指针转换为 C 指针
                buf.len(),           // 要写入的字节数
            )
        };

        // 检查写入是否失败（len == -1 表示失败）
        if len == -1 {
            Err(io::Error::last_os_error()) // 返回系统级错误信息
        } else {
            Ok(len as usize) // 成功写入，返回实际写入的字节数
        }
    }

    /// 刷新内部缓冲区
    ///
    /// # 注意
    /// 对于原始套接字，此方法是一个空操作（no-op），因为套接字没有内部缓冲区
    /// 数据会直接发送到底层网络接口
    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // 总是成功，无需实际操作
    }
}

// 用于执行 ioctl 系统调用
fn ifreq_ioctl(
    lower: libc::c_int,
    ifreq: &mut ifreq,
    cmd: libc::c_ulong,
) -> io::Result<libc::c_int> {
    unsafe {
        #[allow(trivial_casts)]
        #[cfg(target_env = "musl")]
        let res = libc::ioctl(lower, cmd as libc::c_int, ifreq as *mut ifreq);
        #[allow(trivial_casts)]
        #[cfg(not(target_env = "musl"))]
        let res = libc::ioctl(lower, cmd, ifreq as *mut ifreq);

        if res == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(ifreq.ifr_data)
}
