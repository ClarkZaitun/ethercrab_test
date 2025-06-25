use crate::{
    Command, PduLoop,
    error::PduError,
    fmt,
    generate::write_packed,
    pdu_loop::{
        frame_element::{FrameBox, FrameElement, FrameState, receiving_frame::ReceiveFrameFut},
        frame_header::EthercatFrameHeader,
        pdu_flags::PduFlags,
        pdu_header::PduHeader,
    },
};
use core::{ptr::NonNull, sync::atomic::AtomicU8, time::Duration};
use ethercrab_wire::{
    EtherCrabWireRead, EtherCrabWireSized, EtherCrabWireWrite, EtherCrabWireWriteSized,
};

/// A frame in a freshly allocated state.
///
/// This typestate may only be created by
/// [`alloc_frame`](crate::pdu_loop::storage::PduStorageRef::alloc_frame).
#[derive(Debug)]
pub struct CreatedFrame<'sto> {
    inner: FrameBox<'sto>, //帧
    pdu_count: u8,         //当前数据报数量
    /// Position of the last frame's header in the payload.
    ///
    /// Used for updating the `more_follows` flag when pushing a new PDU.
    // 最后一帧的标头在有效负载中的位置。
    // 用于在推送新 PDU 时更新“more_follows”标志。
    last_header_location: Option<usize>,
}

impl<'sto> CreatedFrame<'sto> {
    /// The size of a completely empty PDU.
    ///
    /// Includes header and 2 bytes for working counter.
    pub const PDU_OVERHEAD_BYTES: usize = PduHeader::PACKED_LEN + 2; // 12

    // 从帧缓冲区获取一个空闲帧，重置它
    // 可优化：以太网帧头不需要重置，少了一次赋值
    pub(in crate::pdu_loop) fn claim_created(
        frame: NonNull<FrameElement<0>>,
        frame_index: u8,
        pdu_idx: &'sto AtomicU8,
        frame_data_len: usize,
    ) -> Result<Self, PduError> {
        //原子性地将帧缓冲区一个帧从"未使用"状态(None)切换到"已创建"状态(Created)
        let frame = unsafe { FrameElement::claim_created(frame, frame_index)? };

        let mut inner = FrameBox::new(frame, pdu_idx, frame_data_len);

        // 重置以太网和 EtherCAT 标头（可以省略），将以太网帧有效载荷数据清零。
        inner.init();

        Ok(Self {
            inner,
            pdu_count: 0,
            last_header_location: None,
        })
    }

    pub fn storage_slot_index(&self) -> u8 {
        self.inner.storage_slot_index()
    }

    pub fn is_empty(&self) -> bool {
        self.pdu_count == 0
    }

    /// The frame has been initialised, filled with a data payload (if required), and is now ready
    /// to be sent.
    ///
    /// This method returns a future that should be fulfilled when a response to the sent frame is
    /// received.
    // 帧设置为可发送状态Sendable，返回一个 Future，当收到对已发送帧的响应时，该 Future 将被执行。
    // 前文已经写入帧的数据报，以太网帧头，本函数会写入EtherCAT帧头，组帧完成。
    pub fn mark_sendable(
        mut self,
        pdu_loop: &'sto PduLoop<'sto>,
        timeout: Duration,
        retries: usize,
    ) -> ReceiveFrameFut<'sto> {
        // 创建EtherCAT帧头，压缩数据长度和协议到2字节的EtherCAT帧头中
        EthercatFrameHeader::pdu(self.inner.pdu_payload_len() as u16) // 创建EtherCAT帧头
            .pack_to_slice_unchecked(self.inner.ecat_frame_header_mut()); // ecat_frame_header_mut获取EtherCAT帧头字节切片

        // 设置帧状态
        self.inner.set_state(FrameState::Sendable);

