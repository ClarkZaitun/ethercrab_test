use crate::{
    PduLoop,
    error::{Error, PduError},
    fmt,
    pdu_loop::frame_element::{FrameBox, FrameElement, FrameState, received_frame::ReceivedFrame},
};
use core::{future::Future, ptr::NonNull, sync::atomic::AtomicU8, task::Poll, time::Duration};
use futures_lite::FutureExt;

/// A frame has been sent and is now waiting for a response from the network.
///
/// This state may only be entered once the frame has been sent over the network.
#[derive(Debug)]
pub struct ReceivingFrame<'sto> {
    inner: FrameBox<'sto>,
}

impl<'sto> ReceivingFrame<'sto> {
    // Sent 状态的帧标记为 RxBusy 状态
    pub(in crate::pdu_loop) fn claim_receiving(
        frame: NonNull<FrameElement<0>>,
        pdu_idx: &'sto AtomicU8,
        frame_data_len: usize,
    ) -> Option<Self> {
        //尝试将一个处于 Sent 状态的帧标记为 RxBusy 状态，即声明该帧开始接收响应数据。成功后返回帧指针
        let frame = unsafe { FrameElement::claim_receiving(frame)? };

        Some(Self {
            inner: FrameBox::new(frame, pdu_idx, frame_data_len),
        })
    }

    // 标记 RxBusy 帧的状态为RxDone，唤醒帧相关的任务
    /// Mark the frame as fully received.
    ///
    /// This method may only be called once the frame response (header and data) has been validated
    /// and stored in the frame element.
    pub(in crate::pdu_loop) fn mark_received(&self) -> Result<(), PduError> {
        // Frame state must be updated BEFORE the waker is awoken so the future impl returns
        // `Poll::Ready`. The future will poll, see the `FrameState` as RxDone and return
        // Poll::Ready.

        // NOTE: claim_receiving sets the state to `RxBusy` during parsing of the incoming frame
        // so the previous state here should be RxBusy.
        self.inner
            .swap_state(FrameState::RxBusy, FrameState::RxDone)
            .map_err(|bad| {
                fmt::error!(
                    "Failed to set frame {:#04x} state from RxBusy -> RxDone, got {:?}",
                    self.storage_slot_index(),
                    bad
                );

                PduError::InvalidFrameState
            })?;

        //在完成状态更新后，需要唤醒缓冲区中的帧关联的任务，通知等待该帧响应的任务，告知它们帧已经接收完毕
        //当响应在执行器首次轮询该 future 之前就已经收到时，帧可能没有 waker，此时 wake() 会返回错误，但这种错误属于正常情况，因此可以忽略
        // wake() returns an error if there is no waker. A frame might have no waker if the response
        // is received over the network before the chosen executor has a chance to poll the future
        // for the first time, so we'll ignore that error otherwise we might get false positives.
        let _ = self.inner.wake();

        Ok(())
    }

    //获取一个可变的字节切，该切片指向 EtherCAT数据报
    pub(in crate::pdu_loop) fn buf_mut(&mut self) -> &mut [u8] {
        self.inner.pdu_buf_mut()
    }

    /// Ethernet frame index.
    fn storage_slot_index(&self) -> u8 {
        self.inner.storage_slot_index()
    }
}

pub struct ReceiveFrameFut<'sto> {
    pub(in crate::pdu_loop::frame_element) frame: Option<FrameBox<'sto>>,
    pub(in crate::pdu_loop::frame_element) pdu_loop: &'sto PduLoop<'sto>,
    pub(in crate::pdu_loop::frame_element) timeout_timer: crate::timer_factory::Timer,
    pub(in crate::pdu_loop::frame_element) timeout: Duration,
    pub(in crate::pdu_loop::frame_element) retries_left: usize,
}

impl<'sto> ReceiveFrameFut<'sto> {
    /// Get entire frame buffer. Only really useful for assertions in tests.
    #[cfg(test)] // 将代码限定在测试环境下编译
    pub fn buf(&self) -> &[u8] {
        use crate::{ethernet::EthernetFrame, pdu_loop::frame_header::EthercatFrameHeader};
        use ethercrab_wire::EtherCrabWireSized;

        let frame = self.frame.as_ref().unwrap();

        let b = frame.ethernet_frame();

        let len = EthernetFrame::<&[u8]>::buffer_len(frame.pdu_payload_len())
            + EthercatFrameHeader::PACKED_LEN;

        &b.into_inner()[0..len]
    }

    // 设置帧状态为None
    fn release(r: FrameBox<'sto>) {
        // Make frame available for reuse if this future is dropped.
        r.set_state(FrameState::None);
    }
}

// SAFETY: This unsafe impl is required due to `FrameBox` containing a `NonNull`, however this impl
// is ok because FrameBox also holds the lifetime `'sto` of the backing store, which is where the
// `NonNull<FrameElement>` comes from.
//
// For example, if the backing storage is is `'static`, we can send things between threads. If it's
// not, the associated lifetime will prevent the framebox from being used in anything that requires
// a 'static bound.
unsafe impl Send for ReceiveFrameFut<'_> {}

