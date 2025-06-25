use super::{
    frame_element::FrameState, frame_header::EthercatFrameHeader, pdu_rx::PduRx, pdu_tx::PduTx,
};
use crate::ethernet::EthernetFrame;
use crate::{
    PduLoop,
    error::{Error, PduError},
    fmt,
    pdu_loop::{
        frame_element::{
            FrameElement, created_frame::CreatedFrame, receiving_frame::ReceivingFrame,
        },
        pdu_flags::PduFlags,
    },
};
use atomic_waker::AtomicWaker;
use core::{
    alloc::Layout,
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};
use ethercrab_wire::EtherCrabWireSized;

/// Smallest frame size with a data payload of 0 length //28
const MIN_DATA: usize = EthernetFrame::<&[u8]>::buffer_len(
    //ethernet header 14 +
    EthercatFrameHeader::header_len() //2
                    + super::pdu_header::PduHeader::PACKED_LEN // 10？
                    // PDU payload
                    + PduFlags::const_default().len() as usize // 0
                    // Working counter
                    + 2,
);

/// Stores PDU frames that are currently being prepared to send, in flight, or being received and
/// processed.
///
/// The number of storage elements `N` must be a power of 2.
pub struct PduStorage<const N: usize, const DATA: usize> {
    frames: UnsafeCell<MaybeUninit<[FrameElement<DATA>; N]>>,
    // 缓冲区中的帧索引，用于遍历缓冲区
    frame_idx: AtomicU8,
    // 数据报的index
    pdu_idx: AtomicU8,
    // 用于在初始化时标记是否已经从这个帧缓冲区PduStorage分割出：发送通道、接收通道和 PDU 循环
    is_split: AtomicBool,
    //
    /// A waker used to wake up the TX task when a new frame is ready to be sent.
    pub(in crate::pdu_loop) tx_waker: AtomicWaker,
    /// A flag used to signal that the TX/RX loop should exit.
    ///
    /// Used by [`MainDevice::release`](crate::MainDevice::release) et al.
    exit_flag: AtomicBool,
}

unsafe impl<const N: usize, const DATA: usize> Sync for PduStorage<N, DATA> {}

impl PduStorage<0, 0> {
    /// Calculate the size of a `PduStorage` buffer element to hold the given number of data bytes.
    ///
    /// This computes the additional overhead the Ethernet, EtherCAT frame and EtherCAT PDU headers
    /// require.
    ///
    /// # Examples
    ///
    /// Create a `PduStorage` for a process data image of 128 bytes:
    ///
    /// ```rust
    /// use ethercrab::PduStorage;
    ///
    /// const NUM_FRAMES: usize = 16;
    /// const FRAME_SIZE: usize = PduStorage::element_size(128);
    ///
    /// // 28 byte overhead
    /// assert_eq!(FRAME_SIZE, 156);
    ///
    /// let storage = PduStorage::<NUM_FRAMES, FRAME_SIZE>::new();
    /// ```
    pub const fn element_size(data_len: usize) -> usize {
        //用const定义常量函数，常量函数能在编译时执行，生成编译时常量。
        MIN_DATA + data_len
    }
}

impl<const N: usize, const DATA: usize> PduStorage<N, DATA> {
    /// Create a new `PduStorage` instance.
    ///
    /// It is recommended to use [`element_size`](PduStorage::element_size) to correctly compute the
    /// overhead required to hold a given PDU payload size.
    ///
    /// # Panics
    ///
    /// This method will panic if
    ///
    /// - `N` is larger than `u8::MAX, or not a power of two, or
    /// - `DATA` is less than 28 as this is the minimum size required to hold an EtherCAT frame with
    ///   zero PDU length.
    pub const fn new() -> Self {
        // MSRV: Make `N` a `u8` when `generic_const_exprs` is stablised
        // If possible, try using `NonZeroU8`.
        // NOTE: Keep max frames in flight at 256 or under. This way, we can guarantee the first PDU
        // in any frame has a unique index.
        assert!(
            N <= u8::MAX as usize,
            "Packet indexes are u8s, so cache array cannot be any bigger than u8::MAX"
        );
        assert!(N > 0, "Storage must contain at least one element");

        assert!(
            DATA >= MIN_DATA,
            "DATA must be at least 28 bytes large to hold all frame headers"
        );

        // Index wrapping limitations require a power of 2 number of storage elements.
        if N > 1 {
            assert!(
                N.count_ones() == 1,
                "The number of storage elements must be a power of 2" // ？？
            );
        }

        let frames = UnsafeCell::new(MaybeUninit::zeroed()); //  会创建一个未初始化的 [FrameElement<DATA>; N] 数组，并将其内存填充为零字节（所有位为 0）。自动调用FrameElement::default()？

        Self {
            frames,
            frame_idx: AtomicU8::new(0),
            pdu_idx: AtomicU8::new(0),
            is_split: AtomicBool::new(false),
            tx_waker: AtomicWaker::new(),
            exit_flag: AtomicBool::new(false),
        }
    }

