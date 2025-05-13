use super::{frame_element::sendable_frame::SendableFrame, storage::PduStorageRef};
use core::{sync::atomic::Ordering, task::Waker};

/// EtherCAT frame transmit adapter.
pub struct PduTx<'sto> {
    storage: PduStorageRef<'sto>,
}

impl<'sto> PduTx<'sto> {
    pub(in crate::pdu_loop) fn new(storage: PduStorageRef<'sto>) -> Self {
        Self { storage }
    }

    /// The number of frames that can be in flight at once.
    pub fn capacity(&self) -> usize {
        self.storage.num_frames
    }

    /// Get the next sendable frame, if any are available.
    // NOTE: Mutable so it can only be used in one task.
    pub fn next_sendable_frame(&mut self) -> Option<SendableFrame<'sto>> {
        for idx in 0..self.storage.num_frames {
            if self.should_exit() {
                return None;
            }

            //根据从发送器获取下一个 PDU 帧
            let frame = self.storage.frame_at_index(idx);

            // 通过状态的切换是否成功来判断是否可以发送
            let Some(sending) = SendableFrame::claim_sending(
                frame,
                self.storage.pdu_idx,
                self.storage.frame_data_len,
            ) else {
                continue;
            };

            return Some(sending);
        }

        None
    }

    /// Set or replace the PDU loop waker.
    ///
    /// The waker must be set otherwise the future in charge of sending new packets will not be
    /// woken again, causing a timeout error.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use ethercrab::PduStorage;
    /// use core::future::poll_fn;
    /// use core::task::Poll;
    ///
    /// # static PDU_STORAGE: PduStorage<2, { PduStorage::element_size(2) }> = PduStorage::new();
    /// let (pdu_tx, _pdu_rx, _pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
    ///
    /// poll_fn(|ctx| {
    ///     // Set the waker so this future is polled again when new EtherCAT frames are ready to
    ///     // be sent.
    ///     pdu_tx.replace_waker(ctx.waker());
    ///
    ///     // Send and receive packets over the network interface here
    ///
    ///     Poll::<()>::Pending
    /// });
    /// ```
    #[cfg_attr( //属性宏，它可以根据编译条件应用其他属性。
        any(target_os = "windows", target_os = "macos", not(feature = "std")), //编译条件，满足以下任意一个条件时该属性生效：
        allow(unused)//忽略 replace_waker 方法未被使用的警告。
    )]
    pub fn replace_waker(&self, waker: &Waker) {
        self.storage.tx_waker.register(waker); //register 方法的作用通常是更新当前存储的唤醒器，以便在合适的时机唤醒对应的异步任务
    }

    //检查PduStorage的TX/RX循环退出标志位
    /// Returns `true` if the PDU sender should exit.
    ///
    /// This will be triggered by [`MainDevice::release_all`](crate::MainDevice::release_all). When
    /// giving back ownership of the `PduTx`, be sure to call [`release`](crate::PduTx::release) to
    /// ensure all internal state is correct before reuse.
    pub fn should_exit(&self) -> bool {
        //调用 AtomicBool 的 load 方法，从原子变量中读取当前值。Ordering::Acquire 是内存顺序，它确保在读取 exit_flag 之后的所有读、写操作不会被重排到读取操作之前。
        //这意味着，当 load 操作返回 true 时，程序能保证在设置 exit_flag 之前的所有写操作都已经完成。
        self.storage.exit_flag.load(Ordering::Acquire)
    }

    /// Reset this object ready for reuse.
    pub fn release(self) -> Self {
        self.storage.exit_flag.store(false, Ordering::Relaxed);
        //Ordering::Relaxed：内存顺序参数。Ordering::Relaxed 是最宽松的内存顺序，它只保证原子操作本身的原子性，不提供任何跨线程的内存同步保证，即不保证该操作与其他线程的读写操作之间的顺序。
        //在这个场景下，由于只是简单地重置退出标志，不需要与其他线程的操作进行同步，所以使用 Relaxed 内存顺序就足够了，这样可以获得更好的性能。

        self
    }
}
