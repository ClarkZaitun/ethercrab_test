//! Items to use when not in `no_std` environments.

#[cfg(all(not(target_os = "linux"), unix))]
mod bpf;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(all(not(target_os = "linux"), unix))]
use self::bpf::BpfDevice as RawSocketDesc;
#[cfg(target_os = "linux")]
pub(in crate::std) use self::linux::RawSocketDesc;

use crate::{
    error::Error,
    fmt,
    pdu_loop::{PduRx, PduTx},
};
use async_io::Async;
use core::{future::Future, pin::Pin, task::Poll};
use futures_lite::{AsyncRead, AsyncWrite};

//<'a> 是一个生命周期参数，用于确保结构体中引用类型的生命周期与外部数据的生命周期保持一致，避免悬垂引用。
struct TxRxFut<'a> {
    socket: Async<RawSocketDesc>, //Async 来自 async_io 库，它能将阻塞的 I/O 操作转换为异步操作。
    mtu: usize,                   //获取当前套接字关联网络接口的 MTU 值有什么用？
    tx: Option<PduTx<'a>>,        //整个程序公用一个帧缓冲区，用于存储发送和接收的 PDU 帧。
    //在 TxRxFut 结构体中，tx 和 rx 字段分别用于存储 PduTx 和 PduRx 类型的引用。
    //PduTx 用于发送 PDU 帧，PduRx 用于接收 PDU 帧。
    //通过将 tx 和 rx 字段声明为 Option 类型，TxRxFut 结构体可以在不提前知道 PduTx 和 PduRx 实例的情况下，
    //在运行时动态地分配和释放它们的内存。这样可以避免在编译时就确定 PduTx 和 PduRx 实例的大小，
    //从而减少了程序的内存占用。
    rx: Option<PduRx<'a>>, //使用 Option 类型包装 PduTx 和 PduRx，允许在某些情况下这些对象为空
}

//实现Future trait
impl<'a> Future for TxRxFut<'a> {
    //type Output 是 Future trait 里的关联类型，用于指定 TxRxFut 这个 Future 完成时产生的值的类型
    //意味着所有以这个trait 为返回值的函数，都必须返回一个Result类型？
    type Output = Result<(PduTx<'a>, PduRx<'a>), Error>;

    //尝试发送和接收 EtherCAT 数据，若操作完成就返回 Poll::Ready，若操作未完成则返回 Poll::Pending
    //发送多个，只接收一个？
    fn poll(mut self: Pin<&mut Self>, ctx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        unsafe {
            //更新唤醒器waker
            // Re-register waker to make sure this future is polled again
            self.tx //是 Option<PduTx<'a>> 类型的字段，代表 PDU 发送器。
                .as_mut() //将 Option 转换为可变引用，方便修改内部值。
                .unwrap_unchecked() // 直接获取 Option 内部的 PduTx 可变引用，不进行 None 检查。
                .replace_waker(ctx.waker()); //调用 PduTx 的 replace_waker 方法，将当前任务的唤醒器 ctx.waker() 替换到发送器中。
            //这能确保当发送器有新的可发送数据时，当前的 Future 会被再次轮询。

            // 如果检测到退出标志，则释放 PduTx 和 PduRx，返回 Ok((PduTx, PduRx))，退出这个异步任务
            if self.tx.as_mut().unwrap_unchecked().should_exit() {
                //调用 PduTx 的 should_exit 方法，
                fmt::debug!("TX/RX future was asked to exit");

                return Poll::Ready(Ok((
                    self.tx.take().unwrap().release(),
                    self.rx.take().unwrap().release(),
                )));
            }
        }

        // 发送所有待发送帧
        while let Some(frame) = unsafe { self.tx.as_mut().unwrap_unchecked() }.next_sendable_frame()
        //获取到下一个要发送的帧
        {
            let res = frame.send_blocking(|data| {
                //将 self.socket 的可变引用包装在 Pin 中。Pin 用于固定对象的内存位置，确保在异步操作过程中对象不会被移动，这对实现 Future 的对象尤为重要
                //用 AsyncWrite trait 的 poll_write 方法，尝试将 data 写入 self.socket。ctx 是 core::task::Context 类型的引用，包含任务的唤醒器 waker，用于在 I/O 操作就绪时唤醒任务
                match Pin::new(&mut self.socket).poll_write(ctx, data) {
                    Poll::Ready(Ok(bytes_written)) => {
                        if bytes_written != data.len() {
                            fmt::error!("Only wrote {} of {} bytes", bytes_written, data.len());

                            Err(Error::PartialSend {
                                len: data.len(),
                                sent: bytes_written,
                            })
                        } else {
                            Ok(bytes_written)
                        }
                    }

                    Poll::Ready(Err(e)) => {
                        fmt::error!("Send PDU failed: {}", e);

                        Err(Error::SendFrame)
                    }
                    Poll::Pending => Ok(0),
                }
            });

            if let Err(e) = res {
                fmt::error!("Send PDU failed: {}", e);

                return Poll::Ready(Err(e));
            }
        }
        // 低效？：运行时创建存放帧的缓冲区
        let mut buf = vec![0; self.mtu];

        //poll_read(ctx, &mut buf)：调用 AsyncRead trait 的 poll_read 方法，尝试从 self.socket 读取数据到 buf 中。
        //ctx 是 core::task::Context 类型的引用，包含任务的唤醒器 waker，用于在 I/O 操作就绪时唤醒任务。
        match Pin::new(&mut self.socket).poll_read(ctx, &mut buf) {
            //处理读取成功的情况
            Poll::Ready(Ok(n)) => {
                fmt::trace!("Poll ready");
                //唤醒当前任务，确保后续可能还有数据帧需要处理，特别是在 macOS 系统中，一次 poll_read 调用可能接收多个数据包，但这些数据包可能在下次 poll_read 时才返回
                // Wake again in case there are more frames to consume. This is additionally
                // important for macOS as multiple packets may be received for one `poll_read`
                // call, but will only be returned during the _next_ `poll_read`. If this line
                // is removed, PDU response frames are missed, causing timeout errors.
                ctx.waker().wake_by_ref();

                //从 buf 中截取实际读取的字节数，得到接收到的数据包。如果截取失败，返回 Error::Internal 错误
                let packet = buf.get(0..n).ok_or(Error::Internal)?;

                if n == 0 {
                    fmt::warn!("Received zero bytes");
                }

                //解析以太网 II 帧的 EtherCAT 协议数据单元（PDU），切换帧状态，唤醒帧任务
                if let Err(e) = unsafe { self.rx.as_mut().unwrap_unchecked() }.receive_frame(packet)
                {
                    fmt::error!("Failed to receive frame: {}", e);

                    return Poll::Ready(Err(Error::ReceiveFrame));
                }
            }
            //处理读取失败的情况
            Poll::Ready(Err(e)) => {
                fmt::error!("Receive PDU failed: {}", e);
            }
            //处理读取未完成的情况
            Poll::Pending => (), //表示读取操作尚未完成，需要稍后再次尝试。这里不做任何处理。
        }

        Poll::Pending
    }
}

