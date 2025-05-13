use crate::{PduRx, PduTx, error::Error, fmt, std::ParkSignal, std::unix::RawSocketDesc};
use core::{mem::MaybeUninit, task::Waker};
use io_uring::{IoUring, opcode};
use smallvec::{SmallVec, smallvec};
use std::{io, os::fd::AsRawFd, sync::Arc, time::Instant};

/// Use the upper bit of a u64 to mark whether a frame is a write (`1`) or a read (`0`).
const WRITE_MASK: u64 = 1 << 63;
const ENTRIES: usize = 256;

/// Create a blocking TX/RX loop using `io_uring`.
///
/// This function is only available on `linux` targets as it requires `io_uring` support. Older
/// kernels may not support `io_uring`.
pub fn tx_rx_task_io_uring<'sto>(
    interface: &str,
    mut pdu_tx: PduTx<'sto>,
    mut pdu_rx: PduRx<'sto>,
) -> Result<(PduTx<'sto>, PduRx<'sto>), io::Error> {
    let mut socket = RawSocketDesc::new(interface)?;

    let mtu = socket.interface_mtu()?;

    fmt::debug!(
        "Opening {} with MTU {}, blocking, using io_uring",
        interface,
        mtu
    );

    // MTU is payload size. We need to add the layer 2 header which is 18 bytes.
    let mtu = mtu + 18;

    // SAFETY: Max entries is 256 because `PduStorage::N` is checked to be in 0..u8::MAX, and will
    // eventually be a `u8` once const generics get there. Twice as much space is reserved as each
    // frame requires a send _and_ receive buffer.
    //
    // This data MUST NOT MOVE or be reordered once created as io_uring holds pointers into it.
    // 安全性：最大条目数为 256，因为 `PduStorage::N` 会被检查为 0..u8::MAX 范围内，并且最终会在常量泛型到达该范围时变为 `u8`。
    // 预留的空间是每个帧的两倍，因为每个帧都需要一个发送缓冲区和一个接收缓冲区。
    // 此数据一旦创建就不得移动或重新排序，因为 io_uring 持有指向它的指针。
    // slab 是一个内存池，用于存储 io_uring 的条目和缓冲区。
    // 使用 SmallVec 是一种优化，它在栈上存储小数据，只在数据超过预定义大小时才会分配堆内存
    // 这些slot没有按照一定的顺序排列读写
    let mut bufs: slab::Slab<(io_uring::squeue::Entry, SmallVec<[u8; 1518]>)> =
        slab::Slab::with_capacity(ENTRIES * 2);

    // 创建 io_uring 实例，指定最大条目数为 256。
    let mut ring = IoUring::new(ENTRIES as u32)?;
    // io_uring 是 Linux 内核 5.1 引入的异步 I/O 接口，相比传统的 epoll 具有更低的系统调用开销和更高的吞吐量，特别适合高性能网络应用。
    // 它通过两个主要队列工作：
    // 提交队列(SQ)：用户空间向内核提交 I/O 请求
    // 完成队列(CQ)：内核向用户空间通知完成的 I/O 操作

    // checks io_uring support for used opcodes
    // 检查 io_uring 是否支持用于读取和写入操作的 opcode
    // 不同 Linux 内核版本对 io_uring 的支持程度不同，旧版本可能缺少某些关键功能。
    let mut probe = io_uring::register::Probe::new();
    ring.submitter().register_probe(&mut probe)?;
    if !(probe.is_supported(opcode::Read::CODE) && probe.is_supported(opcode::Write::CODE)) {
        log::error!("io_uring does not support read and/or write opcodes");
        return Err(io::Error::other(Error::Internal));
    }

    let mut high_water_mark = 0;

    // ParkSignal：创建一个用于线程 park/unpark 的同步原语
    let signal = Arc::new(ParkSignal::new());
    // 基于 ParkSignal 创建一个 Waker 对象，用于 Rust 异步任务的唤醒机制
    let waker = Waker::from(Arc::clone(&signal));
    // 异步事件处理：
    // waker 用于唤醒等待 I/O 完成的任务
    // signal 用于控制事件循环的暂停和恢复
    // 高性能 PDU 传输： 通过 io_uring 提交读写操作

    loop {
        // 将 waker 对象注册到 PduTx 实例中，用于在需要发送新的 EtherCAT 帧时唤醒对应的异步任务。
        pdu_tx.replace_waker(&waker);

        let mut sent = 0;

        while let Some(frame) = pdu_tx.next_sendable_frame() {
            let idx = frame.storage_slot_index();

            // 获取一个空闲槽位的可变引用，用于发送操作
            let tx_b = bufs.vacant_entry();
            // 获取该槽位的唯一标识符（键值），用于后续在完成队列中识别操作
            // 这个键值将与 WRITE_MASK 组合，作为 user_data 传递给 io_uring，用于区分读写操作
            let tx_key = tx_b.key();
            // 缓冲区初始化：在获取的槽位中插入一个元组
            let (tx_entry, tx_buf) = tx_b.insert((
                // 使用 MaybeUninit::zeroed().assume_init() 高效地零初始化 io_uring::squeue::Entry 结构体
                unsafe { MaybeUninit::zeroed().assume_init() },
                // 数据缓冲区：使用 smallvec![0; mtu] 创建一个大小为 MTU 的字节数组
                smallvec![0; mtu],
            ));

            frame
                // 这里是闭包，用于处理实际的发送操作
                .send_blocking(|data: &[u8]| {
                    // 创建一个 opcode::Write 操作
                    *tx_entry = opcode::Write::new(
                        // 文件描述符：通过 socket.as_raw_fd() 获取原始套接字描述符
                        io_uring::types::Fd(socket.as_raw_fd()),
                        data.as_ptr(),
                        data.len() as _,
                    )
                    // 调用 .build() 构建操作描述符
                    .build()
                    // Distinguish sent frames from received frames by using the upper bit of
                    // the user data as a flag.
                    // 通过使用用户数据的最高位作为标志来区分已发送帧和已接收帧。
                    .user_data(tx_key as u64 | WRITE_MASK);

                    // TODO: Zero copy
                    // 将 EtherCAT 帧数据复制到发送缓冲区 tx_buf
                    tx_buf
                        .get_mut(0..data.len())
                        .ok_or(Error::Internal)?
                        .copy_from_slice(data);

                    // 尝试将写操作推入提交队列。如果队列为满（push().is_err()），进入循环
                    while unsafe { ring.submission().push(tx_entry).is_err() } {
                        // If the submission queue is full, flush it to the kernel
                        // 调用 ring.submit() 将队列中现有操作刷新到内核
                        ring.submit().expect("Internal error, failed to submit ops");
                    }

                    sent += 1;

                    // 返回成功发送的字节数
                    Ok(data.len())
                })
                .expect("Send blocking");

            // 获取一个空闲槽位的可变引用，用于接收操作
            let rx_b = bufs.vacant_entry();
            let rx_key = rx_b.key();
            let (rx_entry, rx_buf) = rx_b.insert((
                unsafe { MaybeUninit::zeroed().assume_init() },
                smallvec![0; mtu],
            ));

            // 创建一个 opcode::Read 操作，指定要从哪个文件描述符读取数据
            *rx_entry = opcode::Read::new(
                io_uring::types::Fd(socket.as_raw_fd()),
                rx_buf.as_mut_ptr() as _,
                rx_buf.len() as _,
            )
            // 调用 .build() 构建操作描述符
            .build()
            // 通过 .user_data() 添加用户数据 rx_key，用于后续标识和跟踪这个操作
            // 这里没有设置 WRITE_MASK，因为这是一个读取操作
            .user_data(rx_key as u64);

            fmt::trace!(
                "Insert frame TX {:#04x}, key {}, RX key {}",
                idx,
                tx_key,
                rx_key
            );

            // 尝试将接收操作推入 io_uring 的提交队列。如果队列为满（push().is_err()），进入循环并调用 ring.submit() 将队列中现有操作提交给内核处理
            // 这确保了即使在高负载情况下，I/O 操作也能持续处理
            while unsafe { ring.submission().push(rx_entry).is_err() } {
                // If the submission queue is full, flush it to the kernel
                ring.submit().expect("Internal error, failed to submit ops");
            }

            // 维护并更新内存缓冲区使用的高水位标记：
            // 高水位监控：记录 bufs 这个内存池在整个程序运行期间曾经同时使用的最大条目数量
            // 最大值更新：通过 Rust 的 .max() 方法，确保 high_water_mark 始终保持为历史最大值
            // 无副作用：不会修改 bufs 本身的状态，只是读取其当前使用的条目数并更新监控变量
            high_water_mark = high_water_mark.max(bufs.len());
            // 设计意图与作用
            // 这行代码在 EtherCAT 高性能实时通信中具有以下重要作用：

            // 性能监控与调优：

            // 记录内存池的实际使用峰值，帮助开发者确定是否分配了合理的缓冲区容量
            // 如果峰值接近 ENTRIES * 2，可能需要增加容量以避免资源竞争
            // 如果峰值远低于容量，可能有优化内存使用的空间
            // 负载分析：

            // 提供系统通信负载的实时反馈，反映 EtherCAT 网络通信的繁忙程度
            // 有助于识别可能的性能瓶颈或资源限制
            // 内存管理优化：

            // 为未来的内存池容量调整提供数据支持
            // 在不同的应用场景下，可以根据实际的高水位值优化内存分配
            // 调试与问题诊断：

            // 在系统出现性能问题时，可以检查高水位值，判断是否存在缓冲区不足的情况
            // 有助于区分是网络拥塞还是内存资源限制导致的性能下降
        }

        // TODO: Collect these metrics for later gathering instead of just asserting
        // assert_eq!(ring.completion().overflow(), 0);
        // assert_eq!(ring.completion().is_full(), false);
        // assert_eq!(ring.submission().cq_overflow(), false);
        // assert_eq!(ring.submission().dropped(), 0);

        // 队列同步：确保用户空间提交队列中的所有 I/O 请求都被正确同步到内核空间可见的内存区域
        // 内存屏障：在底层实现上，sync() 操作通常包含内存屏障，确保所有之前的内存写入对内核可见
        // 提交准备：这是在调用 submit_and_wait() 前的关键准备步骤，确保内核能够看到所有待处理的 I/O 操作
        ring.submission().sync();

        let now = Instant::now();

        // 调用 submit_and_wait() 会阻塞当前线程，直到指定数量的操作完成或超时
        if sent > 0 {
            // 将提交队列中的所有 I/O 请求提交给内核处理
            // 同步等待 直到至少 sent * 2 个操作完成
            ring.submit_and_wait(sent * 2)?;
            // 为什么等待 sent * 2 个完成事件：
            // 这是因为通信采用请求-响应模式
            // 对于每个发送操作（Write），系统都会同时提交一个对应的接收操作（Read）
            // 因此，每个 EtherCAT 帧交互会产生 2 个完成事件（一个发送完成，一个接收完成）
            // 通过等待 sent * 2 个完成，确保所有帧的完整收发周期都已完成

            // 与单独提交和等待每个操作相比，批量提交和等待显著提高了性能

            // 优化：发送时，切换到其他线程。定时唤醒后，检查是否有完成的接收帧
        }

        fmt::trace!(
            "Submitted, waited for {} completions for {} us",
            // ring.completion() 获取对 io_uring 完成队列的引用
            // .len() 方法返回该队列中当前可用的完成事件数量
            // 这些完成事件代表已经完成处理的 I/O 操作（发送或接收）
            ring.completion().len(),
            // 精确测量 I/O 操作的响应时间
            now.elapsed().as_micros(),
        );

        // SAFETY: We must never call `completion_shared` or `completion` inside this loop.
        // 安全性：我们绝不能在此循环内调用 `completion_shared` 或 `completion`。
        // 遍历 io_uring 完成队列中的所有完成事件
        for recv in unsafe { ring.completion_shared() } {
            // 错误过滤：区分致命错误和非阻塞错误
            // 异常退出：对于致命错误（非 EWOULDBLOCK），立即返回错误
            // 错误恢复：EWOULDBLOCK 被视为可恢复的临时状态
            if recv.result() < 0 && recv.result() != -libc::EWOULDBLOCK {
                return Err(io::Error::last_os_error());
            }

            // 用户数据解析：通过 user_data() 提取操作标识
            // 方向识别：使用 WRITE_MASK 位区分发送（---->）和接收（<--）操作
            let key = recv.user_data();

            let received = Instant::now();

            fmt::trace!(
                "Got a frame by key {} -> {} {}",
                key,
                key & !WRITE_MASK,
                if key & WRITE_MASK == WRITE_MASK {
                    "---->"
                } else {
                    "<--"
                }
            );

            // If upper bit is set, this was a write that is now complete. We can remove its buffer
            // from the slab allocator.
            // 缓冲区管理：发送完成后，从缓冲区池中移除已发送的帧
            if key & WRITE_MASK == WRITE_MASK {
                let key = key & !WRITE_MASK;

                // Clear send buffer grant as it's been sent over the network
                bufs.remove(key as usize);

                continue;
            }

            // Original read did not succeed. Requeue read so we can try again.
            // 接收操作重试：当接收操作阻塞时，将其重新入队等待后续重试
            if recv.result() == -libc::EWOULDBLOCK {
                fmt::trace!("Frame key {} would block. Queuing for retry", key);

                let (rx_entry, _buf) = bufs.get(key as usize).expect("Could not get retry entry");

                // SAFETY: `submission_shared` must not be held at the same time this one is
                while unsafe { ring.submission_shared().push(rx_entry).is_err() } {
                    // If the submission queue is full, flush it to the kernel
                    // 提交队列已满时，尝试提交已有的 I/O 请求到内核
                    ring.submit().expect("Internal error, failed to submit ops");
                }
            } else {
                // 接收操作完成处理：从缓冲区中提取接收到的帧，并传递给 PduRx 进行进一步处理

                let (_entry, frame) = bufs.remove(key as usize);

                // 提取帧索引：从接收的帧数据中提取帧索引（0x11 字节）
                let frame_index = frame
                    .get(0x11)
                    .ok_or_else(|| io::Error::other(Error::Internal))?;

                fmt::trace!(
                    "Raw frame {:#04x} result {} buffer key {}",
                    frame_index,
                    recv.result(),
                    key,
                );

                // 传递给 PduRx 进行处理
                pdu_rx.receive_frame(&frame).map_err(io::Error::other)?;

                fmt::trace!("Received frame in {} ns", received.elapsed().as_nanos());
            }
        }

        // 没有缓冲区帧，等待被唤醒以发送新帧
        if bufs.is_empty() {
            fmt::trace!("No frames in flight, waiting to be woken with new frames to send");

            let start = Instant::now();

            // This must be after the send packet code as there can be a (safe!) race condition on
            // startup where the TX waker hasn't been registered yet, so when a future from another
            // thread tries to send its frame, it has no waker, so we just end up waiting forever.
            //
            // If this wait() is down here, we get at least one loop where any queued packets can be
            // sent.
            // 这必须放在发送数据包代码之后，因为在启动时可能会出现（安全的！）竞态条件，即发送唤醒器尚未注册，
            // 所以当另一个线程的 future 尝试发送其帧时，它没有唤醒器，因此我们最终会无限期地等待。
            // 如果 wait() 函数在这里执行，我们至少会得到一个循环，任何排队的数据包都可能被发送。
            signal.wait();

            fmt::trace!("--> Waited for {} ns", start.elapsed().as_nanos());

            if pdu_tx.should_exit() {
                fmt::debug!("io_uring TX/RX was asked to exit");

                return Ok((pdu_tx.release(), pdu_rx.release()));
            }
        } else {
            // 如果缓冲区中仍有帧在飞行中，记录当前帧索引，继续执行循环
            fmt::trace!(
                "Buf keys {:?} in flight",
                bufs.iter().map(|(k, _v)| k).collect::<Vec<_>>(),
            );
        }
    }
}
