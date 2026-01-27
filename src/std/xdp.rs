use crate::{PduRx, PduTx, error::Error, fmt, pdu_loop::ReceiveAction, std::unix::RawSocketDesc};
use core::{num::NonZeroU32, str::FromStr, task::Waker};
use std::{
    io::{self, Write},
    sync::Arc,
    task::Wake,
    thread::{self, Thread},
    time::Instant,
};
use xsk_rs::{
    CompQueue, FillQueue, FrameDesc, RxQueue, TxQueue, Umem,
    config::{BindFlags, Interface, SocketConfig, UmemConfig},
};

struct ParkSignal {
    current_thread: Thread,
}

impl ParkSignal {
    fn new() -> Self {
        Self {
            current_thread: thread::current(),
        }
    }

    fn wait(&self) {
        thread::park();
    }

    // fn wait_timeout(&self, timeout: Duration) {
    //     thread::park_timeout(timeout)
    // }
}

impl Wake for ParkSignal {
    fn wake(self: Arc<Self>) {
        self.current_thread.unpark();
    }
}

/// Start a TX/RX task using XDP (Linux only).
///
/// Note that running this function on the same core as other EtherCrab code will cause a lockup.
/// Use [`core_affinity`] or other means to move the thread that executes this function to a
/// different core.
///
/// Using XDP requires some build-time dependencies. These can be installed on `deb`-based distros
/// as follows:
///
/// ```bash
/// sudo apt install build-essential m4 clang bpftool libelf-dev libpcap-dev
/// ```
///
/// Ubuntu 22.04 does not provide a `bpftool` package. Instead, install `linux-tools-common
/// linux-tools-$(uname -r)`.
///
/// It may also be necessary to symlink some folders to mitigate an error around `asm/types.h` not
/// being found:
///
/// ```bash
/// sudo ln -s /usr/include/asm-generic/ /usr/include/asm
/// ```
///
// 零拷贝设计：XDP 允许内核直接访问用户空间内存，避免了传统网络栈中的多次数据拷贝
// 高效中断处理：XDP_USE_NEED_WAKEUP 标志优化了中断唤醒机制
// 内存管理：预分配和划分内存描述符，避免运行时分配开销
pub fn tx_rx_task_xdp<'sto>(
    interface: &str,
    mut pdu_tx: PduTx<'sto>,
    mut pdu_rx: PduRx<'sto>,
) -> Result<(), io::Error> {
    let mut socket = RawSocketDesc::new(interface)?;

    let mtu = socket.interface_mtu()?;

    fmt::debug!(
        "Opening {} with MTU {}, blocking, using XDP",
        interface,
        mtu
    );

    // 获得 PDU 帧数量：根据 PDU 存储的容量计算需要的帧数量
    let frame_count = (pdu_tx.capacity() as u32)
        .try_into()
        .expect("Non-zero frame count required");

    fmt::debug!("Frame count {}", frame_count);

    // ParkSignal：创建一个用于线程 park/unpark 的同步原语
    let signal = Arc::new(ParkSignal::new());
    // 基于 ParkSignal 创建一个 Waker 对象，用于 Rust 异步任务的唤醒机制
    let waker = Waker::from(Arc::clone(&signal));

    //  Linux XDP (eXpress Data Path) 套接字的初始化和配置
    let config = SocketConfig::builder()
        .bind_flags(BindFlags::XDP_USE_NEED_WAKEUP)
        .build();
    // 唤醒优化标志：设置 XDP_USE_NEED_WAKEUP 标志，这是一个关键优化
    // 告诉 XDP 驱动程序只有在数据到达时才唤醒用户空间程序
    // 减少不必要的唤醒，显著降低 CPU 使用率和系统开销

    // 创建 XDP 套接字和用户空间内存
    let mut xsk = build_socket_and_umem(
        UmemConfig::default(),            // 使用默认的用户空间内存配置
        config,                           // XDP 套接字配置
        frame_count,                      // 需要的帧数量
        &Interface::from_str(interface)?, //网络接口名称
        0,                                // 队列 ID（主队列），通常为 0
    );

    // 用户空间内存引用：获取 umem 引用，用于后续操作
    let umem = &xsk.umem;
    let mid = xsk.descs.len() / 2;
    // 发送描述符 (tx_descs)：用于发送 EtherCAT 帧
    // 接收描述符 (rx_descs)：用于接收 EtherCAT 帧
    let (tx_descs, mut rx_descs) = xsk.descs.split_at_mut(mid);

    // Make receive slots available
    // 填充接收队列：将接收描述符提交到填充队列(Fill Queue, FQ)
    // 准备接收：告诉 XDP 驱动哪些内存区域可用于接收数据包
    // 驱动协作：使内核能够直接写入用户空间内存，实现零拷贝传输
    unsafe { xsk.fq.produce(&mut rx_descs) };

    // Clear RX buffers before starting up
    // 启动清理：在正式开始前清除任何可能存在的初始数据
    // 循环消费：持续轮询并消费接收队列中的所有数据，直到队列为空
    // 初始化状态：确保通信开始时有一个干净的初始状态
    while unsafe { xsk.rx_q.poll_and_consume(&mut rx_descs, 0).unwrap() } > 0 {}

    let mut in_flight = 0u32;

    loop {
        // 设置唤醒器，使 pdu_tx 能够在有可发送帧时唤醒当前任务
        pdu_tx.replace_waker(&waker);

        let mut tx_frame_count = 0;

        // it 是发送描述符的可变迭代器，用于获取可用于发送的内存描述符
        let mut it = tx_descs.iter_mut();

        // 遍历获取可发送的 EtherCAT PDU 帧
        while let Some(frame) = pdu_tx.next_sendable_frame() {
            frame
                .send_blocking(|data: &[u8]| {
                    // 获取下一个可用的发送描述符
                    let descriptor = it.next().ok_or_else(|| {
                        fmt::error!("Not enough send slots available");

                        Error::SendFrame
                    })?;

                    fmt::debug!(
                        "Queuing EtherCAT PDU {:#04x} to send in descriptor {}",
                        data[0x11],
                        descriptor.addr()
                    );

                    // 将帧数据写入用户内存区域
                    // 通过 umem.data_mut() 直接访问用户空间内存，避免额外复制
                    unsafe { umem.data_mut(descriptor) }
                        .cursor()
                        .write_all(data)
                        .map_err(|e| {
                            fmt::error!("Failed to write frame data: {}", e);

                            Error::SendFrame
                        })?;

                    // 将描述符提交到发送队列
                    unsafe { xsk.tx_q.produce_one(descriptor) };

                    Ok(data.len())
                })
                .expect("Send blocking");

            in_flight += 1;

            tx_frame_count += 1;
        }

        // 创建子切片引用，只关注本次发送的描述符
        let sent_descs = &mut tx_descs[0..tx_frame_count];

        if tx_frame_count > 0 {
            // 唤醒检查与执行：检查是否需要唤醒用户空间程序
            // 如果需要，调用 wakeup() 方法唤醒程序
            if xsk.tx_q.needs_wakeup() {
                xsk.tx_q.wakeup()?;
            }

            fmt::trace!("Sent {} frame(s)", tx_frame_count);

            // Wait until all packets have been sent
            // 等待所有帧发送完成
            loop {
                let frames_filled = unsafe { xsk.cq.consume(sent_descs) };

                fmt::trace!("--> Completion queue filled with {} frames", frames_filled);

                if frames_filled == tx_frame_count {
                    break;
                }
            }
        }

        // ---
        // Receive
        // ---

        // Take ownership of any received descriptors back from the kernel and mark them as ready
        // for reuse.
        // SAFETY: The descriptors could potentially be reused from underneath us if we don't do
        // this on a single thread; the code below parses the frames and copies their contents into
        // other memory, so as long as it's done by the time more packets are received, we're good.
        // 从内核收回所有接收到的描述符，并将其标记为可重用。
        // 安全性：如果我们不在单个线程上执行此操作，这些描述符可能会被内核暗中重用；
        // 下面的代码解析帧并将其内容复制到其他内存中，因此只要在接收到更多数据包之前完成此操作，就没问题。

        // 从 XDP 接收队列中轮询并消费已接收的数据包
        // poll_and_consume 将接收到的数据包描述符所有权从内核转移到用户空间
        // 参数 0 表示非阻塞轮询，立即返回
        // 返回值 pkts_recvd 表示成功接收的数据包数量Pre
        let pkts_recvd = unsafe { xsk.rx_q.poll_and_consume(&mut rx_descs, 0).unwrap() };

        // 遍历所有接收到的数据包描述符，使用 take(pkts_recvd) 确保只处理实际接收到的数据包
        for recv_desc in rx_descs.iter_mut().take(pkts_recvd) {
            let received = Instant::now();

            // 直接从用户内存区域获取接收到的数据，实现零拷贝访问
            let data = unsafe { umem.data(recv_desc) };

            // 解析帧头，获取第一个 PDU 的索引
            let frame_first_pdu_index = data
                .get(0x11)
                .ok_or_else(|| io::Error::other(Error::Internal))?;

            fmt::debug!(
                "Received frame {:#04x} in descriptor {}",
                frame_first_pdu_index,
                recv_desc.addr()
            );

            loop {
                // 将接收到的数据包传递给 PduRx 进行处理
                match pdu_rx.receive_frame(&data) {
                    Ok(action) => {
                        // Return descriptor back to fill queue to receive another packet with
                        // 将描述符返回给填充队列以接收另一个数据包
                        unsafe { xsk.fq.produce_one(&recv_desc) };

                        if action == ReceiveAction::Processed {
                            fmt::trace!(
                                "--> Processed received frame with PDU {:#04x} in {} ns",
                                frame_first_pdu_index,
                                received.elapsed().as_nanos()
                            );

                            // 安全地减少正在处理的帧数量
                            in_flight = in_flight
                                .checked_sub(1)
                                .expect("Can't have fewer than 0 frames in flight");
                        } else {
                            fmt::trace!("--> Frame ignored");
                        }

                        break;
                    }
                    Err(e) => return Err(io::Error::other(e)),
                }
            }
        }

        // 当没有进行中的帧（in_flight == 0）时进入空闲状态，调用 signal.wait() 进行阻塞等待，直到收到唤醒信号
        if in_flight == 0 {
            fmt::debug!("Nothing to send, waiting for wakeup");

            let start = Instant::now();

            signal.wait();

            fmt::trace!("--> Waited for {} ns", start.elapsed().as_nanos());
        }
    }
}

pub fn build_socket_and_umem(
    umem_config: UmemConfig,
    socket_config: SocketConfig,
    frame_count: NonZeroU32,
    if_name: &Interface,
    queue_id: u32,
) -> Xsk {
    let (umem, frames) = Umem::new(umem_config, frame_count, false).expect("failed to build umem");

    let (tx_q, rx_q, fq_and_cq) = unsafe {
        xsk_rs::Socket::new(socket_config, &umem, if_name, queue_id)
            .expect("failed to build socket")
    };

    let (fq, cq) = fq_and_cq.expect(&format!(
        "missing fill and comp queue - interface {:?} may already be bound to",
        if_name
    ));

    Xsk {
        umem,
        fq,
        cq,
        tx_q,
        rx_q,
        descs: frames,
    }
}

pub struct Xsk {
    pub umem: Umem,
    pub fq: FillQueue,
    pub cq: CompQueue,
    pub tx_q: TxQueue,
    pub rx_q: RxQueue,
    pub descs: Vec<FrameDesc>,
}
