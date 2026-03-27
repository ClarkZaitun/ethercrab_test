//! A group of SubDevices.
//!
//! SubDevices can be divided into multiple groups to allow multiple tasks to run concurrently,
//! potentially at different tick rates.

mod group_id;
mod handle;
mod tx_rx_response;

use crate::{
    DcSync,
    MainDevice,
    RegisterAddress,
    SubDeviceState,
    al_control::AlControl,
    command::Command,
    error::{DistributedClockError, Error, Item},
    fmt,
    // lending_lock::LendingLock,
    pdi::PdiOffset,
    pdu_loop::{CreatedFrame, ReceivedPdu},
    subdevice::{
        IoRanges, SubDevice, SubDeviceRef, configuration::PdoDirection, pdi::SubDevicePdi,
    },
    timer_factory::IntoTimeout,
};
use core::{cell::UnsafeCell, marker::PhantomData, sync::atomic::AtomicUsize, time::Duration};
use ethercrab_wire::{EtherCrabWireRead, EtherCrabWireSized};
use lock_api::{RawRwLock, RwLock, RwLockWriteGuard};

pub use self::group_id::GroupId;
pub use self::handle::SubDeviceGroupHandle;
pub use self::tx_rx_response::TxRxResponse;

static GROUP_ID: AtomicUsize = AtomicUsize::new(0);

/// The size of a DC sync PDU.
// 时间同步帧的数据报长度
const DC_PDU_SIZE: usize = CreatedFrame::PDU_OVERHEAD_BYTES + u64::PACKED_LEN;

// MSRV: Remove when core SyncUnsafeCell is stabilised
// MySyncUnsafeCell封装标准库中的 UnsafeCell
#[derive(Debug)]
pub(crate) struct MySyncUnsafeCell<T: ?Sized>(pub UnsafeCell<T>);
// ?Sized 是一个 trait bound，它表示 T 可以是不定大小类型（unsized type），像切片 [T]、trait 对象 dyn Trait 这类在编译时大小不确定的类型也能作为 T 的具体类型
// UnsafeCell<T> 是 Rust 标准库中的类型，它提供了内部可变性，即允许通过共享引用修改其内部数据

impl<T> MySyncUnsafeCell<T> {
    // 构造函数
    pub fn new(inner: T) -> Self {
        Self(UnsafeCell::new(inner))
    }
}

unsafe impl<T: ?Sized + Sync> Sync for MySyncUnsafeCell<T> {}

