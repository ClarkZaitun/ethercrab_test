pub mod created_frame;
mod frame_box;
pub mod received_frame;
pub mod receiving_frame;
pub mod sendable_frame;

use crate::{
    error::PduError, ethernet::EthernetFrame, fmt, pdu_loop::frame_header::EthercatFrameHeader,
};
use atomic_waker::AtomicWaker;
use core::{
    ptr::{NonNull, addr_of, addr_of_mut},
    sync::atomic::{AtomicU16, Ordering},
};
use frame_box::FrameBox;

/// A marker value for empty frames with no pushed PDUs.
///
/// The upper value must be non-zero for sentinel comparisons to work.
pub const FIRST_PDU_EMPTY: u16 = 0xff00;

/// Frame state.
#[atomic_enum::atomic_enum]
#[derive(PartialEq, Default)]
//用于自动为类型实现 PartialEq trait。实现该 trait 后，类型的实例就能使用 == 和 != 操作符进行相等性比较。
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum FrameState {
    // SAFETY: Because we create a bunch of `Frame`s with `MaybeUninit::zeroed`, the `None` state
    // MUST be equal to zero. All other fields in `Frame` are overridden in `replace`, so there
    // should be no UB there.
    /// The frame is available ready to be claimed.
    #[default]
    None = 0,
    /// The frame is claimed with a zeroed data buffer and can be filled with command, data, etc
    /// ready for sending.
    Created = 1,
    /// The frame has been populated with data and is ready to send when the TX loop next runs.
    Sendable = 2,
    /// The frame is being sent over the network interface.
    Sending = 3,
    /// The frame was successfully sent, and is now waiting for a response from the network.
    Sent = 4,
    /// A frame response has been received and validation/parsing is in progress.
    RxBusy = 5,
    /// Frame response parsing is complete and the returned data is now stored in the frame. The
    /// frame and its data is ready to be returned in `Poll::Ready` of [`ReceiveFrameFut`].
    RxDone = 6,
    /// The frame TX/RX is complete, but the frame memory is still held by calling code.
    RxProcessing = 7,
}

/// An individual frame state, PDU header config, and data buffer.
///
/// # A frame's journey
///
// TODO: Update this journey! The current docs are out of date!
// The following flowchart describes a `FrameElement`'s state changes during its use:
//
// <img alt="A flowchart showing the different state transitions of FrameElement" src="https://mermaid.ink/svg/pako:eNqdUztv2zAQ_isHTgngtLuGDLVadGkQ2E7bQYBxEc82YYoU-LBsJPnvPVLMy_JULaR05PfS3ZNorSRRiY22Q7tDF2BVNwb4-eGwo2XAQFV1Zw3Bzc3tcyNQa9uuN6l4dd00Jh8D5cHYAejY6ujVgfQJrrYRHZpAJOHxBBhsp1rwCfAa8IBK46MmCBZaxlRmC0lKI54_Mc8d8Sqnkkohq-ptHzW_wX39wChdh0bOQGLA_wBrRDb3pUO3X3syMsnMVlc_v9_x8gf3BKu_oK3tz-Uuy_kpxWslc5TbkOA92AM5MBQG6_ZTuJTMZbhUGRUvCp6jljh9D9nCLCfroZdx7Y7Zwlyj6koZ0JcLDHRuZHH8Fv1pyjt-L7S_UStOWVnztXe2Je_H39j1mgIx3eIVPkNUVc60iJRZUA5zlDPw1k111Nx7l3TU7z05gsQQXSKdf2gnTsDkzoye2KzvreFN6owp0f2bhUt079VC-omG-18mPYMKu9HOm3uSxbx0tmfPZ7xptMRMdOQ6VJIn8SmxNyLsiEFExVvJqTWiMS98DmOwy5NpRRVcpJmIPZuhWuGWMUW1Qe35K0kVrPs1jnae8Jd_545fZQ" style="background: white; max-height: 800px" />
//
// Source (MermaidJS):
//
// ```mermaid
// flowchart TD
//    FrameState::None -->|"alloc_frame()\nFrame is now exclusively (guaranteed by atomic state) available to calling code"| FrameState::Created
//    FrameState::Created -->|populate PDU command, data| FrameState::Created
//    FrameState::Created -->|"frame.mark_sendable()\nTHEN\nWake TX loop"| FrameState::Sendable
//    FrameState::Sendable -->|TX loop sends over network| FrameState::Sending
//    FrameState::Sending -->|"RX loop receives frame, calls pdu_rx()\nClaims frame as receiving"| FrameState::RxBusy
//    FrameState::RxBusy -->|"Validation/processing complete\nReceivingFrame::mark_received()\nWake frame waker"| FrameState::RxDone
//    FrameState::RxDone -->|"Wake future\nCalling code can now use response data"| FrameState::RxProcessing
//    FrameState::RxProcessing -->|"Calling code is done with frame\nReceivedFrame::drop()"| FrameState::None
//    ```
#[derive(Debug)]
#[repr(C)] // 保证 FrameElement 结构体的内存布局遵循 C 语言的规则，确保在进行指针操作和与 C 代码交互时内存布局的一致性。
// 帧
pub struct FrameElement<const N: usize> {
    // 缓冲区的帧索引
    /// Ethernet frame index in storage. Has nothing to do with PDU header index field.
    storage_slot_index: u8,
    // 帧状态
    status: AtomicFrameState,
    waker: AtomicWaker,

