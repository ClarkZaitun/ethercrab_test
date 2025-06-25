//! Items to use when not in `no_std` environments.

use crate::{
    ReceiveAction,
    error::Error,
    fmt,
    pdu_loop::{PduRx, PduTx},
    std::ParkSignal,
};
use pnet_datalink::{self, Channel, DataLinkReceiver, DataLinkSender, channel};
use std::io;
use std::{sync::Arc, task::Waker, time::SystemTime};

/// Get a TX/RX pair.
fn get_tx_rx(
    device: &str,
) -> Result<(Box<dyn DataLinkSender>, Box<dyn DataLinkReceiver>), std::io::Error> {
    let interfaces = pnet_datalink::interfaces();

    let interface = match interfaces.iter().find(|interface| interface.name == device) {
        Some(interface) => interface,
        None => {
            fmt::error!("Could not find interface {device}");

            fmt::error!("Available interfaces:");

            for interface in interfaces.iter() {
                fmt::error!("-> {} {}", interface.name, interface.description);
            }

            panic!();
        }
    };

    let config = pnet_datalink::Config {
        write_buffer_size: 16384,
        read_buffer_size: 16384,
        ..Default::default()
    };

    let (tx, rx) = match channel(interface, config) {
        Ok(Channel::Ethernet(tx, rx)) => (tx, rx),
        Ok(_) => panic!("Unhandled channel type"),
        Err(e) => return Err(e),
    };

    Ok((tx, rx))
}

/// Windows-specific configuration for [`tx_rx_task_blocking`].
#[derive(Copy, Clone, Debug, Default)]
pub struct TxRxTaskConfig {
    /// If set to `true`, use a spinloop to wait for packet TX or RX instead of putting the thread
    /// to sleep.
    ///
    /// If enabled, this option will peg a CPU core to 100% usage but may improve latency and
    /// jitter. It is recommended to pin it to a core using
    /// [`thread_priority`](https://docs.rs/thread-priority/latest/x86_64-pc-windows-msvc/thread_priority/index.html)
    /// or similar.
    pub spinloop: bool, //如果设置为 'true'，则使用自旋循环等待数据包 TX 或 RX，而不是使线程进入睡眠状态。
}