    /// Create a PDU loop backed by this storage.
    ///
    /// Returns a TX and RX driver, and a handle to the PDU loop. This method will return an error
    /// if called more than once.
    ///
    /// # Errors
    ///
    /// To maintain ownership and lifetime invariants, `try_split` will return an error if called
    /// more than once on any given `PduStorage`.
    #[allow(clippy::result_unit_err)] //禁止 clippy 工具对 Result 类型使用 () 作为错误类型的警告。
    pub fn try_split(&self) -> Result<(PduTx<'_>, PduRx<'_>, PduLoop<'_>), ()> {
        self.is_split
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed) //Ordering::AcqRel 表示在成功时具有获取和释放语义，Ordering::Relaxed 表示在失败时使用宽松的内存顺序。
            // TODO: Make try_split const when ? is allowed in const methods, tracking issue
            // <https://github.com/rust-lang/rust/issues/74935>
            .map_err(|_| ())?; //将 compare_exchange 返回的错误转换为 ()，如果操作失败（即 is_split 已经为 true），则提前返回错误。

        //将 PduStorage 转换为 PduStorageRef 类型的引用
        let storage = self.as_ref();

        Ok((
            PduTx::new(storage.clone()),
            PduRx::new(storage.clone()),
            PduLoop::new(storage),
        ))
    }

    fn as_ref(&self) -> PduStorageRef {
        PduStorageRef {
            //get() 方法返回其内部的裸指针，然后通过 cast() 方法将其转换为 NonNull<FrameElement<DATA>> 类型。
            //NonNull::new_unchecked() 方法用于创建一个非空的裸指针，它接受一个非空的裸指针作为参数，并返回一个 NonNull 类型的指针。
            frames: unsafe { NonNull::new_unchecked(self.frames.get().cast()) },
            //Layout::array::<FrameElement<DATA>>(N) 创建一个包含 N 个 FrameElement<DATA> 类型元素的数组的内存布局。
            //unwrap() 方法用于获取布局的大小（size），并返回一个 usize 类型的值。
            //size() / N 表示每个 FrameElement 在内存中的字节跨度，用于计算不同帧元素在内存中的偏移量。
            //N 表示存储的帧元素的数量。
            frame_element_stride: Layout::array::<FrameElement<DATA>>(N).unwrap().size() / N,
            num_frames: N,
            frame_data_len: DATA,
            frame_idx: &self.frame_idx,
            pdu_idx: &self.pdu_idx,
            tx_waker: &self.tx_waker,
            exit_flag: &self.exit_flag,
            //标记 PduStorageRef 对象的生命周期，确保其生命周期与 PduStorage 实例一致。
            _lifetime: PhantomData,
        }
    }
}

//该结构体用于引用 PduStorage 实例，封装了对 PduStorage 内部状态的访问。
#[derive(Debug, Clone)]
pub(crate) struct PduStorageRef<'sto> {
    //NonNull<FrameElement<0>> 是一个非空裸指针，指向 FrameElement<0> 类型的对象。NonNull 类型保证指针不为空，用于安全地操作 PduStorage 中的帧元素数组
    frames: NonNull<FrameElement<0>>,
    /// Stride in bytes used to calculate frame element index pointer offsets.
    //表示每个 FrameElement 在内存中的字节跨度，用于计算不同帧元素在内存中的偏移量。
    frame_element_stride: usize,
    //记录 PduStorage 中存储的帧元素的数量。
    pub num_frames: usize,
    //表示每个帧元素的数据长度，在缓冲区初始化时通过DATA确定
    pub frame_data_len: usize,
    frame_idx: &'sto AtomicU8,
    pub pdu_idx: &'sto AtomicU8,
    pub tx_waker: &'sto AtomicWaker,
    pub exit_flag: &'sto AtomicBool,
    //PhantomData<&'sto ()> 是一个零大小类型，用于标记结构体的生命周期。虽然结构体中没有直接持有 'sto 生命周期的具体数据，但通过 PhantomData 可以让编译器进行正确的生命周期检查。
    _lifetime: PhantomData<&'sto ()>,
}