    // 跟踪EtherCAT帧数据区已存在的数据报的总长度
    /// Keeps track of how much of the PDU data buffer has been consumed.
    pdu_payload_len: usize,

    // Atomic as we iterate over all `FrameElement`s and read this field when receiving a frame.
    /// Stores the PDU index of the first PDU written into this frame (if any).
    ///
    /// Used by the network RX code to do a linear search in the frame storage to find the storage
    /// behind the received frame.
    ///
    /// The lower byte stores the PDU index, the upper byte stores a sentinel used to signify
    /// whether the PDU has been set or not.
    first_pdu: AtomicU16, //该帧的第一个数据报的索引
    // 低字节存储 PDU 索引，高字节存储哨兵值，用于标记 PDU 索引是否已设置。在接收帧时，会遍历所有 FrameElement 实例并读取该字段。

    // MUST be the last element otherwise pointer arithmetic doesn't work for
    // `NonNull<FrameElement<0>>`.
    ethernet_frame: [u8; N], //必须作为结构体的最后一个字段，否则 NonNull<FrameElement<0>> 的指针算术操作会出错。
}

// 为 FrameElement<N> 实现 Default trait（生成默认值）
// 何时调用FrameElement::default()？
impl<const N: usize> Default for FrameElement<N> {
    fn default() -> Self {
        Self {
            status: AtomicFrameState::new(FrameState::None),
            ethernet_frame: [0; N],
            storage_slot_index: 0,
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
            waker: AtomicWaker::default(),
        }
    }
}

// 为 FrameElement<N> 定义自身的关联函数/方法（如状态操作、指针获取等）
impl<const N: usize> FrameElement<N> {
    /// 获取指向整个以太网帧数据的指针
    unsafe fn ptr(this: NonNull<FrameElement<N>>) -> NonNull<u8> {
        let buf_ptr: *mut [u8; N] = unsafe { addr_of_mut!((*this.as_ptr()).ethernet_frame) };
        let buf_ptr: *mut u8 = buf_ptr.cast();

        unsafe { NonNull::new_unchecked(buf_ptr) }
    }

    // 获取指向 EtherCAT 数据报的指针
    /// Get pointer to EtherCAT frame payload. i.e. the buffer after the end of the EtherCAT frame
    /// header where all the PDUs (header and data) go.
    unsafe fn ethercat_payload_ptr(this: NonNull<FrameElement<N>>) -> NonNull<u8> {
        unsafe {
            Self::ptr(this)
                .byte_add(EthernetFrame::<&[u8]>::header_len()) //偏移以太网帧头
                .byte_add(EthercatFrameHeader::header_len()) //偏移EtherCAT帧头
                .cast() //由于 byte_add 操作后指针类型为 NonNull<u8>，调用 cast 方法确保返回的指针类型符合方法的预期
        }
    }

    // 设置帧状态
    // 无返回值，原子写入一定不会失败吗？
    /// Set the frame's state without checking its current state.
    pub(in crate::pdu_loop) unsafe fn set_state(this: NonNull<FrameElement<N>>, state: FrameState) {
        let fptr = this.as_ptr(); // 转换为原始裸指针 *mut FrameElement<N>

        // 使用 addr_of_mut! 宏获取 status 字段的可变引用，避免潜在的未定义行为
        // addr_of_mut! 宏保证只获取成员的地址，不会实际访问该成员
        unsafe { (*addr_of_mut!((*fptr).status)).store(state, Ordering::Release) };
        // Ordering::Release 是内存顺序，它确保在存储操作之前的所有写操作不会被重排到存储操作之后，保证其他线程在看到新状态时，也能看到之前的写操作结果
    }

