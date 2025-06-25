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

impl AsRawFd for RawSocketDesc {
    fn as_raw_fd(&self) -> RawFd {
        self.lower
    }
}

impl AsFd for RawSocketDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.lower) }
    }
}

// SAFETY: Implementing this trait pledges that the underlying socket resource will not be dropped
// by `Read` or `Write` impls. More information can be read
// [here](https://docs.rs/async-io/latest/async_io/trait.IoSafe.html).
unsafe impl IoSafe for RawSocketDesc {}

impl Drop for RawSocketDesc {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.lower);
        }
    }
}

impl io::Read for RawSocketDesc {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = unsafe { libc::read(self.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if len == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(len as usize)
        }
    }
}

impl io::Write for RawSocketDesc {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = unsafe { libc::write(self.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if len == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(len as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

//用于执行 ioctl 系统调用
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