impl<'sto> PduStorageRef<'sto> {
    /// Reset all state ready for a fresh MainDevice or other reuse.
    pub(crate) fn reset(&mut self) {
        // NOTE: Don't reset waker so this `PduStorageRef` can still wake an existing TX/RX handler

        self.frame_idx.store(0, Ordering::Relaxed);
        self.pdu_idx.store(0, Ordering::Relaxed);

        for i in 0..self.num_frames {
            let frame = self.frame_at_index(i);

            unsafe { FrameElement::set_state(frame, FrameState::None) };
        }
    }

    // 从预分配的帧存储池中找到一个可用的帧，并将其标记为"已创建"状态，以便后续用于发送 PDU 数据
    /// Allocate a PDU frame with the given command and data length.
    pub(in crate::pdu_loop) fn alloc_frame(&self) -> Result<CreatedFrame<'sto>, Error> {
        // 这里有数据同步问题？数据处理是否可以在一个线程中顺序执行
        // Find next frame that is not currently in use.
        //
        // Escape hatch: we'll only loop through the frame storage array twice to put an upper
        // bound on the number of times this loop can execute. It could be allowed to execute
        // indefinitely and rely on PDU future timeouts to cancel, but that seems brittle hence
        // this safety check.
        //
        // This can be mitigated by using a `RetryBehaviour` of `Count` or `Forever`.
        for _ in 0..(self.num_frames * 2) {
            let frame_idx = self.frame_idx.fetch_add(1, Ordering::Relaxed) % self.num_frames as u8;

            fmt::trace!("Try to allocate frame {}", frame_idx);

            //通过传入的索引计算对应帧元素的内存地址，并返回一个非空裸指针（高效：操作指针而不是复制）
            // Claim 帧，使其具有唯一的所有者，直到其响应数据被删除。必须在初始化之前声明它，以避免其他线程可能声明同一帧的竞争条件。争用条件通过帧中的原子状态变量和上面的原子索引计数器来缓解。
            // Claim frame so it has a unique owner until its response data is dropped. It must be
            // claimed before initialisation to avoid race conditions with other threads potentially
            // claiming the same frame. The race conditions are mitigated by an atomic state
            // variable in the frame, and the atomic index counter above.
            let frame = self.frame_at_index(usize::from(frame_idx));

            // 获得一个重置过的帧
            let frame =
                CreatedFrame::claim_created(frame, frame_idx, self.pdu_idx, self.frame_data_len);

            if let Ok(f) = frame {
                return Ok(f);
            }
        }

        // We've searched twice and found no free slots. This means the application should
        // either slow down its packet sends, or increase `N` in `PduStorage` as there
        // aren't enough slots to hold all in-flight packets.
        fmt::error!("No available frames in {} slots", self.num_frames);

        Err(PduError::SwapState.into())
    }

    // Sent 状态的帧标记为 RxBusy 状态
    /// Updates state from SENDING -> RX_BUSY
    pub(in crate::pdu_loop) fn claim_receiving(
        &self,
        frame_idx: u8,
    ) -> Option<ReceivingFrame<'sto>> {
        let frame_idx = usize::from(frame_idx);

        if frame_idx >= self.num_frames {
            return None;
        }

        fmt::trace!("--> Claim receiving frame index {}", frame_idx);

        // Sent 状态的帧标记为 RxBusy 状态，返回ReceivingFrame类型对象
        ReceivingFrame::claim_receiving(
            self.frame_at_index(frame_idx),
            self.pdu_idx,
            self.frame_data_len,
        )
    }

    //根据给定的首个 PDU 索引，在存储的帧元素中查找对应的帧
    pub(in crate::pdu_loop) fn frame_index_by_first_pdu_index(
        &self,
        search_pdu_idx: u8,
    ) -> Option<u8> {
        for frame_index in 0..self.num_frames {
            // SAFETY: Frames pointer will always be non-null as it was created by Rust code.
            let frame = unsafe {
                NonNull::new_unchecked(
                    self.frames
                        .as_ptr() //将 NonNull<FrameElement<0>> 类型的 self.frames 转换为原始裸指针
                        .byte_add(frame_index * self.frame_element_stride), //将指针偏移 frame_index * self.frame_element_stride 字节，得到当前帧元素的内存地址
                )
            };

            //检查当前帧的首个 PDU 索引是否与 search_pdu_idx 相等
            if unsafe { FrameElement::<0>::first_pdu_is(frame, search_pdu_idx) } {
                return Some(frame_index as u8);
            }
        }

        None
    }

    //通过传入的索引计算对应帧元素的内存地址，并返回一个非空裸指针（高效：操作指针而不是复制）
    /// Retrieve a frame at the given index.
    ///
    /// If the given index is greater than the value in `PduStorage::N`, this will return garbage
    /// data off the end of the frame element buffer.
    pub(crate) fn frame_at_index(&self, idx: usize) -> NonNull<FrameElement<0>> {
        assert!(idx < self.num_frames); //如果 idx 超出存储的帧元素的数量，程序会触发 panic，避免访问越界内存。

        // SAFETY: `self.frames` was created by Rust, so will always be valid. The index is checked
        // that it doesn't extend past the end of the storage array above, so we should never return
        // garbage data as long as `self.frame_element_stride` is computed correctly.
        unsafe {
            //将计算得到的裸指针封装为 NonNull 类型。new_unchecked 方法不会检查指针是否为空，调用者需要确保传入的指针非空
            NonNull::new_unchecked(
                self.frames
                    .as_ptr() //将 NonNull<FrameElement<0>> 类型的 self.frames 转换为原始裸指针
                    .byte_add(idx * self.frame_element_stride), //将指针偏移 idx * self.frame_element_stride 字节
            )
        }
    }
}