    //通过原子性的比较并交换操作，确保在多线程环境下安全地将帧的状态从 from 切换到 to。如果帧当前状态与 from 不匹配，操作会失败并返回实际状态；如果匹配，则成功切换状态并返回原始指针
    /// Atomically swap the frame state from `from` to `to`.
    ///
    /// If the frame is not currently in the given `from` state, this method will return an error
    /// with the actual current frame state.
    unsafe fn swap_state(
        this: NonNull<FrameElement<N>>,
        from: FrameState,
        to: FrameState,
    ) -> Result<NonNull<FrameElement<N>>, FrameState> {
        let fptr = this.as_ptr(); //将 NonNull<FrameElement<N>> 类型的 this 转换为原始裸指针 *mut FrameElement<N>

        unsafe {
            // 使用 addr_of_mut! 宏获取 status 字段的可变引用，避免潜在的未定义行为
            // compare_exchange 交换前，当前值必须是期望值，才会交换
            (*addr_of_mut!((*fptr).status)).compare_exchange(
                from,
                to,
                Ordering::AcqRel, //成功时的内存顺序，具有获取和释放语义。获取语义确保在交换操作之后的所有读操作不会被重排到交换操作之前；释放语义确保在交换操作之前的所有写操作不会被重排到交换操作之后
                Ordering::Relaxed, //失败时的内存顺序，只保证操作的原子性，不提供任何跨线程的内存同步保证
            )
        }?; //如果 compare_exchange 操作失败，方法会提前返回 Err，错误值为帧当前实际的状态

        Ok(this)
    }

    //原子性地将帧缓冲区一个帧从"未使用"状态(None)切换到"已创建"状态(Created)
    /// Attempt to clame a frame element as CREATED. Succeeds if the selected FrameElement is
    /// currently in the NONE state.
    unsafe fn claim_created(
        this: NonNull<FrameElement<N>>,
        frame_index: u8,
    ) -> Result<NonNull<FrameElement<N>>, PduError> {
        // SAFETY: We atomically ensure the frame is currently available to use which guarantees no
        // other thread could take it from under our feet.
        //
        // It is imperative that we check the existing state when claiming a frame as created. It
        // matters slightly less for all other state transitions because once we have a created
        // frame nothing else is able to take it unless it is put back into the `None` state.
        let this = unsafe { Self::swap_state(this, FrameState::None, FrameState::Created) }
            .map_err(|e| {
                fmt::trace!(
                    "Failed to claim frame {}: status is {:?}, expected {:?}",
                    frame_index,
                    e,
                    FrameState::None
                );

                PduError::SwapState
            })?;

        unsafe {
            (*addr_of_mut!((*this.as_ptr()).storage_slot_index)) = frame_index;
            (*addr_of_mut!((*this.as_ptr()).pdu_payload_len)) = 0;
        }

        Ok(this)
    }

    //借助 swap_state 方法原子性地尝试将帧的状态从可发送切换到正在发送。若切换成功，返回包含原始指针的 Some；若失败，返回 None
    unsafe fn claim_sending(this: NonNull<FrameElement<N>>) -> Option<NonNull<FrameElement<N>>> {
        unsafe { Self::swap_state(this, FrameState::Sendable, FrameState::Sending) }.ok()
        //.ok()：Result 类型的方法，作用是将 Result 转换为 Option。若 Result 是 Ok，则返回 Some，并将 Ok 中的值提取出来；若 Result 是 Err，则返回 None。
    }

    //尝试将一个处于 Sent 状态的帧标记为 RxBusy 状态，即声明该帧开始接收响应数据
    unsafe fn claim_receiving(this: NonNull<FrameElement<N>>) -> Option<NonNull<FrameElement<N>>> {
        unsafe { Self::swap_state(this, FrameState::Sent, FrameState::RxBusy) }
            .map_err(|actual_state| {
                fmt::error!(
                    "Failed to claim receiving frame {}: expected state {:?}, but got {:?}",
                    unsafe { *addr_of_mut!((*this.as_ptr()).storage_slot_index) },
                    FrameState::Sent,
                    actual_state
                );
            })
            .ok()
    }

    unsafe fn storage_slot_index(this: NonNull<FrameElement<0>>) -> u8 {
        unsafe { *addr_of!((*this.as_ptr()).storage_slot_index) }
    }

    //检查当前帧的首个 PDU 索引是否与 search 相等
    pub(in crate::pdu_loop) unsafe fn first_pdu_is(
        this: NonNull<FrameElement<0>>,
        search: u8,
    ) -> bool {
        let raw = unsafe { (*addr_of!((*this.as_ptr()).first_pdu)).load(Ordering::Acquire) };

        // Unused sentinel value occupies upper byte, so this equality will never hold for empty
        // frames
        //由于 first_pdu 的高字节是未使用的哨兵值，对于空帧，该等式永远不成立，因为空帧的 first_pdu 值为 FIRST_PDU_EMPTY
        u16::from(search) == raw
    }