impl<T: ?Sized> MySyncUnsafeCell<T> {
    /// Gets a mutable pointer to the wrapped value.
    ///
    /// This can be cast to a pointer of any kind.
    /// Ensure that the access is unique (no active references, mutable or not)
    /// when casting to `&mut T`, and ensure that there are no mutations
    /// or mutable aliases going on when casting to `&T`
    // 获取指向封装值的可变指针
    // 这可以转换为任何类型的指针：
    // 转换为 `&mut T` 时，确保访问唯一（无活动引用，无论是否可变）。
    // 转换为 `&T` 时，确保没有发生任何突变或可变别名
    #[inline]
    // 返回一个指向包装值的可变原始指针 *mut T
    // 当将该指针转换为 &mut T 时，要确保访问是唯一的（没有其他活跃的引用）；转换为 &T 时，要确保没有突变或可变别名。
    pub const fn get(&self) -> *mut T {
        self.0.get()
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// This call borrows the `SyncUnsafeCell` mutably (at compile-time) which
    /// guarantees that we possess the only reference.
    #[inline]
    // 接收 self 的可变引用，返回一个指向底层数据的可变引用 &mut T
    // 由于函数参数是 &mut self，Rust 编译器会保证此时只有一个引用，避免数据竞争。
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

// 用一系列常量泛型参数来表示 SubDeviceGroup 的EtherCAT状态
// 每个状态都是一个结构体，没有字段，只是一个标记

/// A typestate for [`SubDeviceGroup`] representing a group that is shut down.
///
/// This corresponds to the EtherCAT states INIT.
#[derive(Copy, Clone, Debug)]
pub struct Init;

/// A typestate for [`SubDeviceGroup`] representing a group that is undergoing initialisation.
///
/// This corresponds to the EtherCAT states INIT and PRE-OP.
#[derive(Copy, Clone, Debug)]
pub struct PreOp;

/// The same as [`PreOp`] but with access to PDI methods. All SubDevice configuration should be complete
/// at this point.
#[derive(Copy, Clone, Debug)]
pub struct PreOpPdi;

/// A typestate for [`SubDeviceGroup`] representing a group that is in SAFE-OP.
#[derive(Copy, Clone, Debug)]
pub struct SafeOp;

/// A typestate for [`SubDeviceGroup`] representing a group that is in OP.
#[derive(Copy, Clone, Debug)]
pub struct Op;

// 表示从站组是否配置了分布式时钟（DC）
// 如果配置了 DC，那么从站组的状态就会是 HasDc
// 如果没有配置 DC，那么从站组的状态就会是 NoDc

/// A typestate for [`SubDeviceGroup`]s that do not have a Distributed Clock configuration
#[derive(Copy, Clone, Debug)]
pub struct NoDc;

/// A typestate for [`SubDeviceGroup`]s that have a configured Distributed Clock.
///
/// This typestate can be entered by calling [`SubDeviceGroup::configure_dc_sync`].
#[derive(Copy, Clone, Debug)]
pub struct HasDc {
    // SYNC0周期时间
    sync0_period: u64,
    // SYNC0偏移时间
    sync0_shift: u64,
    /// Configured address of the DC reference SubDevice.
    // 参考时钟配置地址
    reference: u16,
}

// 表示从站组是否配置了过程数据映像（PDI）
/// Marker trait for `SubDeviceGroup` typestates where all SubDevices have a PDI.
#[doc(hidden)]
pub trait HasPdi {}

impl HasPdi for PreOpPdi {}
impl HasPdi for SafeOp {}
impl HasPdi for Op {}

// 表示从站组是否在 PRE-OP 状态
#[doc(hidden)]
pub trait IsPreOp {}

impl IsPreOp for PreOp {}
impl IsPreOp for PreOpPdi {}

// 保存组的从站数组和PDI起始偏移量
#[derive(Default)]
struct GroupInner<const MAX_SUBDEVICES: usize> {
    subdevices: heapless::Vec<SubDevice, MAX_SUBDEVICES>, // 当前组的从站数组
    pdi_start: PdiOffset, // 当前组的 PDI 起始偏移量，也是逻辑地址。逻辑地址从0开始
}

const CYCLIC_OP_ENABLE: u8 = 0b0000_0001;
const SYNC0_ACTIVATE: u8 = 0b0000_0010;
const SYNC1_ACTIVATE: u8 = 0b0000_0100;

/// Group distributed clock configuration.
#[derive(Default, Debug, Copy, Clone)]
pub struct DcConfiguration {
    /// How long the SubDevices in the group should wait before starting SYNC0 pulse generation.
    // 0x990延迟时间
    pub start_delay: Duration,

    /// SYNC0 cycle time.
    ///
    /// SubDevices with an `AssignActivate` value of `0x0300` in their ESI definition should set
    /// this value.
    // 0x9A0
    pub sync0_period: Duration,

    /// Shift time relative to SYNC0 pulse.
    // 偏移时间
    pub sync0_shift: Duration,
}

// 用于在DC模式下，每周期返回发帧时间信息，包括DC系统时间、下一个周期的等待时间、当前周期的偏移时间
// 方便应用层确认下周期的时间点
/// Information useful to a process data cycle.
#[derive(Debug, Copy, Clone)]
pub struct CycleInfo {
    /// Distributed Clock System time in nanoseconds.
    pub dc_system_time: u64,

    /// The time to wait before starting the next process data cycle.
    ///
    /// This duration is calculated based on the [`sync0_period`](DcConfiguration::sync0_period) and
    /// [`sync0_shift`](DcConfiguration::sync0_shift) passed into [`SubDeviceGroup::configure_dc_sync`]
    /// and is meant to be used to accurately synchronise the MainDevice process data cycle with the
    /// DC system time.
    pub next_cycle_wait: Duration,

    /// The difference between the SYNC0 pulse and when the current cycle's data was received by the
    /// DC reference SubDevice.
    pub cycle_start_offset: Duration,
}

// PDO域？
// 这个库只允许通过组来访问从站PDO
/// A group of one or more EtherCAT SubDevices.
///
/// Groups are created during EtherCrab initialisation, and are the only way to access individual
/// SubDevice PDI sections.
#[doc(alias = "SlaveGroup")] // 为文档添加别名 SlaveGroup，这意味着在文档搜索时，输入 SlaveGroup 也能找到 SubDeviceGroup 的相关信息
pub struct SubDeviceGroup<
    const MAX_SUBDEVICES: usize,
    const MAX_PDI: usize,
    R: RawRwLock = crate::DefaultLock, // 可选锁类型，默认为 DefaultLock
    S = PreOp,                         // S = PreOp：一个泛型类型参数，默认值为 PreOp；
    DC = NoDc,                         // DC = NoDc：一个泛型类型参数，默认值为 NoDc
> {
    // 组ID
    // 从0开始，每个新增的组ID会加一
    id: GroupId,
    // 过程数据映像（PDI）数据区：PDI数据区开头部分为从站输入数据；剩余字部分为输出数据
    // 读写锁。spin::rwlock::RwLock 是一个自旋锁，它允许并发的读操作和独占的写操作
    // MySyncUnsafeCell<[u8; MAX_PDI]> 是锁保护的数据，MySyncUnsafeCell 是自定义的类型，用于包装 UnsafeCell 并实现 Sync trait
    // crate::SpinStrategy 是自旋锁的自旋策略
    pdi: RwLock<R, MySyncUnsafeCell<[u8; MAX_PDI]>>,
    /// The number of bytes at the beginning of the PDI reserved for SubDevice inputs.
    //  PDI 数据区输入数据实际总字节数
    read_pdi_len: usize,
    /// The total length (I and O) of the PDI for this group.
    // PDI数据区输入输出数据实际总字节数。需要小于等于 MAX_PDI
    pdi_len: usize,
    // inner 保存了从站数组和PDI数据区的起始地址
    inner: MySyncUnsafeCell<GroupInner<MAX_SUBDEVICES>>,
    dc_conf: DC,            // 标记类型，具体类型取决于结构体实例化时传入的类型
    _state: PhantomData<S>, // PhantomData 是一个零大小类型，不占用实际内存空间，仅用于类型标记
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, PreOp, DC>
{
    // 配置TxPDO和RxPDO的FMMU，并更新 SubDeviceGroup 中 GroupInner 每个从站的 PDI 偏移
    /// Configure read/write FMMUs and PDI for this group.
    async fn configure_fmmus(&mut self, maindevice: &MainDevice<'_>) -> Result<(), Error> {
        let inner = self.inner.get_mut();

        // 当前组的 PDI 起始偏移量。也就是第一个从站PDO在PDI中的起始地址
        // 在本函数内会随着遍历从站不断累加
        let mut pdi_position = inner.pdi_start;

        fmt::debug!(
            "Going to configure group with {} SubDevice(s), starting PDI offset {:#010x}",
            inner.subdevices.len(),
            inner.pdi_start.start_address
        );

        // Configure master read PDI mappings in the first section of the PDI
        // 在 PDI 的第一部分配置 从站输入 PDI 映射
        for subdevice in inner.subdevices.iter_mut() {
            // We're in PRE-OP at this point
            // 刷新PDI未分配给从站PDO的地址，得到下一个从站PDO的起始地址
            pdi_position = SubDeviceRef::new(maindevice, subdevice.configured_address(), subdevice)
                // 读取从站PDO配置后，设置输入FMMU，得到从站PDO在PDI中的地址范围
                .configure_fmmus(
                    pdi_position,                  // 当前从站PDO在PDI中的起始地址，也就是逻辑地址
                    inner.pdi_start.start_address, // 组的PDI起始地址
                    PdoDirection::MasterRead,
                )
                .await?;
        }

        // 计算当前组的PDI数据区输入数据实际总字节数
        self.read_pdi_len = (pdi_position.start_address - inner.pdi_start.start_address) as usize;

        fmt::debug!("SubDevice mailboxes configured and init hooks called");

        // We configured all read PDI mappings as a contiguous block in the previous loop. Now we'll
        // configure the write mappings in a separate loop. This means we have IIIIOOOO instead of
        // IOIOIO.
        // 我们在上一个循环中将所有读取 PDI 映射配置为一个连续块。现在我们将在一个单独的循环中配置写入映射。这意味着我们将使用 IIIIOOOO 而不是 IOIOIO。
        // 在 PDI 的第二部分配置 从站输出 PDI 映射
        for subdevice in inner.subdevices.iter_mut() {
            let addr = subdevice.configured_address();

            let mut subdevice_config = SubDeviceRef::new(maindevice, addr, subdevice);

            // Still in PRE-OP
            pdi_position = subdevice_config
                // 读取从站PDO配置后，设置输输出FMMU，得到从站PDO在PDI中的地址范围
                .configure_fmmus(
                    pdi_position,                  // 当前从站PDO在PDI中的起始地址，也就是逻辑地址
                    inner.pdi_start.start_address, // 组的PDI起始地址
                    PdoDirection::MasterWrite,
                )
                .await?;
        }

        fmt::debug!("SubDevice FMMUs configured for group. Able to move to SAFE-OP");

        // 计算当前组的PDI数据区实际总字节数
        self.pdi_len = (pdi_position.start_address - inner.pdi_start.start_address) as usize;

        fmt::debug!(
            "Group PDI length: start {:#010x}, {} total bytes ({} input bytes)",
            inner.pdi_start.start_address,
            self.pdi_len,
            self.read_pdi_len
        );

        // 检查PDI长度是否超过最大长度，超过则返回错误
        if self.pdi_len > MAX_PDI {
            return Err(Error::PdiTooLong {
                max_length: MAX_PDI,
                desired_length: self.pdi_len,
            });
        }

        Ok(())
    }

    // 根据从站在组中的索引获取从站引用
    /// Borrow an individual SubDevice.
    #[deny(clippy::panic)]
    #[doc(alias = "slave")]
    pub fn subdevice<'maindevice, 'group>(
        &'group self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        index: usize,
    ) -> Result<SubDeviceRef<'maindevice, &'group SubDevice>, Error> {
        let subdevice = self.inner().subdevices.get(index).ok_or(Error::NotFound {
            item: Item::SubDevice,
            index: Some(index),
        })?;

        Ok(SubDeviceRef::new(
            maindevice,
            subdevice.configured_address(),
            subdevice,
        ))
    }

    // 从pre op切换到op，没有设置SYNC。在此之前需要配置PDO
    /// Transition the group from PRE-OP -> SAFE-OP -> OP.
    ///
    /// To transition individually from PRE-OP to SAFE-OP, then SAFE-OP to OP, see
    /// [`SubDeviceGroup::into_safe_op`].
    pub async fn into_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>, Error> {
        // SYNC的设置在用户程序中，不在库中
        // PDO的配置在用户程序中，不在库中
        // 切换到 Safe Op时会配置FMMU，同时会设置PDI
        let self_ = self.into_safe_op(maindevice).await?;

        self_.into_op(maindevice).await
    }

    // 配置FMMU，同时会设置PDI
    /// Configure FMMUs, but leave the group in [`PreOp`] state.
    ///
    /// This method is used to obtain access to the group's PDI and related functionality. All SDO
    /// and other configuration should be complete at this point otherwise issues with cyclic data
    /// may occur (e.g. incorrect lengths, misplaced fields, etc).
    pub async fn into_pre_op_pdi(
        mut self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, PreOpPdi, DC>, Error> {
        // 配置TxPDO和RxPDO的FMMU
        self.configure_fmmus(maindevice).await?;

        Ok(SubDeviceGroup {
            id: self.id,
            pdi: self.pdi,
            read_pdi_len: self.read_pdi_len,
            pdi_len: self.pdi_len,
            inner: self.inner,
            dc_conf: self.dc_conf,
            _state: PhantomData,
        })
    }

    /// Transition the SubDevice group from PRE-OP to SAFE-OP.
    pub async fn into_safe_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, SafeOp, DC>, Error> {
        // 设置FMMU
        let self_ = self.into_pre_op_pdi(maindevice).await?;

        // We're done configuring FMMUs, etc, now we can request all SubDevices in this group go into
        // SAFE-OP
        // 请求切换到safe op
        self_
            .transition_to(maindevice, SubDeviceState::SafeOp)
            .await
    }

    /// Transition all SubDevices in the group from PRE-OP to INIT.
    pub async fn into_init(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Init, DC>, Error> {
        self.transition_to(maindevice, SubDeviceState::Init).await
    }

    // 获取本组的所有从站引用的迭代器
    /// Get an iterator over all SubDevices in this group.
    pub fn iter<'group, 'maindevice>(
        &'group self,
        maindevice: &'maindevice MainDevice<'maindevice>,
    ) -> impl Iterator<Item = SubDeviceRef<'maindevice, &'group SubDevice>> {
        self.inner()
            .subdevices
            .iter()
            .map(|sd| SubDeviceRef::new(maindevice, sd.configured_address, sd))
    }

    // 获取本组的所有从站引用的可变迭代器
    /// Get a mutable iterator over all SubDevices in this group
    pub fn iter_mut<'group, 'maindevice>(
        &'group mut self,
        maindevice: &'maindevice MainDevice<'maindevice>,
    ) -> impl Iterator<Item = SubDeviceRef<'maindevice, &'group mut SubDevice>> {
        self.inner
            .get_mut()
            .subdevices
            .iter_mut()
            .map(|sd| SubDeviceRef::new(maindevice, sd.configured_address, sd))
    }
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, S, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, S, DC>
where
    S: IsPreOp,
{
    // 完整的DC配置
    /// Configure Distributed Clock SYNC0 for all SubDevices in this group.
    ///
    /// All configured times in the [`DcConfiguration`] struct must be under `u32::MAX` nanoseconds.
    /// This means that e.g. the sync start delay must not be greater than rougly 4.2 seconds.
    ///
    /// # Errors
    ///
    /// This method will return with a
    /// [`Error::DistributedClock(DistributedClockError::NoReference)`](Error::DistributedClock)
    /// error if no DC reference SubDevice is present on the network.
    ///
    /// This method will also return an error if any of the [`DcConfiguration`] struct's fields hold
    /// a value greater than `u32::MAX` nanoseconds.
    pub async fn configure_dc_sync(
        self,
        maindevice: &MainDevice<'_>,
        dc_conf: DcConfiguration,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, PreOpPdi, HasDc>, Error> {
        fmt::debug!("Configuring distributed clocks for group");

        let Some(reference) = maindevice.dc_ref_address() else {
            fmt::error!("No DC reference clock SubDevice present, unable to configure DC");

            return Err(DistributedClockError::NoReference.into());
        };

        let DcConfiguration {
            start_delay,
            sync0_period,
            sync0_shift,
        } = dc_conf;

        // Coerce generics into concrete `PreOp` type as we don't need the PDI to configure the DC.
        let self_ = SubDeviceGroup {
            id: self.id,
            pdi: self.pdi,
            read_pdi_len: self.read_pdi_len,
            pdi_len: self.pdi_len,
            inner: self.inner,
            dc_conf: NoDc,
            _state: PhantomData::<PreOp>,
        };

        // 只配置支持DC的从站
        // Only configure DC for those devices that want and support it
        let dc_devices = self_.iter(maindevice).filter(|subdevice| {
            subdevice.dc_support().any() && !matches!(subdevice.dc_sync(), DcSync::Disabled)
        });

        // 读取 0x910 系统时间
        let system_time = SubDeviceRef::new(maindevice, reference, ())
            .register_read::<u64>(RegisterAddress::DcSystemTime)
            .await?;

        // Kinda weird converting to/from u32 but these values must not exceed u32::MAX
        // 与 u32 格式转换有点奇怪，但这些值不能超过 u32::MAX。
        // TODO
        let sync0_period = u64::from(u32::try_from(sync0_period.as_nanos())?);

        let first_pulse_delay = u64::from(u32::try_from(start_delay.as_nanos())?);

        for subdevice in dc_devices {
            fmt::debug!(
                "--> Configuring SubDevice {:#06x} {} DC mode {}",
                subdevice.configured_address(),
                subdevice.name(),
                subdevice.dc_sync()
            );

            // FPWR 0x981 禁用SYNC0
            // Disable cyclic op, ignore WKC
            subdevice
                .write(RegisterAddress::DcSyncActive)
                .ignore_wkc()
                .send(maindevice, 0u8)
                .await?;

            // FPWR 0x980
            // Write access to EtherCAT
            subdevice
                .write(RegisterAddress::DcCyclicUnitControl)
                .send(maindevice, 0u8)
                .await?;

            // 计算出 0x990 同步起始时间
            // 从站当前时间 + 首次脉冲延迟，然后四舍五入到一个完整的周期
            // Round first pulse time to a whole number of cycles
            let start_time = (system_time + first_pulse_delay) / sync0_period * sync0_period;

            fmt::debug!("--> Computed DC sync start time: {}", start_time);

            // FPWR 0x990 同步起始时间
            subdevice
                .write(RegisterAddress::DcSyncStartTime)
                .send(maindevice, start_time)
                .await?;

            // FPWR 0x9A0 周期时间
            // Cycle time in nanoseconds
            subdevice
                .write(RegisterAddress::DcSync0CycleTime)
                .send(maindevice, sync0_period)
                .await?;

            // 如果使用 SYNC1 ，还需要配置 SYNC1 周期时间
            let flags = if let DcSync::Sync01 { sync1_period } = subdevice.dc_sync() {
                let sync1_period = u64::from(u32::try_from(sync1_period.as_nanos())?);

                subdevice
                    .write(RegisterAddress::DcSync1CycleTime)
                    .send(maindevice, sync1_period)
                    .await?;

                SYNC1_ACTIVATE | SYNC0_ACTIVATE | CYCLIC_OP_ENABLE
            } else {
                SYNC0_ACTIVATE | CYCLIC_OP_ENABLE
            };

            // 激活 SYNC0 和 SYNC1
            subdevice
                .write(RegisterAddress::DcSyncActive)
                .send(maindevice, flags)
                .await?;
        }

        Ok(SubDeviceGroup {
            id: self_.id,
            pdi: self_.pdi,
            read_pdi_len: self_.read_pdi_len,
            pdi_len: self_.pdi_len,
            inner: self_.inner,
            dc_conf: HasDc {
                sync0_period: sync0_period,
                sync0_shift: sync0_shift.as_nanos() as u64,
                reference,
            },
            _state: PhantomData,
        })
    }
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, PreOpPdi, DC>
{
    /// Transition the SubDevice group from PRE-OP to SAFE-OP.
    pub async fn into_safe_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, SafeOp, DC>, Error> {
        self.transition_to(maindevice, SubDeviceState::SafeOp).await
    }

    /// Transition all SubDevices in the group from PRE-OP to SAFE-OP, then to OP.
    ///
    /// This is a convenience method that calls [`into_safe_op`](SubDeviceGroup::into_safe_op) then
    /// [`into_op`](SubDeviceGroup::into_op).
    pub async fn into_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>, Error> {
        let self_ = self.into_safe_op(maindevice).await?;

        self_.transition_to(maindevice, SubDeviceState::Op).await
    }

    /// Like [`into_op`](SubDeviceGroup::into_op), however does not wait for all SubDevices to enter
    /// OP state.
    ///
    /// This allows the application process data loop to be started, so as to e.g. not time out
    /// watchdogs, or provide valid data to prevent DC sync errors.
    ///
    /// The group's state can be checked by testing the result of a `tx_rx_*` call using methods on
    /// the [`TxRxResponse`] struct.
    pub async fn request_into_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>, Error> {
        let self_ = self.into_safe_op(maindevice).await?;

        self_.request_into_op(maindevice).await
    }

    /// Transition all SubDevices in the group from PRE-OP to INIT.
    pub async fn into_init(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Init, DC>, Error> {
        self.transition_to(maindevice, SubDeviceState::Init).await
    }
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, SafeOp, DC>
{
    /// Transition all SubDevices in the group from SAFE-OP to OP.
    pub async fn into_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>, Error> {
        // 请求切换到op状态
        self.transition_to(maindevice, SubDeviceState::Op).await
    }

    /// Transition all SubDevices in the group from SAFE-OP to PRE-OP.
    pub async fn into_pre_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, PreOp, DC>, Error> {
        self.transition_to(maindevice, SubDeviceState::PreOp).await
    }

    /// Like [`into_op`](SubDeviceGroup::into_op), however does not wait for all SubDevices to enter OP
    /// state.
    ///
    /// This allows the application process data loop to be started, so as to e.g. not time out
    /// watchdogs, or provide valid data to prevent DC sync errors.
    ///
    /// The group's state can be checked by testing the result of a `tx_rx_*` call using methods on
    /// the [`TxRxResponse`] struct.
    pub async fn request_into_op(
        mut self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>, Error> {
        for subdevice in self.inner.get_mut().subdevices.iter_mut() {
            SubDeviceRef::new(maindevice, subdevice.configured_address(), subdevice)
                .request_subdevice_state_nowait(SubDeviceState::Op)
                .await?;
        }

        Ok(SubDeviceGroup {
            id: self.id,
            pdi: self.pdi,
            read_pdi_len: self.read_pdi_len,
            pdi_len: self.pdi_len,
            inner: self.inner,
            dc_conf: self.dc_conf,
            _state: PhantomData,
        })
    }
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, Op, DC>
{
    /// Transition all SubDevices in the group from OP to SAFE-OP.
    pub async fn into_safe_op(
        self,
        maindevice: &MainDevice<'_>,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, SafeOp, DC>, Error> {
        self.transition_to(maindevice, SubDeviceState::SafeOp).await
    }
}

impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, S> Default
    for SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, S>
{
    fn default() -> Self {
        Self {
            // ID从全局原子变量0开始加一
            id: GroupId(GROUP_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)),
            pdi: RwLock::new(MySyncUnsafeCell::new([0u8; MAX_PDI])),
            read_pdi_len: Default::default(), // 0
            pdi_len: Default::default(),      // 0
            inner: MySyncUnsafeCell::new(GroupInner::default()),
            dc_conf: NoDc,
            _state: PhantomData,
        }
    }
}

// 通用实现，没有DC和状态的限制
impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, S, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, S, DC>
{
    fn inner(&self) -> &GroupInner<MAX_SUBDEVICES> {
        unsafe { &*self.inner.get() }
    }

    /// Get the number of SubDevices in this group.
    pub fn len(&self) -> usize {
        self.inner().subdevices.len()
    }

    /// Check whether this SubDevice group is empty or not.
    pub fn is_empty(&self) -> bool {
        self.inner().subdevices.is_empty()
    }

    // 检查组里的所有从站是否在预期状态。只检查一次，如果失败返回false
    /// Check if all SubDevices in the group are the given desired state.
    async fn is_state(
        &self,
        maindevice: &MainDevice<'_>,
        desired_state: SubDeviceState,
    ) -> Result<bool, Error> {
        fmt::trace!("Check group state");

        let mut subdevices = self.inner().subdevices.iter();

        let mut total_checks = 0;

        // Send as many frames as required to check statuses of all subdevices
        // 发一个帧，等待返回后才会发下一个帧
        // TODO：应该是所有从站的数据发送后，再统一检查
        loop {
            // 从预分配的帧存储池中找到一个可用的帧，并将其标记为"已创建"状态，以便后续用于发送 PDU 数据
            let mut frame = maindevice.pdu_loop.alloc_frame()?;

            // 在帧里插入检查状态的数据报，返回剩余未检查状态的从站数组和已检查的从站数
            let (rest, num_in_this_frame) = push_state_checks(subdevices, &mut frame)?;

            // 刷新待检查状态的从站数组
            subdevices = rest;

            // Nothing to send, we've checked all SDs
            if num_in_this_frame == 0 {
                fmt::trace!("--> No more state checks, pushed {}", total_checks);

                break;
            }

            total_checks += num_in_this_frame;

            // 帧设置为可发送状态Sendable，返回一个 Future，当收到对已发送帧的响应时，该 Future 将被执行。
            // TODO：重试次数
            let frame = frame.mark_sendable(
                &maindevice.pdu_loop,
                maindevice.timeouts.pdu(),
                maindevice.config.retry_behaviour.retry_count(),
            );

            // 唤醒Tx任务
            maindevice.pdu_loop.wake_sender();

            // 帧返回
            let received = frame.await?;

            // 转换为数据报迭代器
            for pdu in received.into_pdu_iter() {
                let pdu = pdu?;

                // 解析出0x0130寄存器值
                let result = AlControl::unpack_from_slice(&pdu)?;
                // TODO 在这里可以检查 result.error是否为0，非0则表示从站有错误

                // Return from this fn as soon as the first undesired state is found
                if result.state != desired_state {
                    return Ok(false);
                }
            }
        }

        // Just sanity checking myself
        debug_assert_eq!(total_checks, self.len());

        Ok(true)
    }

    // 持续检查从站是否在预期状态，要么超时，要么报错
    /// Wait for all SubDevices in this group to transition to the given state.
    async fn wait_for_state(
        &self,
        maindevice: &MainDevice<'_>,
        desired_state: SubDeviceState,
    ) -> Result<(), Error> {
        async {
            loop {
                // 检查组里的所有从站是否在预期状态。只检查一次，如果失败返回false
                if self.is_state(maindevice, desired_state).await? {
                    break Ok(());
                }

                // 等待一段时间，进入下一次循环
                maindevice.timeouts.loop_tick().await;
            }
        }
        // 将原始 Future (self) 包装成一个带超时 5000 ms的 TimeoutFuture
        .timeout(maindevice.timeouts.state_transition())
        .await
    }

    // 切换状态到指定状态
    /// Transition to a new state.
    async fn transition_to<TO>(
        mut self,
        maindevice: &MainDevice<'_>,
        desired_state: SubDeviceState,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, TO, DC>, Error> {
        // We're done configuring FMMUs, etc, now we can request all SubDevices in this group go into
        // SAFE-OP
        for subdevice in self.inner.get_mut().subdevices.iter_mut() {
            SubDeviceRef::new(maindevice, subdevice.configured_address(), subdevice)
                .request_subdevice_state_nowait(desired_state) // 请求从站状态切换，如果有故障读取故障码并打印
                .await?;
        }

        fmt::debug!("Waiting for group state {}", desired_state);

        // 持续检查从站是否在预期状态，要么超时，要么报错
        self.wait_for_state(maindevice, desired_state).await?;

        fmt::debug!("--> Group reached state {}", desired_state);

        Ok(SubDeviceGroup {
            id: self.id,
            pdi: self.pdi,
            read_pdi_len: self.read_pdi_len,
            pdi_len: self.pdi_len,
            inner: self.inner,
            dc_conf: self.dc_conf,
            _state: PhantomData,
        })
    }
}

// 在帧里插入检查状态的数据报，返回剩余未检查状态的从站数组和已检查的从站数
fn push_state_checks<'group, 'sto, I>(
    mut subdevices: I,
    frame: &mut CreatedFrame<'sto>,
) -> Result<(I, usize), Error>
where
    I: Iterator<Item = &'group SubDevice>,
{
    let mut num_in_this_frame = 0;

    // 检查帧剩余空间是否能插入2字节（0x0130寄存器长度）的数据
    while frame.can_push_pdu_payload(AlControl::PACKED_LEN) {
        // 从从站数组中获取一个从站
        let Some(sd) = subdevices.next() else {
            break;
        };

        // A too-long error here should be unreachable as we check if the payload can be
        // pushed in the loop condition.
        // 插入FPRD 0x0130
        frame.push_pdu(
            Command::fprd(sd.configured_address(), RegisterAddress::AlStatus.into()).into(),
            (),
            Some(AlControl::PACKED_LEN as u16),
        )?;

        num_in_this_frame += 1;

        // A status check datagram is 14 bytes, meaning we can fit at most just over 100
        // checks per normal EtherCAT frame. This leaves spare PDU indices available for
        // other purposes, however if the user is using jumbo frames or something, we should
        // always leave some indices free for e.g. other threads.
        if num_in_this_frame > 128 {
            break;
        }
    }

    fmt::trace!(
        "--> Pushed {} status checks into frame {}",
        num_in_this_frame,
        frame.storage_slot_index()
    );

    Ok((subdevices, num_in_this_frame))
}

// Methods for any state where a PDI has been configured.
impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, S, DC>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, S, DC>
where
    S: HasPdi,
{
    // 用户程序中可以通过索引获取从站引用，然后读写PDO
    /// Borrow an individual SubDevice.
    #[doc(alias = "slave")]
    pub fn subdevice<'maindevice, 'group>(
        &'group self,
        maindevice: &'maindevice MainDevice<'maindevice>,
        index: usize,
    ) -> Result<SubDeviceRef<'maindevice, SubDevicePdi<'group, MAX_PDI, R>>, Error> {
        let subdevice = self.inner().subdevices.get(index).ok_or(Error::NotFound {
            item: Item::SubDevice,
            index: Some(index),
        })?;

        let io_ranges = subdevice.io_segments().clone();

        // 获取的变量只用于打印
        let IoRanges {
            input: input_range,
            output: output_range,
        } = &io_ranges;

        fmt::trace!(
            "Get SubDevice {:#06x} IO ranges I: {}, O: {} (group PDI {} byte subset of {} byte max)",
            subdevice.configured_address(),
            input_range,
            output_range,
            self.pdi_len,
            MAX_PDI
        );

        Ok(SubDeviceRef::new(
            maindevice,
            subdevice.configured_address(),
            // TODO 这里可以传入 io_ranges ，限定PDI的范围，只给当前从站使用
            SubDevicePdi::new(subdevice, &self.pdi),
        ))
    }

    /// Get an iterator over all SubDevices in this group.
    pub fn iter<'group, 'maindevice>(
        &'group self,
        maindevice: &'maindevice MainDevice<'maindevice>,
    ) -> impl Iterator<Item = SubDeviceRef<'group, SubDevicePdi<'group, MAX_PDI, R>>>
    where
        'maindevice: 'group,
    {
        self.inner().subdevices.iter().map(|sd| {
            SubDeviceRef::new(
                maindevice,
                sd.configured_address,
                SubDevicePdi::new(sd, &self.pdi),
            )
        })
    }

    /// Drive the SubDevice group's inputs and outputs.
    ///
    /// A `SubDeviceGroup` will not process any inputs or outputs unless this method is called
    /// periodically. It will send an `LRW` to update SubDevice outputs and read SubDevice inputs.
    ///
    /// This method returns a [`TxRxResponse`] containing the working counter and a list of all
    /// SubDevice states on success.
    ///
    /// # Errors
    ///
    /// This method will return with an error if the PDU could not be sent over the network, or the
    /// response times out.
    pub async fn tx_rx<'sto>(
        &self,
        maindevice: &'sto MainDevice<'sto>,
    ) -> Result<TxRxResponse<MAX_SUBDEVICES>, Error> {
        fmt::trace!(
            "Group TX/RX, start address {:#010x}, data len {}, of which read bytes: {}",
            self.inner().pdi_start.start_address,
            self.pdi_len,
            self.read_pdi_len
        );

        // 阻塞当前线程直到获取独占写权限（同一时间只能有一个写者，或多个读者）。
        let mut pdi_lock = self.pdi.write();

        let mut total_bytes_sent = 0;
        // LRW命令的WKC总和
        let mut lrw_wkc_sum = 0;

        let mut subdevices = self.inner().subdevices.iter();
        // 已检查状态的从站总数
        let mut total_checks = 0;
        // 从站FPRD 0x0130的数组
        let mut subdevice_states = heapless::Vec::<_, MAX_SUBDEVICES>::new();

        // 发送单个周期所需的所有帧,包含LRW和FPRD 0x0130
        loop {
            // 计算未发送的PDI数据块大小
            // TODO：chunk_len的长度需要做出限制，不能超过周期帧可容纳的大小；并且也必须是N个从站的PDO总长度，不能从中间截断
            // 在下文push_pdu_slice_rest中会检查数据报是否能放入帧中
            let chunk_len = self.pdi_len.saturating_sub(total_bytes_sent);

            // 退出条件：PDI发送完成
            if chunk_len == 0 && total_checks >= self.len() {
                break;
            }

            // 获得未发送的PDI数据块的字节切片的不可变引用
            let chunk_start = total_bytes_sent.min(self.pdi_len);
            let chunk = pdi_lock.get_mut()[chunk_start..(chunk_start + chunk_len)].as_ref();

            // 从帧管理器中获取一个帧
            let mut frame = maindevice.pdu_loop.alloc_frame()?;

            // 返回PduResponseHandle
            // Start offset in the EtherCAT address space
            let pushed_chunk = if !chunk.is_empty() {
                let start_addr = self.inner().pdi_start.start_address + total_bytes_sent as u32;

                // 将LRW命令放入帧中，数据区填充PDI数据块的字节切片的不可变引用
                // 向帧写入尽可能长的数据报，返回pushed_chunk（已写入的数据报的长度和PduResponseHandle）
                frame.push_pdu_slice_rest(Command::lrw(start_addr).into(), chunk)?
            } else {
                None
            };

            // If there's space left, push as many state checks as we can into the frame
            // 在帧里插入检查状态的数据报，返回剩余未检查状态的从站数组和已检查的从站数。
            // TODO：周期帧里面不应该使用FPRD来读取状态，应该使用BRD
            let (rest, num_checks_in_this_frame) = push_state_checks(subdevices, &mut frame)?;
            subdevices = rest;
            total_checks += num_checks_in_this_frame;

            if frame.is_empty() {
                break;
            }

            // 标记周期帧为可发送,返回ReceiveFrameFut
            // TODO：重发次数应该为0
            let frame = frame.mark_sendable(
                &maindevice.pdu_loop,
                maindevice.timeouts.pdu(),
                maindevice.config.retry_behaviour.retry_count(),
            );

            // 唤醒socket poll发帧，收帧
            maindevice.pdu_loop.wake_sender();

            // 等待接收完成
            let received = frame.await?;

            // ethercrab没有实现完善的数据报返回机制。这里的处理方案是：本函数发送的帧第一个数据报是LRW，后续是FPRD 0x0130。
            // 这个帧的所有数据报内容都已知，因此不用匹配

            // 用于处理一帧多个数据报的情况：转换为数据报迭代器
            let mut pdus = received.into_pdu_iter();

            // 通过PduResponseHandle处理接收的数据报
            // If we pushed a non-zero amount of PDI bytes, process the response
            if let Some((bytes_in_this_chunk, _pdu_handle)) = pushed_chunk {
                // 从数据报中提取PDI数据块的字节切片，返回WKC
                let wkc = self.process_received_pdi_chunk(
                    total_bytes_sent,
                    bytes_in_this_chunk,
                    &pdus.next().ok_or(Error::Internal)??,
                    &mut pdi_lock,
                )?;

                // 更新已发送的PDI数据块大小
                total_bytes_sent += bytes_in_this_chunk;
                lrw_wkc_sum += wkc;
            }

            // LRW命令之后就是FPRD 0x0130，获取所有从站状态
            // If there are any more PDUs, these are state checks
            for state_check_pdu in pdus {
                let state_check_pdu = state_check_pdu?;

                let state = AlControl::unpack_from_slice(&state_check_pdu)?;

                let _ = subdevice_states.push(state.state);
            }
        }

        // 返回总WKC和从站状态
        Ok(TxRxResponse {
            working_counter: lrw_wkc_sum,
            subdevice_states,
            extra: (),
        })
    }

    // DC模式，但没有带SYNC0 同步
    // 发送 FRMW 0x0910 LRW 和 FPRD 0x0130 命令
    /// Drive the SubDevice group's inputs and outputs and synchronise EtherCAT system time with
    /// `FRMW`.
    ///
    /// A `SubDeviceGroup` will not process any inputs or outputs unless this method is called
    /// periodically. It will send an `LRW` to update SubDevice outputs and read SubDevice inputs.
    ///
    /// This method returns a [`TxRxResponse`] struct, containing the working counter, group
    /// SubDevice statuses and the current EtherCAT system time in nanoseconds on success. If the
    /// PDI must be sent in multiple chunks, the returned working counter is the sum of all returned
    /// working counter values.
    ///
    /// # Errors
    ///
    /// This method will return with an error if the PDU could not be sent over the network, or the
    /// response times out.
    pub async fn tx_rx_sync_system_time<'sto>(
        &self,
        maindevice: &'sto MainDevice<'sto>,
    ) -> Result<TxRxResponse<MAX_SUBDEVICES, Option<u64>>, Error> {
        let mut pdi_lock = self.pdi.write();

        fmt::trace!(
            "Group TX/RX with DC sync, start address {:#010x}, data len {}, of which read bytes: {}",
            self.inner().pdi_start.start_address,
            self.pdi_len,
            self.read_pdi_len
        );

        // 可以选择参考时钟从站
        if let Some(dc_ref) = maindevice.dc_ref_address() {
            let mut total_bytes_sent = 0;
            let mut time = 0;
            let mut lrw_wkc_sum = 0;
            let mut time_read = false;

            let mut subdevices = self.inner().subdevices.iter();
            let mut total_checks = 0;
            let mut subdevice_states = heapless::Vec::<_, MAX_SUBDEVICES>::new();

            loop {
                let mut frame = maindevice.pdu_loop.alloc_frame()?;

                let dc_handle = if !time_read {
                    // 往帧中插入FRMW 0x0910命令
                    let dc_handle = frame.push_pdu(
                        Command::frmw(dc_ref, RegisterAddress::DcSystemTime.into()).into(),
                        0u64,
                        None,
                    )?;

                    // Just double checking
                    debug_assert_eq!(dc_handle.alloc_size, DC_PDU_SIZE);

                    Some(dc_handle)
                } else {
                    None
                };

                let chunk_start = total_bytes_sent.min(self.pdi_len);
                let chunk_len = self.pdi_len.saturating_sub(total_bytes_sent);
                let chunk = pdi_lock.get_mut()[chunk_start..(chunk_start + chunk_len)].as_ref();

                let pushed_chunk = if !chunk.is_empty() {
                    let start_addr = self.inner().pdi_start.start_address + total_bytes_sent as u32;

                    // 将LRW命令放入帧中，数据区填充PDI数据块的字节切片的不可变引用
                    frame.push_pdu_slice_rest(Command::lrw(start_addr).into(), chunk)?
                } else {
                    None
                };

                if let Some((bytes_in_this_chunk, _)) = pushed_chunk {
                    fmt::trace!("Wrote {} byte chunk", bytes_in_this_chunk);
                }

                // If there's space left, push as many state checks as we can into the frame
                let (rest, num_checks_in_this_frame) = push_state_checks(subdevices, &mut frame)?;
                subdevices = rest;
                total_checks += num_checks_in_this_frame;

                if frame.is_empty() {
                    break Ok(TxRxResponse {
                        working_counter: lrw_wkc_sum,
                        subdevice_states,
                        extra: Some(time),
                    });
                }

                // 标记周期帧为可发送,返回ReceiveFrameFut
                let frame = frame.mark_sendable(
                    &maindevice.pdu_loop,
                    maindevice.timeouts.pdu(),
                    maindevice.config.retry_behaviour.retry_count(),
                );

                // 唤醒socket poll发帧，收帧
                maindevice.pdu_loop.wake_sender();

                // 等待接收完成
                let received = frame.await?;

                // 用于处理一帧多个数据报的情况：转换为数据报迭代器
                let mut pdus = received.into_pdu_iter();

                if dc_handle.is_some() {
                    let dc_pdu = pdus.next().ok_or(Error::Internal)?;

                    // 获取参考时钟时间
                    // TODO：如果发送多个周期帧，就会重复执行这块代码
                    time =
                        dc_pdu.and_then(|rx| u64::unpack_from_slice(&rx).map_err(Error::from))?;

                    time_read = true;
                }

                // If we pushed a non-zero amount of PDI bytes, process the response
                if let Some((bytes_in_this_chunk, _pdu_handle)) = pushed_chunk {
                    let wkc = self.process_received_pdi_chunk(
                        total_bytes_sent,
                        bytes_in_this_chunk,
                        &pdus.next().ok_or(Error::Internal)??,
                        &mut pdi_lock,
                    )?;

                    total_bytes_sent += bytes_in_this_chunk;
                    lrw_wkc_sum += wkc;
                }

                // If there are any more PDUs, these are state checks
                for state_check_pdu in pdus {
                    let state_check_pdu = state_check_pdu?;

                    let state = AlControl::unpack_from_slice(&state_check_pdu)?;

                    let _ = subdevice_states.push(state.state);
                }

                // NOTE: Not using a while loop as we want to always send the DC sync PDU even if
                // the PDI is empty.
                if chunk_len == 0 && total_checks >= self.len() {
                    break Ok(TxRxResponse {
                        working_counter: lrw_wkc_sum,
                        subdevice_states,
                        extra: Some(time),
                    });
                }
            }
        } else {
            self.tx_rx(maindevice).await.map(|response| TxRxResponse {
                working_counter: response.working_counter,
                subdevice_states: response.subdevice_states,
                extra: None,
            })
        }
    }

    // 从数据报中提取PDI数据块的字节切片，返回WKC
    fn process_received_pdi_chunk(
        &self,
        total_bytes_sent: usize,
        bytes_in_this_chunk: usize,
        data: &ReceivedPdu<'_>,
        pdi_lock: &mut RwLockWriteGuard<'_, R, MySyncUnsafeCell<[u8; MAX_PDI]>>,
    ) -> Result<u16, Error> {
        let wkc = data.working_counter;

        // 计算逻辑读对应的PDI数据区中的input的字节范围
        let rx_range = total_bytes_sent.min(self.read_pdi_len)
            ..(total_bytes_sent + bytes_in_this_chunk).min(self.read_pdi_len);

        // 获得LRW数据区的读写锁对应的字节切片
        let inputs_chunk = &mut pdi_lock.get_mut()[rx_range];

        // 从数据报中提取PDI数据块的字节切片
        inputs_chunk.copy_from_slice(data.get(0..inputs_chunk.len()).ok_or(Error::Internal)?);

        Ok(wkc)
    }
}

// Methods for when the group has a PDI AND has Distributed Clocks configured
impl<const MAX_SUBDEVICES: usize, const MAX_PDI: usize, R: RawRwLock, S>
    SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, R, S, HasDc>
where
    S: HasPdi,
{
    // DC模式，带SYNC0 同步。周期收发帧函数
    /// Drive the SubDevice group's inputs and outputs, synchronise EtherCAT system time with
    /// `FRMW`, and return cycle timing and SubDevice state information.
    ///
    /// A `SubDeviceGroup` will not process any inputs or outputs unless this method is called
    /// periodically. It will send an `LRW` to update SubDevice outputs and read SubDevice inputs.
    ///
    /// This method returns a [`TxRxResponse`] struct, containing the working counter, a
    /// [`CycleInfo`] containing values that can be used to synchronise the MainDevice to the
    /// network SYNC0 event, and the state of all SubDevices in the group.
    ///
    /// # Errors
    ///
    /// This method will return with an error if the PDU could not be sent over the network, or the
    /// response times out.
    ///
    /// # Examples
    ///
    /// This example sends process data at 2.5ms offset into a 5ms cycle.
    ///
    /// ```rust,no_run
    /// # use ethercrab::{
    /// #     error::Error,
    /// #     subdevice_group::{CycleInfo, DcConfiguration, TxRxResponse},
    /// #     std::ethercat_now,
    /// #     MainDevice, MainDeviceConfig, PduStorage, Timeouts, DcSync,
    /// # };
    /// # use std::time::{Duration, Instant};
    /// # const MAX_SUBDEVICES: usize = 16;
    /// # const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// # const MAX_FRAMES: usize = 32;
    /// # const PDI_LEN: usize = 64;
    /// # static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();
    /// # fn main() -> Result<(), Error> { smol::block_on(async {
    /// let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
    ///
    /// let maindevice = MainDevice::new(pdu_loop, Timeouts::default(), MainDeviceConfig::default());
    ///
    /// let cycle_time = Duration::from_millis(5);
    ///
    /// let mut group = maindevice
    ///     .init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now)
    ///     .await
    ///     .expect("Init");
    ///
    /// // This example enables SYNC0 for every detected SubDevice
    /// for mut subdevice in group.iter_mut(&maindevice) {
    ///     subdevice.set_dc_sync(DcSync::Sync0);
    /// }
    ///
    /// let mut group = group
    ///     .into_pre_op_pdi(&maindevice)
    ///     .await
    ///     .expect("PRE-OP -> PRE-OP with PDI")
    ///     .configure_dc_sync(
    ///         &maindevice,
    ///         DcConfiguration {
    ///             // Start SYNC0 100ms in the future
    ///             start_delay: Duration::from_millis(100),
    ///             // SYNC0 period should be the same as the process data loop in most cases
    ///             sync0_period: cycle_time,
    ///             // Send process data half way through cycle
    ///             sync0_shift: cycle_time / 2,
    ///         },
    ///     )
    ///     .await
    ///     .expect("DC configuration")
    ///     .request_into_op(&maindevice)
    ///     .await
    ///     .expect("PRE-OP -> SAFE-OP -> OP");
    ///
    /// // Wait for all SubDevices in the group to reach OP, whilst sending PDI to allow DC to start
    /// // correctly.
    /// loop {
    ///     let now = Instant::now();
    ///
    ///     let response @ TxRxResponse {
    ///         working_counter: _wkc,
    ///         extra: CycleInfo {
    ///             next_cycle_wait, ..
    ///         },
    ///         ..
    ///     } = group.tx_rx_dc(&maindevice).await.expect("TX/RX");
    ///
    ///     if response.all_op() {
    ///         break;
    ///     }
    ///
    ///     smol::Timer::at(now + next_cycle_wait).await;
    /// }
    ///
    /// // Main application process data cycle
    /// loop {
    ///     let now = Instant::now();
    ///
    ///     let TxRxResponse {
    ///         working_counter: _wkc,
    ///         extra: CycleInfo {
    ///             next_cycle_wait, ..
    ///         },
    ///         ..
    ///     } = group.tx_rx_dc(&maindevice).await.expect("TX/RX");
    ///
    ///     // Process data computations happen here
    ///
    ///     smol::Timer::at(now + next_cycle_wait).await;
    /// }
    /// # }) }
    /// ```
    pub async fn tx_rx_dc<'sto>(
        &self,
        maindevice: &'sto MainDevice<'sto>,
    ) -> Result<TxRxResponse<MAX_SUBDEVICES, CycleInfo>, Error> {
        fmt::trace!(
            "Group TX/RX with DC sync, start address {:#010x}, data len {}, of which read bytes: {}",
            self.inner().pdi_start.start_address,
            self.pdi_len,
            self.read_pdi_len
        );

        // 获得PDI数据区的读写锁
        let mut pdi_lock = self.pdi.write();

        let mut total_bytes_sent = 0;
        let mut time = 0;
        let mut lrw_wkc_sum = 0;
        let mut time_read = false;

        let mut subdevices = self.inner().subdevices.iter();
        let mut total_checks = 0;
        let mut subdevice_states = heapless::Vec::<_, MAX_SUBDEVICES>::new();

        loop {
            // 分配一个帧
            let mut frame = maindevice.pdu_loop.alloc_frame()?;

            let dc_handle = if !time_read {
                let dc_handle = frame.push_pdu(
                    // FRMW 0x0910 同步系统时间
                    Command::frmw(self.dc_conf.reference, RegisterAddress::DcSystemTime.into())
                        .into(),
                    0u64,
                    None,
                )?;

                // Just double checking
                debug_assert_eq!(dc_handle.alloc_size, DC_PDU_SIZE);

                Some(dc_handle)
            } else {
                None
            };

            let chunk_start = total_bytes_sent.min(self.pdi_len);
            let chunk_len = self.pdi_len.saturating_sub(total_bytes_sent);
            let chunk = pdi_lock.get_mut()[chunk_start..(chunk_start + chunk_len)].as_ref();

            let pushed_chunk = if !chunk.is_empty() {
                let start_addr = self.inner().pdi_start.start_address + total_bytes_sent as u32;

                // LRW 命令
                frame.push_pdu_slice_rest(Command::lrw(start_addr).into(), chunk)?
            } else {
                None
            };

            // 状态检查
            // If there's space left, push as many state checks as we can into the frame
            let (rest, num_checks_in_this_frame) = push_state_checks(subdevices, &mut frame)?;
            subdevices = rest;
            total_checks += num_checks_in_this_frame;

            if frame.is_empty() {
                break;
            }

            let frame = frame.mark_sendable(
                &maindevice.pdu_loop,
                maindevice.timeouts.pdu(),
                maindevice.config.retry_behaviour.retry_count(),
            );

            maindevice.pdu_loop.wake_sender();

            let received = frame.await?;

            let mut pdus = received.into_pdu_iter();

            if dc_handle.is_some() {
                let dc_pdu = pdus.next().ok_or(Error::Internal)?;

                // 获得参考时钟时间
                time = dc_pdu.and_then(|rx| u64::unpack_from_slice(&rx).map_err(Error::from))?;

                time_read = true;
            }

            // 获得PDI数据区的读写锁
            // If we pushed a non-zero amount of PDI bytes, process the response
            if let Some((bytes_in_this_chunk, _pdu_handle)) = pushed_chunk {
                let wkc = self.process_received_pdi_chunk(
                    total_bytes_sent,
                    bytes_in_this_chunk,
                    &pdus.next().ok_or(Error::Internal)??,
                    &mut pdi_lock,
                )?;

                total_bytes_sent += bytes_in_this_chunk;
                lrw_wkc_sum += wkc;
            }

            // 获得从站状态
            // If there are any more PDUs, these are state checks
            for state_check_pdu in pdus {
                let state_check_pdu = state_check_pdu?;

                let state = AlControl::unpack_from_slice(&state_check_pdu)?;

                let _ = subdevice_states.push(state.state);
            }

            // NOTE: Not using a while loop as we want to always send the DC sync PDU even if the
            // PDI is empty.
            // This condition will exit the loop if the whole PDI has been sent as well as all
            // SubDevice status check PDUs.
            if chunk_len == 0 && total_checks >= self.len() {
                break;
            }
        }

        // 分布式时钟时间同步算法核心逻辑
        // 根据参考时钟从站的系统时间，计算主设备下一次数据交换的精确时机
        // 这个算法确保主设备与网络中的SYNC0脉冲保持同步，实现精确的实时控制

        // 计算当前时间距离当前周期起始点的偏移量
        // 例如：如果周期是1ms，当前时间是1234567ns，那么偏移量是234567ns
        // 从周期开始算起的纳秒数。这样做的原因是第一个 SYNC0 脉冲的时间被四舍五入到 `sync0_period` 长度的周期的整数倍。
        // TODO 这里的time是64位，32位的情况没有考虑
        let cycle_start_offset = time % self.dc_conf.sync0_period;

        // 计算到下一个周期开始还需等待的时间，加上用户设定的偏移量
        // 这样可以确保主设备在指定的时间点进行数据交换，与网络的SYNC0脉冲保持同步
        // 公式： 本周期休眠时间 = (完整周期时间 - 当前周期已过去的时间) + 用户指定的偏移时间
        let time_to_next_iter =
            (self.dc_conf.sync0_period - cycle_start_offset) + self.dc_conf.sync0_shift;

        Ok(TxRxResponse {
            working_counter: lrw_wkc_sum,
            subdevice_states,
            extra: CycleInfo {
                dc_system_time: time,
                cycle_start_offset: Duration::from_nanos(cycle_start_offset),
                next_cycle_wait: Duration::from_nanos(time_to_next_iter),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MainDeviceConfig, PduStorage, Timeouts,
        ethernet::{EthernetAddress, EthernetFrame},
        pdu_loop::ReceivedFrame,
    };
    use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::{sync::Arc, thread};

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn tx_rx_miri() {
        const MAX_SUBDEVICES: usize = 16;
        const MAX_PDU_DATA: usize = PduStorage::element_size(8);
        const MAX_FRAMES: usize = 128;
        const MAX_PDI: usize = 128;

        static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

        crate::test_logger();

        let (mock_net_tx, mock_net_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);

        let (mut tx, mut rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

        let stop = Arc::new(AtomicBool::new(false));

        let stop1 = stop.clone();

        let tx_handle = thread::spawn(move || {
            fmt::info!("Spawn TX task");

            while !stop1.load(Ordering::Relaxed) {
                while let Some(frame) = tx.next_sendable_frame() {
                    fmt::info!("Sendable frame");

                    frame
                        .send_blocking(|bytes| {
                            mock_net_tx.send(bytes.to_vec()).unwrap();

                            Ok(bytes.len())
                        })
                        .unwrap();

                    thread::yield_now();
                }

                thread::sleep(Duration::from_millis(1));
            }
        });

        let stop1 = stop.clone();

        let rx_handle = thread::spawn(move || {
            fmt::info!("Spawn RX task");

            while let Ok(ethernet_frame) = mock_net_rx.recv() {
                fmt::info!("RX task received packet");

                // Let frame settle for a mo
                thread::sleep(Duration::from_millis(1));

                // Munge fake sent frame into a fake received frame
                let ethernet_frame = {
                    let mut frame = EthernetFrame::new_checked(ethernet_frame).unwrap();
                    frame.set_src_addr(EthernetAddress([0x12, 0x10, 0x10, 0x10, 0x10, 0x10]));
                    frame.into_inner()
                };

                while rx.receive_frame(&ethernet_frame).is_err() {}

                thread::yield_now();

                if stop1.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let maindevice = Arc::new(MainDevice::new(
            pdu_loop,
            Timeouts {
                pdu: Duration::from_secs(1),
                wait_loop_delay: Duration::ZERO,
                ..Timeouts::default()
            },
            MainDeviceConfig::default(),
        ));

        let group: SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, crate::DefaultLock, PreOpPdi, NoDc> =
            SubDeviceGroup {
                id: GroupId(0),
                pdi: RwLock::new(MySyncUnsafeCell::new([0u8; MAX_PDI])),
                read_pdi_len: 32,
                pdi_len: 96,
                inner: MySyncUnsafeCell::new(GroupInner {
                    subdevices: heapless::Vec::new(),
                    pdi_start: PdiOffset::default(),
                }),
                dc_conf: NoDc,
                _state: PhantomData,
            };

        let out = group.tx_rx(&maindevice).await;

        // No subdevices so no WKC, but success
        assert_eq!(
            out,
            Ok(TxRxResponse {
                working_counter: 0,
                subdevice_states: heapless::Vec::new(),
                extra: ()
            })
        );

        stop.store(true, Ordering::Relaxed);

        tx_handle.join().unwrap();
        rx_handle.join().unwrap();
    }

    #[test]
    fn multi_state_checks_single_frame() {
        const MAX_FRAMES: usize = 1;
        const MAX_PDU_DATA: usize = PduStorage::element_size(AlControl::PACKED_LEN);
        static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

        crate::test_logger();

        let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

        let mut frame = pdu_loop.alloc_frame().expect("No frame");

        assert!(
            frame.can_push_pdu_payload(AlControl::PACKED_LEN),
            "should be possible to push one status check PDU"
        );
        assert!(
            !frame.can_push_pdu_payload(AlControl::PACKED_LEN + 12),
            "test requires the frame to fit exactly one status check PDU"
        );

        let single_sd = vec![SubDevice {
            ..SubDevice::default()
        }];

        let subdevices = single_sd.iter();

        let (rest, num_pushed) =
            push_state_checks(subdevices, &mut frame).expect("Could not push status check");

        assert_eq!(rest.count(), 0);
        assert_eq!(num_pushed, single_sd.len());

        assert!(!frame.can_push_pdu_payload(1), "frame should be full");
    }

    #[test]
    fn multi_state_checks_space_left_over() {
        // 1 byte left. AlControl takes 2 bytes.
        const SPACE_LEFT: usize = 1;

        const MAX_FRAMES: usize = 1;
        const MAX_PDU_DATA: usize = (AlControl::PACKED_LEN + CreatedFrame::PDU_OVERHEAD_BYTES) * 2
            + (SPACE_LEFT + CreatedFrame::PDU_OVERHEAD_BYTES)
            // Ethernet and EtherCAT frame headers
            + 16;
        static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

        crate::test_logger();

        let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

        let mut frame = pdu_loop.alloc_frame().expect("No frame");

        let sds = vec![
            SubDevice {
                ..SubDevice::default()
            },
            SubDevice {
                ..SubDevice::default()
            },
            SubDevice {
                ..SubDevice::default()
            },
        ];

        let subdevices = sds.iter();

        let (rest, num_pushed) =
            push_state_checks(subdevices, &mut frame).expect("Could not push status check");

        assert_eq!(num_pushed, 2, "frame should hold two SD status checks");
        assert_eq!(rest.count(), 1, "frame can only hold two SD status checks");

        assert!(
            frame.can_push_pdu_payload(SPACE_LEFT),
            "frame has {} bytes available",
            SPACE_LEFT
        );
    }

    // This records the behaviour of a DC setup of the following 16 SubDevices:
    //
    // - EK1100
    // - EL2828
    // - EL2889
    // - EL2004
    // - EL1004
    // - EL1018
    // - EL1008
    // - EL1004
    // - EL2004
    // - EL2008
    // - EL1008
    // - EL2008
    // - EL2008
    // - EL2522
    // - EL1258
    // - EL9505
    #[test]
    fn large_group_frame_split() {
        const MAX_SUBDEVICES: usize = 32;
        const MAX_PDU_DATA: usize = PduStorage::element_size(256);
        const MAX_FRAMES: usize = 32;
        const MAX_PDI: usize = 512;
        static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

        crate::test_logger();

        let (mock_net_tx, mock_net_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);

        let (mut tx, mut rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

        let maindevice = Arc::new(MainDevice::new(
            pdu_loop,
            Timeouts::default(),
            MainDeviceConfig::default(),
        ));

        let stop = Arc::new(AtomicBool::new(false));

        let stop1 = stop.clone();

        let tx_handle = thread::spawn(move || {
            fmt::info!("Spawn TX task");

            while !stop1.load(Ordering::Relaxed) {
                while let Some(frame) = tx.next_sendable_frame() {
                    fmt::info!("Sendable frame");

                    frame
                        .send_blocking(|bytes| {
                            mock_net_tx.send(bytes.to_vec()).unwrap();

                            Ok(bytes.len())
                        })
                        .unwrap();

                    thread::yield_now();
                }
            }
        });

        let stop1 = stop.clone();

        let rx_handle = thread::spawn(move || {
            fmt::info!("Spawn RX task");

            while let Ok(ethernet_frame) = mock_net_rx.recv() {
                fmt::info!("RX task received packet");

                // Munge fake sent frame into a fake received frame
                let ethernet_frame = {
                    let mut frame = EthernetFrame::new_checked(ethernet_frame).unwrap();
                    frame.set_src_addr(EthernetAddress([0x12, 0x10, 0x10, 0x10, 0x10, 0x10]));
                    frame.into_inner()
                };

                while rx.receive_frame(&ethernet_frame).is_err() {}

                thread::yield_now();

                if stop1.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        fn sd(addr: u16) -> SubDevice {
            SubDevice {
                configured_address: addr,
                ..SubDevice::default()
            }
        }

        let subdevices = heapless::Vec::<_, MAX_SUBDEVICES>::from_slice(&[
            sd(0x1000),
            sd(0x1001),
            sd(0x1002),
            sd(0x1003),
            sd(0x1004),
            sd(0x1005),
            sd(0x1006),
            sd(0x1007),
            sd(0x1008),
            sd(0x1009),
            sd(0x100a),
            sd(0x100b),
            sd(0x100c),
            sd(0x100d),
            sd(0x100e),
            sd(0x100f),
        ])
        .unwrap();

        // Test setup had 16 devices
        assert_eq!(subdevices.len(), 16);

        let group = SubDeviceGroup::<MAX_SUBDEVICES, MAX_PDI, crate::DefaultLock, Op, HasDc> {
            id: GroupId(0),
            pdi: RwLock::new(MySyncUnsafeCell::new([0u8; MAX_PDI])),
            read_pdi_len: 406,
            pdi_len: 474,
            inner: MySyncUnsafeCell::new(GroupInner {
                subdevices,
                pdi_start: PdiOffset { start_address: 0 },
            }),
            dc_conf: HasDc {
                sync0_period: 100_000,
                sync0_shift: 0,
                reference: 0,
            },
            _state: PhantomData::<Op>,
        };

        cassette::block_on(group.tx_rx_dc(&maindevice)).unwrap();

        stop.store(true, Ordering::Relaxed);

        tx_handle.join().unwrap();
        rx_handle.join().unwrap();

        const PDI_FRAME_0: usize = 236;
        const PDI_FRAME_1: usize = 238;

        assert_eq!(PDI_FRAME_0 + PDI_FRAME_1, 474);

        // Expected PDU lengths for each frame
        let expected_pdus = [
            [
                8,           // DC FRMW
                PDI_FRAME_0, // Consume rest of frame with PDI
            ]
            .as_slice(),
            &[
                PDI_FRAME_1, // Entire frame filled with PDI
                2,           // First status check
            ],
            // 15 remaining SubDevice status checks
            &[2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
        ];

        // We should have sent 3 frames in this test
        for (i, expected_lens) in expected_pdus.iter().enumerate() {
            let f = maindevice
                .pdu_loop
                .test_only_storage_ref()
                .frame_at_index(i);

            let idx = AtomicU8::new(i as u8);

            let b = ReceivedFrame::from_frame_element_for_test_only(f, &idx, MAX_PDU_DATA);

            let expected_pdu_count = expected_lens.len();
            let mut actual_pdu_count = 0;

            for (pdu_idx, pdu) in b.into_pdu_iter().enumerate() {
                let pdu = pdu.unwrap();

                actual_pdu_count += 1;

                assert_eq!(
                    pdu.len(),
                    expected_lens[pdu_idx],
                    "frame {}, PDU {} length",
                    i,
                    pdu_idx
                );
            }

            assert_eq!(
                actual_pdu_count, expected_pdu_count,
                "frame {} PDU count",
                i
            );
        }

        let f = maindevice
            .pdu_loop
            .test_only_storage_ref()
            .frame_at_index(3);
        let idx = AtomicU8::new(3);
        let b = ReceivedFrame::from_frame_element_for_test_only(f, &idx, MAX_PDU_DATA);

        // 4th frame should be empty as we only sent 3
        assert_eq!(b.into_pdu_iter().count(), 0);
    }
}