unsafe impl<'sto> Send for PduStorageRef<'sto> {}
unsafe impl<'sto> Sync for PduStorageRef<'sto> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, pdu_loop::pdu_header::PduHeader};
    use core::time::Duration;

    #[test]
    fn zeroed_data() {
        crate::test_logger();

        let storage: PduStorage<1, { PduStorage::element_size(8) }> = PduStorage::new();

        let (_tx, _rx, pdu_loop) = storage.try_split().unwrap();

        let mut frame = pdu_loop.alloc_frame().expect("Allocate first frame");

        frame
            .push_pdu(Command::bwr(0x1000).into(), [0xaa, 0xbb, 0xcc, 0xdd], None)
            .unwrap();

        // Drop frame future to reset its state to `FrameState::None`
        drop(frame.mark_sendable(&pdu_loop, Duration::MAX, usize::MAX));

        let mut frame = pdu_loop.alloc_frame().expect("Allocate second frame");

        const LEN: usize = 8;

        frame.push_pdu(Command::Nop, (), Some(LEN as u16)).unwrap();

        let pdu_start = EthernetFrame::<&[u8]>::header_len()
            + EthercatFrameHeader::header_len()
            + PduHeader::PACKED_LEN;

        let frame = frame.mark_sendable(&pdu_loop, Duration::MAX, usize::MAX);

        // 10 byte PDU header, 8 byte payload, 2 byte WKC
        assert_eq!(
            // Skip all headers
            &frame.buf()[pdu_start..],
            // PDU payload plus working counter
            &[0u8; { LEN + 2 }]
        );
    }

    #[test]
    fn no_spare_frames() {
        crate::test_logger();

        const NUM_FRAMES: usize = 16;
        const DATA: usize = PduStorage::element_size(128);

        let storage: PduStorage<NUM_FRAMES, DATA> = PduStorage::new();
        let s = storage.as_ref();

        for _ in 0..NUM_FRAMES {
            let f = s.alloc_frame().expect("should have free frames");

            // The `CreatedFrame` Drop impl will automatically release the frames for reuse, so we
            // need to forget them to prevent that.
            core::mem::forget(f);
        }

        assert!(
            s.alloc_frame().is_err(),
            "there should be no frame slots available"
        );
    }

    #[test]
    fn reset() {
        crate::test_logger();

        const NUM_FRAMES: usize = 16;
        const DATA: usize = PduStorage::element_size(128);

        let storage: PduStorage<NUM_FRAMES, DATA> = PduStorage::new();
        let mut s = storage.as_ref();

        for _ in 0..NUM_FRAMES {
            let f = s.alloc_frame().expect("should have frame slots");

            core::mem::forget(f);
        }

        // No more frames
        assert!(s.alloc_frame().is_err());

        s.reset();

        // We should be able to allocate every frame again
        for _ in 0..NUM_FRAMES {
            let f = s.alloc_frame().expect("should have frame slots");

            core::mem::forget(f);
        }

        assert!(s.alloc_frame().is_err());
    }
}
