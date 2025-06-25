use crate::{
    error::Error,
    ethernet::EthernetFrame,
    fmt,
    pdu_loop::{
        frame_element::{FrameBox, FrameElement, FrameState},
        frame_header::EthercatFrameHeader,
    },
};
use core::{ptr::NonNull, sync::atomic::AtomicU8};
use ethercrab_wire::EtherCrabWireSized;

/// An EtherCAT frame that is ready to be sent over the network.
///
/// This struct can be acquired by calling
/// [`PduLoop::next_sendable_frame`](crate::pdu_loop::PduTx::next_sendable_frame).
///
/// # Examples
///
/// ```rust,no_run
/// # use ethercrab::PduStorage;
/// use core::future::poll_fn;
/// use core::task::Poll;
///
/// # static PDU_STORAGE: PduStorage<2, { PduStorage::element_size(2) }> = PduStorage::new();
/// let (mut pdu_tx, _pdu_rx, _pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
///
/// let mut buf = [0u8; 1530];
///
/// poll_fn(|ctx| {
///     // Set the waker so this future is polled again when new EtherCAT frames are ready to
///     // be sent.
///     pdu_tx.replace_waker(ctx.waker());
///
///     if let Some(frame) = pdu_tx.next_sendable_frame() {
///         frame.send_blocking(|data| {
///             // Send packet over the network interface here
///
///             // Return the number of bytes sent over the network
///             Ok(data.len())
///         });
///
///         // Wake the future so it's polled again in case there are more frames to send
///         ctx.waker().wake_by_ref();
///     }
///
///     Poll::<()>::Pending
/// });
/// ```
#[derive(Debug)]
pub struct SendableFrame<'sto> {
    pub(in crate::pdu_loop) inner: FrameBox<'sto>, //保存帧的指针，索引，数据长度
}

unsafe impl Send for SendableFrame<'_> {}

impl<'sto> SendableFrame<'sto> {
    pub(crate) fn claim_sending(
        frame: NonNull<FrameElement<0>>,
        pdu_idx: &'sto AtomicU8,
        frame_data_len: usize,
    ) -> Option<Self> {
        let frame = unsafe { FrameElement::claim_sending(frame)? }; //标记帧状态为正在发送

        Some(Self {
            inner: FrameBox::new(frame, pdu_idx, frame_data_len),
        })
    }

    /// The frame has been sent by the network driver.
    fn mark_sent(&self) {
        fmt::trace!("Frame index {} is sent", self.inner.storage_slot_index());

        self.inner.set_state(FrameState::Sent);
    }

    pub(crate) fn storage_slot_index(&self) -> u8 {
        self.inner.storage_slot_index()
    }

    /// Used on send failure to release the frame sending claim so the frame can attempt to be sent
    /// again, or reclaimed for reuse.
    fn release_sending_claim(&self) {
        self.inner.set_state(FrameState::Sendable);
    }

    fn as_bytes(&self) -> &[u8] {
        let frame = self.inner.ethernet_frame().into_inner();

        let len = EthernetFrame::<&[u8]>::buffer_len(
            EthercatFrameHeader::PACKED_LEN + self.inner.pdu_payload_len(),
        );

        &frame[0..len]
    }

    /// Get the Ethernet frame length of this frame.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    //使用阻塞回调函数（闭包）发送帧。同时指出，闭包必须返回通过网络接口发送的字节数，若该数值与传入闭包的数据包长度不一致，方法将返回错误。
    /// Send the frame using a blocking callback.
    ///
    /// The closure must return the number of bytes sent over the network interface. If this does
    /// not match the length of the packet passed to the closure, this method will return an error.
    pub fn send_blocking(
        self,
        send: impl FnOnce(&[u8]) -> Result<usize, Error>, //接收一个闭包作为参数，该闭包接收一个字节切片引用，返回 Result<usize, Error> 类型。FnOnce 表示该闭包只能被调用一次
    ) -> Result<usize, Error> {
        let len = self.as_bytes().len();

        //调用传入的闭包 send，将 self.as_bytes() 返回的字节切片作为参数传递给闭包，使用 match 语句处理闭包的返回结果
        match send(self.as_bytes()) {
            Ok(bytes_sent) if bytes_sent == len => {
                self.mark_sent();

                Ok(bytes_sent)
            }
            Ok(bytes_sent) => {
                self.release_sending_claim();

                Err(Error::PartialSend {
                    len,
                    sent: bytes_sent,
                })
            }
            Err(res) => {
                self.release_sending_claim();

                Err(res)
            }
        }
    }
}