//根据给定的网络接口名称、PDU 发送器和 PDU 接收器，创建一个异步任务。该任务会在指定的网络接口上进行 PDU 的发送和接收操作，并在任务完成时返回最终的发送器和接收器实例，或者相应的错误信息。
/// Spawn a TX and RX task.
pub fn tx_rx_task<'sto>(
    interface: &str,
    pdu_tx: PduTx<'sto>,
    #[allow(unused_mut)] mut pdu_rx: PduRx<'sto>, //#[allow(unused_mut)] 注解用于告诉编译器忽略 pdu_rx 变量可变但未被修改的警告。
) -> Result<impl Future<Output = Result<(PduTx<'sto>, PduRx<'sto>), Error>> + 'sto, std::io::Error>
//impl Future 表明函数返回一个实现了 Future trait 的类型，不过具体类型由编译器推断，调用者无需关心。
//Output 是 Future trait 里的关联类型，用于指定 Future 完成时产生的值的类型
//Ok 变体包含一个元组 (PduTx<'sto>, PduRx<'sto>)，PduTx<'sto> 和 PduRx<'sto> 分别是用于发送和接收协议数据单元（PDU）的类型，'sto 是生命周期参数，确保这些类型中的引用在 'sto 生命周期内有效。
//+ 'sto 是生命周期约束，表明返回的 Future 类型中所有引用的生命周期至少要和 'sto 一样长。这能保证在 'sto 生命周期内，Future 持有的引用不会失效。
// 总结：返回的 task 类型 TxRxFut<'sto> 实现了 Future trait，其 Output 类型就是 Result<(PduTx<'sto>, PduRx<'sto>), Error>
{
    let mut socket = RawSocketDesc::new(interface)?;

    // macOS forcibly sets the source address to the NIC's MAC, so instead of using `MASTER_ADDR`
    // for filtering returned packets, we must set the address to compare to the NIC MAC.
    #[cfg(all(not(target_os = "linux"), unix))]
    if let Some(mac) = socket.mac().ok().flatten() {
        fmt::debug!("Setting source MAC to {}", mac);

        pdu_rx.set_source_mac(mac);
    }

    let mtu = socket.interface_mtu()?;

    fmt::debug!("Opening {} with MTU {}", interface, mtu);

    //将一个阻塞的 I/O 对象转换为支持异步操作的对象
    let async_socket = Async::new(socket)?;
    //由于原始套接字的 I/O 操作默认是阻塞的，无法直接在异步代码里高效使用。
    //因此，借助 Async::new 函数将其转换为支持异步操作的对象，后续就能在 TxRxFut 结构体的 poll 方法中使用 async_socket 进行异步读写操作了。

    let task = TxRxFut {
        socket: async_socket,
        mtu,
        tx: Some(pdu_tx),
        rx: Some(pdu_rx),
    };

    Ok(task)
}

// 从OS获取用EtherCAT体系表示的时间（从2000-01-01开始的时间，而不是从1970年开始的时间）
/// Get the current time in nanoseconds from the EtherCAT epoch, 2000-01-01.
///
/// On POSIX systems, this function uses the monotonic clock provided by the system.
pub fn ethercat_now() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    //调用 C 标准库函数 clock_gettime 来获取系统单调时间
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    };

    // 时间单位转换为ns
    let t = (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64);

    // EtherCAT epoch is 2000-01-01
    t.saturating_sub(946684800) //转换为从2000-01-01开始的时间
}

// Unix only
#[allow(trivial_numeric_casts)]
fn ifreq_for(name: &str) -> ifreq {
    let mut ifreq = ifreq {
        ifr_name: [0; libc::IF_NAMESIZE],
        ifr_data: 0,
    };
    for (i, byte) in name.as_bytes().iter().enumerate() {
        ifreq.ifr_name[i] = *byte as libc::c_char;
    }
    ifreq
}

#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
struct ifreq {
    ifr_name: [libc::c_char; libc::IF_NAMESIZE],
    ifr_data: libc::c_int, /* ifr_ifindex or ifr_mtu */
}