    // 原子地设置（如果还没设置）帧中第一个数据报的index，通过替换FIRST_PDU_EMPTY为u8的index实现
    // 这里需要注意内存顺序。保证前面的数据已经写入
    /// If no PDUs are present in the frame, set the first PDU index to the given value.
    unsafe fn set_first_pdu(this: NonNull<FrameElement<0>>, value: u8) {
        let first_pdu = unsafe { &mut *addr_of_mut!((*this.as_ptr()).first_pdu) };

        // Only set first PDU index if the frame is empty, as denoted by the `FIRST_PDU_EMPTY`
        // sentinel. Failures are ignored as we want a noop when the first PDU value was already
        // set.
        let _ = first_pdu.compare_exchange(
            FIRST_PDU_EMPTY,
            u16::from(value),
            Ordering::Release, //确保在成功交换后，之前的写操作不会被重排到交换之后
            Ordering::Relaxed, //失败时不提供任何内存顺序保证
        );
    }

    /// Clear first PDU.
    unsafe fn clear_first_pdu(this: NonNull<FrameElement<0>>) {
        let first_pdu = unsafe { &*addr_of!((*this.as_ptr()).first_pdu) };

        first_pdu.store(FIRST_PDU_EMPTY, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdu_loop::frame_element::{AtomicFrameState, FIRST_PDU_EMPTY, FrameElement};
    use atomic_waker::AtomicWaker;
    use core::{ptr::NonNull, sync::atomic::AtomicU16};

    #[test]
    fn set_first_pdu_only_once() {
        crate::test_logger();

        const BUF_LEN: usize = 16;

        let frame = FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        };

        let frame_ptr = NonNull::from(&frame);

        unsafe { FrameElement::<0>::set_first_pdu(frame_ptr.cast(), 0xab) };
        unsafe { FrameElement::<0>::set_first_pdu(frame_ptr.cast(), 0xcd) };

        assert_eq!(frame.first_pdu.load(Ordering::Relaxed), 0xab);
    }

    #[test]
    fn find_empty_frame() {
        crate::test_logger();

        const BUF_LEN: usize = 16;

        let frame = FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        };

        let frame_ptr = NonNull::from(&frame);

        assert!(!unsafe { FrameElement::<0>::first_pdu_is(frame_ptr.cast(), 0) });
    }

    #[test]
    fn find_frame_zero() {
        crate::test_logger();

        const BUF_LEN: usize = 16;

        let frame = FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        };

        let frame_ptr = NonNull::from(&frame);

        unsafe { FrameElement::<0>::set_first_pdu(frame_ptr.cast(), 0) }

        assert!(unsafe { FrameElement::<0>::first_pdu_is(frame_ptr.cast(), 0) });
    }

    #[test]
    fn find_frame_1() {
        crate::test_logger();

        const BUF_LEN: usize = 16;

        let frame_0 = FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        };

        let frame_ptr_0 = NonNull::from(&frame_0);

        unsafe { FrameElement::<0>::set_first_pdu(frame_ptr_0.cast(), 123) }

        // ---

        let frame_1 = FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        };

        let frame_ptr_1 = NonNull::from(&frame_1);

        unsafe { FrameElement::<0>::set_first_pdu(frame_ptr_1.cast(), 0xff) }

        // ---

        assert!(!unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_0.cast(), 0) });
        assert!(unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_0.cast(), 123) });
        assert!(!unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_0.cast(), 0xff) });

        assert!(!unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_1.cast(), 0) });
        assert!(!unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_1.cast(), 123) });
        assert!(unsafe { FrameElement::<0>::first_pdu_is(frame_ptr_1.cast(), 0xff) });
    }

    // A sanity check to make sure we get hold of a pointer to the start of the ethernet frame array
    // and not the start of the struct. This test is added due to a regression caused by refactoring
    // the `ethercat_payload_ptr` method.
    #[test]
    fn payload_offset() {
        const N: usize = 32;
        // Minus ethernet header and EtherCAT header
        const ETHERCAT_PAYLOAD: usize = N - 14 - 2;

        let frame = FrameElement {
            storage_slot_index: 0xaa,
            // 5
            status: AtomicFrameState::new(FrameState::RxBusy),
            waker: AtomicWaker::default(),
            // Should be zero but we'll set it to a random value for debugging
            pdu_payload_len: 0xbb,
            first_pdu: AtomicU16::new(0xcc),
            // Fill with a canary value
            ethernet_frame: [0xabu8; N],
        };

        let ptr = NonNull::from(&frame);

        let payload = unsafe { FrameElement::<N>::ethercat_payload_ptr(ptr) };

        let raw =
            unsafe { core::slice::from_raw_parts(payload.as_ptr() as *const u8, ETHERCAT_PAYLOAD) };

        assert_eq!(raw, &[0xabu8; ETHERCAT_PAYLOAD]);
    }
}