        // 创建future
        ReceiveFrameFut {
            frame: Some(self.inner),
            pdu_loop,
            timeout_timer: crate::timer_factory::timer(timeout), // 创建一个定时器
            timeout,
            retries_left: retries,
        }
    }

    /// Push a PDU into this frame, consuming as much space as possible.
    ///
    /// Returns the number of bytes from the given `data` that were written into the frame, or
    /// `None` if the input slice is empty or the frame is full.
    pub(crate) fn push_pdu_slice_rest(
        &mut self,
        command: Command,
        bytes: &[u8],
    ) -> Result<Option<(usize, PduResponseHandle)>, PduError> {
        let consumed = self.inner.pdu_payload_len();

        if bytes.is_empty() {
            return Ok(None);
        }

        // The maximum number of bytes we can insert into this frame
        let max_bytes = self
            .inner
            .pdu_buf()
            .len()
            .saturating_sub(consumed)
            .saturating_sub(Self::PDU_OVERHEAD_BYTES);

        if max_bytes == 0 {
            fmt::trace!("Pushed 0 bytes of {} into PDU", bytes.len());

            return Ok(None);
        }

        let sub_slice_len = max_bytes.min(bytes.packed_len());

        let bytes = &bytes[0..sub_slice_len];

        let flags = PduFlags::new(sub_slice_len as u16, false);

        let alloc_size = sub_slice_len + Self::PDU_OVERHEAD_BYTES;

        let buf_range = consumed..(consumed + alloc_size);

        // Establish mapping between this PDU index and the Ethernet frame it's being put in
        let pdu_idx = self.inner.next_pdu_idx();

        fmt::trace!(
            "Write PDU {:#04x} into rest of frame index {} ({}, {} frame bytes + {} payload bytes at {:?})",
            pdu_idx,
            self.inner.storage_slot_index(),
            command,
            sub_slice_len,
            Self::PDU_OVERHEAD_BYTES,
            buf_range
        );

        let l = self.inner.pdu_buf_mut().len();

        let pdu_buf = self
            .inner
            .pdu_buf_mut()
            .get_mut(buf_range.clone())
            .ok_or_else(|| {
                fmt::error!(
                    "Fill rest of PDU buf range too long: wanted {:?} from {:?}",
                    buf_range,
                    0..l
                );

                PduError::TooLong
            })?;

        let header = PduHeader {
            command_code: command.code(),
            index: pdu_idx,
            command_raw: command.pack(),
            flags,
            irq: 0,
        };

        let pdu_buf = write_packed(header, pdu_buf);

        // Payload
        let _pdu_buf = write_packed(bytes, pdu_buf);

        // Next two bytes are working counter, but they are always zero on send (and the buffer is
        // zero-initialised) so there's nothing to do.

        // Don't need to check length here as we do that with `pdu_buf_mut().get_mut()` above.
        self.inner.add_pdu(alloc_size, pdu_idx);

        let index_in_frame = self.pdu_count;

        self.pdu_count += 1;

        // Frame was added successfully, so now we can update the previous PDU `more_follows` flag to true.
        if let Some(last_header_location) = self.last_header_location.as_mut() {
            // Flags start at 6th bit of header
            let flags_offset = 6usize;

            let last_flags_buf = fmt::unwrap_opt!(
                self.inner
                    .pdu_buf_mut()
                    .get_mut((*last_header_location + flags_offset)..)
            );

            let mut last_flags = fmt::unwrap!(PduFlags::unpack_from_slice(last_flags_buf));

            last_flags.more_follows = true;

            last_flags.pack_to_slice_unchecked(last_flags_buf);

            // Previous header is now the one we just inserted
            *last_header_location = buf_range.start;
        } else {
            self.last_header_location = Some(0);
        }

        Ok(Some((
            sub_slice_len,
            PduResponseHandle {
                index_in_frame,
                pdu_idx,
                command_code: command.code(),
                alloc_size,
            },
        )))
    }

    pub(crate) fn can_push_pdu_payload(&self, packed_len: usize) -> bool {
        let alloc_size = packed_len + Self::PDU_OVERHEAD_BYTES;

        let start_byte = self.inner.pdu_payload_len();

        start_byte + alloc_size <= self.inner.pdu_buf().len()
    }

    // 在帧中插入一个数据报，没有处理帧空间不足的情况
    // 更改帧的字节切片，因此效率较高
    /// Push a PDU into this frame.
    ///
    /// # Errors
    ///
    /// Returns [`PduError::TooLong`] if the remaining space in the frame is not enough to hold the
    /// new PDU.
    pub fn push_pdu(
        &mut self,
        command: Command,
        data: impl EtherCrabWireWrite,
        len_override: Option<u16>,
    ) -> Result<PduResponseHandle, PduError> {
        // 如果有 len_override 参数（Some值），则使用该值与 data.packed_len() 中的较大值
        // 如果没有 len_override 参数（None），则直接使用 data.packed_len()
        let data_length_usize =
            len_override.map_or(data.packed_len(), |l| usize::from(l).max(data.packed_len()));

        // 新建EtherCAT数据报头flag字段（2字节）
        let flags = PduFlags::new(data_length_usize as u16, false);

        // PDU header + data + working counter (space is required for the response value - we never
        // actually write it)
        let alloc_size = data_length_usize + Self::PDU_OVERHEAD_BYTES; //待插入的完整数据报的长度

        // 此帧中已消耗的payload字节数（例如，来自先前插入的数据报）。这是我们要推送的当前数据报的起始字节
        // The number of payload bytes already consumed in this frame (e.g. from prior PDU
        // insertions). This is the start byte of the current PDU we want to push.
        let start_byte = self.inner.pdu_payload_len();

        // 获取待插入数据报总长度应该包括的字节片
        // Comprises PDU header, body, working counter
        let buf_range = start_byte..(start_byte + alloc_size);

        // 原子性地获取并递增数据报index
        // 建立此 PDU 索引和它所在的以太网帧之间的映射？
        // Establish mapping between this PDU index and the Ethernet frame it's being put in
        let pdu_idx = self.inner.next_pdu_idx();

        fmt::trace!(
            "Write PDU {:#04x} into frame index {} ({}, {} bytes at {:?})",
            pdu_idx,
            self.inner.storage_slot_index(),
            command,
            data_length_usize,
            buf_range
        );

        // 整个以太网帧中可用于EtherCAT数据报的区域长度
        let l = self.inner.pdu_buf_mut().len();

        // 检查EtherCAT数据报的区域剩余空间是否能放下新的数据报，成功返回字节裸指针，失败报错
        let pdu_buf = self
            .inner
            .pdu_buf_mut()
            .get_mut(buf_range.clone())
            .ok_or_else(|| {
                fmt::trace!(
                    "Push PDU buf range too long: wanted {:?} from {:?}",
                    buf_range,
                    0..l
                );

                PduError::TooLong //如果空间不足，返回这个错误
            })?;

        let header = PduHeader {
            command_code: command.code(), //命令转换为具体数字
            index: pdu_idx,
            command_raw: command.pack(), //命令转换为从站地址和寄存器地址
            flags,                       // 怎么从PduFlags压缩到2字节的？
            irq: 0,
        };

        // 打包数据报头并将它写入buff的开头，返回剩余未使用的buff（字节切片）
        let pdu_buf = write_packed(header, pdu_buf);

        // 打包数据报的数据
        // Payload
        let _pdu_buf = write_packed(data, pdu_buf);

        // 接下来的两个字节是工作计数器，但它们在发送时始终为零（并且缓冲区已初始化为零），因此无需执行任何操作。
        // Next two bytes are working counter, but they are always zero on send (and the buffer is
        // zero-initialised) so there's nothing to do.

        // 更新EtherCAT帧数据区已存在的数据报的总长度，原子地设置（如果还没设置）帧中第一个数据报的index
        // 这里不需要检查长度，因为我们上面使用 `pdu_buf_mut().get_mut()` 来检查长度。
        // Don't need to check length here as we do that with `pdu_buf_mut().get_mut()` above.
        self.inner.add_pdu(alloc_size, pdu_idx);

        let index_in_frame = self.pdu_count;

        self.pdu_count += 1;

        // 新数据报已成功添加，因此将上一个数据报“more_follows”标志更新为 true
        // Frame was added successfully, so now we can update the previous PDU `more_follows` flag to true.
        if let Some(last_header_location) = self.last_header_location.as_mut() {
            // Flags start at 6th bit of header
            // more_follows标志位在数据报头第六字节，bit 0
            let flags_offset = 6usize;

            // 获取前一个数据报的标志位所在的字节切片
            let last_flags_buf = fmt::unwrap_opt!(
                self.inner
                    .pdu_buf_mut()
                    .get_mut((*last_header_location + flags_offset)..) //获取上一个帧头的起始地址+偏移之后字节切片
            );

            // 解包前一个PDU的标志位
            let mut last_flags = fmt::unwrap!(PduFlags::unpack_from_slice(last_flags_buf));

            last_flags.more_follows = true;

            // 将PDU标志位打包到字节缓冲区中，压缩为2字节
            last_flags.pack_to_slice_unchecked(last_flags_buf);

            // Previous header is now the one we just inserted
            *last_header_location = buf_range.start;
        } else {
            self.last_header_location = Some(0);
        }

        Ok(PduResponseHandle {
            index_in_frame,
            pdu_idx,
            command_code: command.code(),
            alloc_size,
        })
    }
}