impl<'sto> Future for ReceiveFrameFut<'sto> {
    type Output = Result<ReceivedFrame<'sto>, Error>;

    // 检查帧是否接收完成，处理超时情况，并根据帧的状态决定是继续等待、返回成功结果还是返回错误结果。同时，它还处理了重试逻辑，在超时且有重试次数时重新发送帧
    fn poll(
        mut self: core::pin::Pin<&mut Self>, // 接收一个可变的 Pin 指针，Pin 用于确保 self 不会被移动，这在处理自引用结构体时很重要
        cx: &mut core::task::Context<'_>, // 任务上下文，包含一个 Waker，用于在异步操作准备好时唤醒任务
    ) -> Poll<Self::Output> {
        // 检查帧是否已被取出。
        // 已取出就是发送了？或者正在被这个函数处理？
        // take 把 Option 内部的值取出来，同时将原 Option 置为 None。在取值过程中，它会获取 Option 内部值的所有权
        let Some(rxin) = self.frame.take() else {
            //如果 self.frame 为 None，说明帧已经被取出，记录错误日志并返回 Err 表示操作失败
            fmt::error!("Frame is taken");

            return Poll::Ready(Err(PduError::InvalidFrameState.into()));
        };

        // 将 rxin 中的唤醒器替换为当前任务上下文的唤醒器，以便在帧准备好时能唤醒当前任务
        rxin.replace_waker(cx.waker());

        let frame_idx = rxin.storage_slot_index();

        // 尝试将帧的状态从 RxDone 交换为 RxProcessing。如果交换成功，说明帧已经接收完成，记录日志并返回 Ok 表示操作成功；如果交换失败，记录之前的状态
        // RxDone is set by mark_received when the incoming packet has been parsed and stored
        let swappy = rxin.swap_state(FrameState::RxDone, FrameState::RxProcessing);

        let was = match swappy {
            Ok(_) => {
                fmt::trace!("frame index {} is ready", frame_idx);

                return Poll::Ready(Ok(ReceivedFrame::new(rxin)));
            }
            Err(e) => e,
        };

        fmt::trace!("frame index {} not ready yet ({:?})", frame_idx, was);

        // 检查超时
        // 对超时定时器进行 poll 操作。如果定时器超时且没有重试次数了，释放帧并返回 Err 表示超时；
        // 如果还有重试次数，重新设置定时器，将帧状态设置为 Sendable 并唤醒帧发送器，减少重试次数。
        // 如果定时器未超时，则继续等待。
        // ??帧处理完成后会检查超时，以便我们至少有一次机会从网络接收回复。这应该可以缓解在帧刚收到时超时导致的竞争情况。
        // Timeout checked after frame handling so we get at least one chance to receive reply from
        // network. This should mitigate race conditions when timeout expires just as the frame is
        // received.
        match self.timeout_timer.poll(cx) {
            Poll::Ready(_) => {
                // We timed out
                fmt::trace!(
                    "PDU response timeout with {} retries remaining",
                    self.retries_left
                );

                if self.retries_left == 0 {
                    // Release frame and PDU slots for reuse
                    Self::release(rxin);

                    return Poll::Ready(Err(Error::Timeout));
                }

                // If we have retry loops left:

                // Assign new timeout
                self.timeout_timer = crate::timer_factory::timer(self.timeout);
                // Poll timer once to register with the executor
                let _ = self.timeout_timer.poll(cx);

                // Mark frame as sendable once more
                rxin.set_state(FrameState::Sendable);
                // Wake frame sender so it picks up this frame we've just marked
                self.pdu_loop.wake_sender();

                self.retries_left -= 1;
            }
            Poll::Pending => {
                // Haven't timed out yet. Nothing to do - still waiting to be woken from the network
                // response.
            }
        }

        match was {
            FrameState::Sendable | FrameState::Sending | FrameState::Sent | FrameState::RxBusy => {
                self.frame = Some(rxin); // 将帧重新放回 self.frame，归还所有权

                Poll::Pending // 返回 Pending 表示操作未完成
            }
            // 这个帧被错误地处理，没有在正确的状态
            state => {
                fmt::error!("Frame is in invalid state {:?}", state);

                Poll::Ready(Err(PduError::InvalidFrameState.into()))
            }
        }
    }
}

// If this impl is removed, timed out frames will never be reclaimed, clogging up the PDU loop and
// crashing the program.
impl Drop for ReceiveFrameFut<'_> {
    fn drop(&mut self) {
        // Frame option is taken when future completes successfully, so this drop logic will only
        // fire if the future is dropped before it completes.
        if let Some(r) = self.frame.take() {
            fmt::debug!("Dropping in-flight future, possibly caused by timeout");

            Self::release(r);
        }
    }
}