/// Create a blocking task that waits for PDUs to send, and receives PDU responses.
pub fn tx_rx_task_blocking<'sto>(
    device: &str,
    mut pdu_tx: PduTx<'sto>,
    mut pdu_rx: PduRx<'sto>,
    config: TxRxTaskConfig,
) -> Result<(PduTx<'sto>, PduRx<'sto>), io::Error> {
    // 创建一个 ParkSignal 实例，并使用 Arc（原子引用计数）进行包装。
    // Arc 允许在多个线程之间安全地共享 ParkSignal 实例。
    // ParkSignal 是一个自定义结构体，其 `new` 方法会将当前线程的句柄存储在实例中。
    // 后续可以利用这个句柄来控制线程的暂停和唤醒操作。
    let signal = Arc::new(ParkSignal::new());

    // Arc::clone(&signal) 会克隆一个新的 Arc 引用，这不会复制 ParkSignal 实例本身，仅增加引用计数。
    // 这样做能保证多个地方可安全共享 ParkSignal 实例，且在所有引用都被丢弃后，ParkSignal 实例才会被销毁。
    // 然后使用这个克隆的引用创建一个 Waker 对象。
    // Waker 是 Rust 异步编程中的核心概念，它代表一个唤醒器，
    // 可以在异步任务准备好继续执行时调用 `wake` 方法来唤醒对应的任务。
    // 这里将 ParkSignal 与 Waker 关联起来，意味着当调用这个 Waker 的 `wake` 方法时，
    // 会触发 ParkSignal 中存储的线程的唤醒操作。
    let waker = Waker::from(Arc::clone(&signal));

    // 从指定的网络设备创建一个 pcap 捕获器实例。
    // `pcap` 是一个 Rust 库，它封装了 libpcap 或 WinPcap 库的功能，用于捕获和发送网络数据包。
    // `from_device(device)` 方法接受一个设备名称作为参数，尝试创建一个针对该设备的捕获器。
    // 如果创建失败，`.expect("Device")` 会触发 panic 并显示错误信息 "Device"。
    let mut cap = pcap::Capture::from_device(device) //在项目的 Cargo.toml 文件中，已经把 pcap 库添加为依赖项。
        // 这意味着在编译时，Rust 会自动下载并构建 pcap 库的二进制文件，以便在项目中使用。
        .expect("Device")
        // 设置捕获器为立即模式。
        // 在立即模式下，捕获器会在接收到数据包后立即返回，而不是等待缓冲区填满或者超时。
        // 这有助于减少捕获数据包的延迟。
        .immediate_mode(true)
        // 打开捕获器，使其准备好捕获网络数据包。
        // 如果打开失败，`.expect("Open device")` 会触发 panic 并显示错误信息 "Open device"。
        .open()
        .expect("Open device")
        // 将捕获器设置为非阻塞模式。
        // 在非阻塞模式下，调用捕获数据包的方法时，如果没有可用的数据包，方法会立即返回，而不是阻塞线程等待。
        // 这使得程序可以在等待数据包的同时执行其他任务。
        // 如果设置失败，`.expect("Can't set non-blocking")` 会触发 panic 并显示错误信息 "Can't set non-blocking"。
        .setnonblock()
        .expect("Can't set non-blocking");

    //SendQueue 是 pcap 库中用于管理待发送数据包队列的结构体。
    // 1MB send queue.
    let mut sq = pcap::sendqueue::SendQueue::new(1024 * 1024).expect("Failed to create send queue");

    // 记录当前已经发送但还未收到响应的数据包数量，也就是飞行中数据包的数量
    let mut in_flight = 0usize;

    loop {
        fmt::trace!("Begin TX/RX iteration");

        //replace_waker 方法将之前创建的 waker 对象传递给它。waker 用于在异步任务准备好继续执行时唤醒对应的任务
        pdu_tx.replace_waker(&waker);

        //记录本次迭代中发送的帧数
        let mut sent_this_iter = 0usize;

        while let Some(frame) = pdu_tx.next_sendable_frame() {
            let idx = frame.storage_slot_index(); // 获取当前帧的在帧缓冲区的索引

            frame
                .send_blocking(|frame_bytes| {
                    fmt::trace!("Send frame {:#04x}, {} bytes", idx, frame_bytes.len());

                    //将帧数据添加到 SendQueue 中，如果失败则触发 panic
                    // Add 256 bytes of L2 payload
                    sq.queue(None, frame_bytes).expect("Enqueue");

                    Ok(frame_bytes.len())
                })
                .map_err(std::io::Error::other)?; //将可能的错误转换为 std::io::Error 并在出错时提前返回

            sent_this_iter += 1;
        }

        // 发送队列中的数据包
        // Send any queued packets
        if sent_this_iter > 0 {
            fmt::trace!("Send {} enqueued frames", sent_this_iter);

            // 将队列中的所有数据包发送出去，SendSync::Off 表示发送时数据包之间无延迟
            // SendSync::Off = transmit with no delay between packets
            sq.transmit(&mut cap, pcap::sendqueue::SendSync::Off)
                .expect("Transmit");

            in_flight += sent_this_iter;
        }

        // 有飞行帧才执行
        if in_flight > 0 {
            //debug_assert! 宏在调试模式下检查 cap 是否处于非阻塞模式，若不是则触发断言失败
            debug_assert!(cap.is_nonblock(), "Must be in non-blocking mode");

            fmt::trace!("{} frames are in flight", in_flight);

            // Receive any in-flight frames
            loop {
                //cap.next_packet() 尝试从捕获器 cap 中获取下一个数据包
                match cap.next_packet() {
                    // NOTE: We receive our own sent frames. `receive_frame` will make sure they're
                    // ignored.
                    Ok(packet) => {
                        let frame_buf = packet.data;

                        let frame_index = frame_buf
                            .get(0x11) //尝试从 frame_buf 的第 0x11 位置获取帧索引，若失败则返回内部错误
                            .ok_or_else(|| io::Error::other(Error::Internal))?;

                        let res = pdu_rx
                            .receive_frame(&frame_buf) //解析以太网 II 帧的 EtherCAT 协议数据单元（PDU），切换帧状态，唤醒帧任务
                            .map_err(|e| io::Error::other(e))?; //将可能的错误转换为 io::Error

                        fmt::trace!(
                            "Received and {:?} frame {:#04x} ({} bytes)",
                            res,
                            frame_index,
                            packet.header.len
                        );

                        //若receive_frame处理结果为 ReceiveAction::Processed，表示该帧已成功处理，将 in_flight 减 1，若结果为负数则触发 panic
                        if res == ReceiveAction::Processed {
                            in_flight = in_flight
                                .checked_sub(1) // `checked_sub` 是 `usize` 类型的方法，用于安全地执行减法操作。
                                // 若减法结果不会导致下溢（即结果不会为负数，因为 `usize` 是非负整数类型），
                                // 则返回 `Some(result)`；若会下溢，则返回 `None`。
                                .expect("More frames processed than in flight");
                        }
                    }
                    Err(pcap::Error::NoMorePackets) => {
                        // Nothing to read yet

                        break;
                    }
                    Err(pcap::Error::TimeoutExpired) => {
                        // Timeouts are instant as we're in non-blocking mode (I think), so we just
                        // ignore them (we're spinlooping while packets are in flight essentially).

                        break;
                    }
                    Err(e) => {
                        fmt::error!("Packet receive failed: {}", e);

                        // Quit the TX/RX loop - we failed somewhere
                        // TODO: Allow this to be configured so we ignore RX failures
                        return Err(io::Error::other(e));
                    }
                }
            }
        }
        // 当飞行中没有数据包，且 spinloop 配置为 false 时执行此分支
        // No frames in flight. Wait to be woken again by something sending a frame
        else if !config.spinloop {
            fmt::trace!("No frames in flight, waiting to be woken with new frames to send");

            // 调用 signal 的 wait 方法，使当前线程进入等待状态
            // 当有新的数据包需要发送时，会通过 Waker 唤醒该线程
            signal.wait();

            if pdu_tx.should_exit() {
                fmt::debug!("io_uring TX/RX was asked to exit");

                // Break out of entire TX/RX loop
                break;
            }
        } else {
            // 当 spinloop 配置为 true 时，执行忙等待
            // std::hint::spin_loop() 会让 CPU 进行空转，不断检查是否有新的数据包需要处理
            // 这种方式会使 CPU 占用率达到 100%，但可能会减少延迟和抖动
            std::hint::spin_loop()
        }
    }

    // 退出循环后，释放 PduTx 和 PduRx 的内部状态，并将它们作为结果返回
    Ok((pdu_tx.release(), pdu_rx.release()))
}

/// Get the current time in nanoseconds from the EtherCAT epoch, 2000-01-01.
///
/// Note that on Windows this clock is not monotonic.
pub fn ethercat_now() -> u64 {
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // EtherCAT epoch is 2000-01-01
    t.saturating_sub(946684800)
}
