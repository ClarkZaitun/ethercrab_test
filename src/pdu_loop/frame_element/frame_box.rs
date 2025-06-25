use crate::{
    ETHERCAT_ETHERTYPE, MAINDEVICE_ADDR,
    ethernet::{EthernetAddress, EthernetFrame},
    pdu_loop::{
        frame_element::{FrameElement, FrameState},
        frame_header::EthercatFrameHeader,
    },
};
use atomic_waker::AtomicWaker;
use core::{
    fmt::Debug,
    marker::PhantomData,
    ptr::{NonNull, addr_of, addr_of_mut},
    sync::atomic::{AtomicU8, Ordering},
    task::Waker,
};
use ethercrab_wire::EtherCrabWireSized;

use super::FIRST_PDU_EMPTY;

//说明 FrameBox 结构体存储的是所有类型状态下通用的帧数据。类型状态编程是一种编程范式，用于在编译时对不同状态进行建模，保证状态转换的正确性。
/// Frame data common to all typestates.
#[derive(Copy, Clone)]
pub struct FrameBox<'sto> {
    frame: NonNull<FrameElement<0>>, // 完整以太网帧
    pdu_idx: &'sto AtomicU8,         // 数据报索引
    max_len: usize,                  // 帧缓冲区中完整以太网帧的长度
    _lifetime: PhantomData<&'sto mut FrameElement<0>>, //PhantomData 是 Rust 标准库中的一个零大小类型，用于在编译时标记类型的某些属性，而不会在运行时占用额外的内存
                                                       //这里使用 PhantomData<&'sto mut FrameElement<0>> 是为了让编译器知道 FrameBox 结构体在逻辑上拥有一个 'sto 生命周期的 FrameElement<0> 的可变引用，尽管实际上并没有存储该引用。这有助于正确处理生命周期，避免悬垂引用等问题。
}

impl Debug for FrameBox<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let data = self.pdu_buf();

        f.debug_struct("FrameBox")
            .field("state", unsafe {
                &(*addr_of!((*self.frame.as_ptr()).status))
            })
            .field("frame_index", &self.storage_slot_index()) //存储以太网帧索引。与 PDU 头索引字段无关。
            .field("data_hex", &format_args!("{:02x?}", data))
            .finish()
    }
}