impl Drop for CreatedFrame<'_> {
    fn drop(&mut self) {
        // ONLY free the frame if it's still in created state. If it's been moved into
        // sending/sent/receiving/etc, we must leave it alone.
        let _ = self.inner.swap_state(FrameState::Created, FrameState::None);
    }
}

// SAFETY: This unsafe impl is required due to `FrameBox` containing a `NonNull`, however this impl
// is ok because FrameBox also holds the lifetime `'sto` of the backing store, which is where the
// `NonNull<FrameElement>` comes from.
//
// For example, if the backing storage is is `'static`, we can send things between threads. If it's
// not, the associated lifetime will prevent the framebox from being used in anything that requires
// a 'static bound.
unsafe impl Send for CreatedFrame<'_> {}

#[derive(Debug)]
#[cfg_attr(test, derive(Eq, PartialEq))]
pub struct PduResponseHandle {
    // Might want this in the future
    #[allow(unused)]
    // 帧中的序号
    pub index_in_frame: u8,

    // 数据报索引和命令码，用于验证响应帧是否匹配
    // 这样可能还不够唯一，需要加上从站+地址
    /// PDU wire index and command used to validate response match.
    pub pdu_idx: u8,
    pub command_code: u8,

    // 整个数据报长度
    /// The number of bytes allocated for the PDU header, payload and WKC in the frame.
    pub alloc_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PduStorage, RegisterAddress,
        ethernet::EthernetFrame,
        pdu_loop::frame_element::{AtomicFrameState, FIRST_PDU_EMPTY, FrameElement},
    };
    use atomic_waker::AtomicWaker;
    use core::{
        cell::UnsafeCell,
        ptr::NonNull,
        sync::atomic::{AtomicU8, AtomicU16},
    };

    #[test]
    fn chunked_send() {
        crate::test_logger();

        const MAX_PAYLOAD: usize = 32;

        const BUF_LEN: usize = PduStorage::element_size(MAX_PAYLOAD);

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let whatever_handle = created.push_pdu(Command::frmw(0x1000, 0x0918).into(), 0u64, None);

        assert!(whatever_handle.is_ok());

        let big_frame = [0xaau8; MAX_PAYLOAD * 2];

        let (rest, _handle) = created
            .push_pdu_slice_rest(Command::fpwr(0x1000, 0x0918).into(), &big_frame)
            .expect("Should not fail")
            .unwrap();

        assert_eq!(rest, 12);
    }

    #[test]
    fn too_long() {
        crate::test_logger();

        const BUF_LEN: usize = 16;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let handle = created.push_pdu(Command::fpwr(0x1000, 0x0918).into(), [0xffu8; 9], None);

        assert_eq!(handle.unwrap_err(), PduError::TooLong);
    }

    #[test]
    fn auto_more_follows() {
        crate::test_logger();

        const BUF_LEN: usize = 64;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let handle = created.push_pdu(Command::fpwr(0x1000, 0x0918).into(), (), None);
        assert!(handle.is_ok());

        let handle = created.push_pdu(Command::fpwr(0x1001, 0x0918).into(), (), None);
        assert!(handle.is_ok());

        let handle = created.push_pdu(Command::fpwr(0x1002, 0x0918).into(), (), None);
        assert!(handle.is_ok());

        const FLAGS_OFFSET: usize = 6;

        assert_eq!(
            created.inner.pdu_buf()[FLAGS_OFFSET..][..2],
            PduFlags::new(0, true).pack()
        );

        assert_eq!(
            created.inner.pdu_buf()[PduHeader::PACKED_LEN + 2 + FLAGS_OFFSET..][..2],
            PduFlags::new(0, true).pack()
        );

        assert_eq!(
            created.inner.pdu_buf()[(PduHeader::PACKED_LEN + 2 + FLAGS_OFFSET) * 2..][..2],
            PduFlags::new(0, false).pack()
        );
    }

    #[test]
    fn push_rest_too_long() {
        crate::test_logger();

        const BUF_LEN: usize = 32;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let data = [0xaau8; 128];

        let res = created.push_pdu_slice_rest(Command::Nop, &data);

        // 32 byte frame contains all the headers with a little bit left over for writing some data
        // into.
        let expected_written = BUF_LEN
            - CreatedFrame::PDU_OVERHEAD_BYTES
            - EthercatFrameHeader::header_len()
            - EthernetFrame::<&[u8]>::header_len();

        // Just double checking
        assert_eq!(expected_written, 4);

        assert_eq!(
            res,
            Ok(Some((
                expected_written,
                PduResponseHandle {
                    index_in_frame: 0,
                    pdu_idx: 0,
                    command_code: 0,
                    // The size of this PDU with a 4 byte payload (plus 12 byte header)
                    alloc_size: 4 + CreatedFrame::PDU_OVERHEAD_BYTES
                }
            )))
        );

        // Can't push anything else
        let res = created.push_pdu_slice_rest(Command::Nop, &data);

        assert_eq!(res, Ok(None));
    }

    #[test]
    fn push_rest_after_dc_sync() {
        crate::test_logger();

        const BUF_LEN: usize = 64;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let dc_handle = created
            .push_pdu(
                Command::frmw(0x1000, RegisterAddress::DcSystemTime.into()).into(),
                0u64,
                None,
            )
            .expect("DC handle");

        // 12 byte PDU header plus 8 byte payload
        assert_eq!(dc_handle.alloc_size, 20);

        let data = [0xaau8; 128];

        let remaining = BUF_LEN
            - EthernetFrame::<&[u8]>::header_len()
            - EthercatFrameHeader::header_len()
            - dc_handle.alloc_size;

        // Just double checking
        assert_eq!(remaining, 28);

        let res = created.push_pdu_slice_rest(Command::Nop, &data);

        assert_eq!(
            res,
            Ok(Some((
                remaining - CreatedFrame::PDU_OVERHEAD_BYTES,
                PduResponseHandle {
                    index_in_frame: 1,
                    pdu_idx: 1,
                    command_code: 0,
                    alloc_size: remaining
                }
            )))
        );

        // Can't push anything else
        let res = created.push_pdu_slice_rest(Command::Nop, &data);

        assert_eq!(res, Ok(None));
    }

    #[test]
    fn push_rest_empty() {
        crate::test_logger();

        const BUF_LEN: usize = 64;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            storage_slot_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: AtomicU16::new(FIRST_PDU_EMPTY),
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        assert_eq!(
            created.push_pdu_slice_rest(
                Command::frmw(0x1000, RegisterAddress::DcSystemTime.into()).into(),
                &[]
            ),
            Ok(None)
        );
    }
}