impl<'sto> FrameBox<'sto> {
    /// Wrap a [`FrameElement`] pointer in a `FrameBox` without modifying the underlying data.
    pub fn new(
        frame: NonNull<FrameElement<0>>,
        pdu_idx: &'sto AtomicU8,
        max_len: usize,
    ) -> FrameBox<'sto> {
        Self {
            frame,
            max_len,
            pdu_idx,
            _lifetime: PhantomData,
        }
    }

    // 重置以太网和EtherCAT头（可以省略），将以太网帧有效载荷数据清零。
    /// Reset Ethernet and EtherCAT headers, zero out Ethernet frame payload data.
    pub fn init(&mut self) {
        unsafe {
            // 初始化一个新的AtomicWaker
            // addr_of_mut!宏用于安全地获取字段的可变指针
            addr_of_mut!((*self.frame.as_ptr()).waker).write(AtomicWaker::new());

            // 重置first_pdu标记为FIRST_PDU_EMPTY 0xff00
            (*addr_of_mut!((*self.frame.as_ptr()).first_pdu))
                .store(FIRST_PDU_EMPTY, Ordering::Relaxed); //Ordering::Relaxed表示内存操作不需要严格的顺序保证

            // 重置PDU负载长度为0
            addr_of_mut!((*self.frame.as_ptr()).pdu_payload_len).write(0);
        }

        let mut ethernet_frame = self.ethernet_frame_mut();

        // 设置源地址为主设备地址
        ethernet_frame.set_src_addr(MAINDEVICE_ADDR);
        // 设置目的地址为广播地址
        ethernet_frame.set_dst_addr(EthernetAddress::BROADCAST);
        // 设置以太类型为EtherCAT类型
        ethernet_frame.set_ethertype(ETHERCAT_ETHERTYPE);
        // 清空有效载荷数据
        ethernet_frame.payload_mut().fill(0);
    }

    // 原子性地获取并递增数据报index
    pub fn next_pdu_idx(&self) -> u8 {
        self.pdu_idx.fetch_add(1, Ordering::Relaxed)
    }

    // 替换唤醒器
    pub fn replace_waker(&self, waker: &Waker) {
        let ptr = unsafe { &*addr_of!((*self.frame.as_ptr()).waker) };

        ptr.register(waker);
    }

    //尝试唤醒与帧关联的任务
    pub fn wake(&self) -> Result<(), ()> {
        // SAFETY: `self.frame` is a `NonNull`, so `addr_of` will always point to valid data.
        //先获取 waker 字段的引用，再解引用该引用，最终得到 AtomicWaker 的引用
        let waker = unsafe { &*addr_of!((*self.frame.as_ptr()).waker) };

        //调用 AtomicWaker 的 take 方法，尝试获取存储在其中的 Waker
        if let Some(waker) = waker.take() {
            waker.wake(); // 唤醒关联的任务

            Ok(())
        } else {
            Err(())
        }
    }

    pub fn storage_slot_index(&self) -> u8 {
        unsafe { FrameElement::<0>::storage_slot_index(self.frame) }
    }

    // 获取EtherCAT帧头字节切片
    /// Get EtherCAT frame header buffer.
    pub fn ecat_frame_header_mut(&mut self) -> &mut [u8] {
        let ptr = unsafe { FrameElement::<0>::ptr(self.frame) };

        let ethercat_header_start = EthernetFrame::<&[u8]>::header_len(); // 14

        unsafe {
            // 从原始指针创建可变字节切片
            core::slice::from_raw_parts_mut(
                ptr.as_ptr().byte_add(ethercat_header_start),
                EthercatFrameHeader::PACKED_LEN, // 2
            )
        }
    }

    // 获取一个可变的字节切片，该切片指向整个以太网帧中可用于EtherCAT数据报的区域
    /// Get frame payload for writing PDUs into
    pub fn pdu_buf_mut(&mut self) -> &mut [u8] {
        //获取指向 EtherCAT 数据报的指针
        let ptr = unsafe { FrameElement::<0>::ethercat_payload_ptr(self.frame) };

        // 数据报起始字节数为以太网帧头字节数加EtherCAT帧头字节数
        let pdu_payload_start =
            EthernetFrame::<&[u8]>::header_len() + EthercatFrameHeader::header_len();

        //from_raw_parts_mut：这是标准库中的一个函数，用于从原始指针和长度创建一个可变字节切片 &mut [u8]
        unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), self.max_len - pdu_payload_start) }
    }

    // 获取EtherCAT 数据报的字节切片
    /// Get frame payload area. This contains one or more PDUs and is located after the EtherCAT
    /// frame header.
    pub fn pdu_buf(&self) -> &[u8] {
        // 获取指向 EtherCAT 数据报的指针
        let ptr = unsafe { FrameElement::<0>::ethercat_payload_ptr(self.frame) };

        // EtherCAT 数据报在帧中的偏移字节数
        let pdu_payload_start =
            EthernetFrame::<&[u8]>::header_len() + EthercatFrameHeader::header_len();

        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), self.max_len - pdu_payload_start) }
    }

    fn ethernet_frame_mut(&mut self) -> EthernetFrame<&mut [u8]> {
        // SAFETY: We hold a mutable reference to the containing `FrameBox`. A `FrameBox` can only
        // be created from a successful unique acquisition of a frame element.
        unsafe {
            EthernetFrame::new_unchecked(core::slice::from_raw_parts_mut(
                FrameElement::<0>::ptr(self.frame).as_ptr(),
                self.max_len,
            ))
        }
    }

    pub fn ethernet_frame(&self) -> EthernetFrame<&[u8]> {
        unsafe {
            EthernetFrame::new_unchecked(core::slice::from_raw_parts(
                FrameElement::<0>::ptr(self.frame).as_ptr(),
                self.max_len,
            ))
        }
    }

    // 获取用于存储一个或多个 PDU 的帧区域中消耗的字节数
    /// Get the number of bytes consumed in the region of the frame used to store one or more PDUs.
    pub fn pdu_payload_len(&self) -> usize {
        //使用 addr_of! 宏安全地获取 pdu_payload_len 字段的指针
        unsafe { *addr_of!((*self.frame.as_ptr()).pdu_payload_len) }
    }

    // 设置帧状态
    // 当前 set_state 方法的返回值类型为 ()，也就是无返回值，目前写法是省略返回值的形式
    pub fn set_state(&self, to: FrameState) {
        unsafe { FrameElement::set_state(self.frame, to) };
    }

    pub fn swap_state(&self, from: FrameState, to: FrameState) -> Result<(), FrameState> {
        unsafe { FrameElement::swap_state(self.frame, from, to) }.map(|_| ())
    }

    pub fn clear_first_pdu(&self) {
        unsafe {
            FrameElement::<0>::clear_first_pdu(self.frame);
        }
    }

    //更新EtherCAT帧数据区已存在的数据报的总长度，原子地设置（如果还没设置）帧中第一个数据报的index
    /// Add the given number of bytes in `alloc_size` to the consumed bytes counter in the frame.
    ///
    /// Also sets the first PDU index if it hasn't already been set.
    pub fn add_pdu(&mut self, alloc_size: usize, pdu_idx: u8) {
        //更新EtherCAT帧数据区已存在的数据报的总长度，增加新增的数据报长度
        unsafe { *addr_of_mut!((*self.frame.as_ptr()).pdu_payload_len) += alloc_size };

        //原子地设置（如果还没设置）帧中第一个数据报的index
        unsafe { FrameElement::<0>::set_first_pdu(self.frame, pdu_idx) };
    }
}
