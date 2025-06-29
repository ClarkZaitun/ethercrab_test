use crate::{
    BASE_SUBDEVICE_ADDRESS, MainDeviceConfig, SubDeviceGroup, Timeouts,
    al_control::AlControl,
    al_status_code::AlStatusCode,
    command::Command,
    dc,
    eeprom::types::SyncManager,
    error::{Error, Item},
    fmmu::Fmmu,
    fmt,
    pdi::PdiOffset,
    pdu_loop::{PduLoop, ReceivedPdu},
    register::RegisterAddress,
    subdevice::SubDevice,
    subdevice_group::{self, SubDeviceGroupHandle},
    subdevice_state::SubDeviceState,
    timer_factory::IntoTimeout,
};
use core::{
    cell::UnsafeCell,
    mem::size_of,
    sync::atomic::{AtomicU16, Ordering},
};
use ethercrab_wire::{EtherCrabWireSized, EtherCrabWireWrite};
use heapless::FnvIndexMap;

/// The main EtherCAT controller.
///
/// The `MainDevice` is passed by reference to [`SubDeviceGroup`]s to drive their TX/RX methods. It
/// also provides direct access to EtherCAT PDUs like `BRD`, `LRW`, etc.
#[doc(alias = "Client")]
#[doc(alias = "Master")]
#[derive(Debug)]
pub struct MainDevice<'sto> {
    pub(crate) pdu_loop: PduLoop<'sto>,
    /// The total number of discovered subdevices.
    ///
    /// Using an `AtomicU16` here only to satisfy `Sync` requirements, but it's only ever written to
    /// once so its safety is largely unused.
    num_subdevices: AtomicU16,
    /// DC reference clock.
    ///
    /// If no DC subdevices are found, this will be `0`.
    // 参考时钟的配置地址
    dc_reference_configured_address: AtomicU16,
    pub(crate) timeouts: Timeouts, // 用于确认从站状态切换到pre op的超时时间
    pub(crate) config: MainDeviceConfig,
}

// 为 MainDevice<'_> 类型手动实现 Sync trait。
// 多数情况下，Rust 会依据类型的字段自动推断该类型是否实现 Sync。例如，若一个结构体的所有字段都实现了 Sync，那么这个结构体也会自动实现 Sync。不过，在某些特殊情形下，需要手动实现 Sync trait，特别是当类型包含一些 unsafe 代码或者特殊的内存管理逻辑时。
// 对于 MainDevice<'_> 类型，由于它包含了一个 PduLoop<'sto> 字段，这个字段可能包含了一些 unsafe 代码或者特殊的内存管理逻辑，因此需要手动实现 Sync trait。
unsafe impl Sync for MainDevice<'_> {}

impl<'sto> MainDevice<'sto> {
    /// Create a new EtherCrab MainDevice.
    pub const fn new(
        pdu_loop: PduLoop<'sto>,
        timeouts: Timeouts,
        config: MainDeviceConfig,
    ) -> Self {
        Self {
            pdu_loop,
            num_subdevices: AtomicU16::new(0),
            dc_reference_configured_address: AtomicU16::new(0),
            timeouts,
            config,
        }
    }

    // BWR 数据全为0的数据报，不检查返回帧WKC是否正确
    /// Write zeroes to every SubDevice's memory in chunks.
    async fn blank_memory<const LEN: usize>(&self, start: impl Into<u16>) -> Result<(), Error> {
        let start = start.into(); // 这个into在哪里实现的？

        self.pdu_loop
            // BWR 数据全为0的数据报，不检查返回帧WKC是否正确
            .pdu_broadcast_zeros(
                start,
                LEN as u16,
                self.timeouts.pdu,
                self.config.retry_behaviour.retry_count(),
            )
            .await
    }

    // FIXME: When adding a powered on SubDevice to the network, something breaks. Maybe need to reset
    // the configured address? But this broke other stuff so idk...
    // 重置的寄存器可能还不够，没有检查WKC
    async fn reset_subdevices(&self) -> Result<(), Error> {
        fmt::debug!("Beginning reset");

        // BWR 0x0120
        // Reset SubDevices to init
        Command::bwr(RegisterAddress::AlControl.into())
            .ignore_wkc() // 不应该忽略WKC
            .send(self, AlControl::reset())
            .await?;

        // 没必要重置所有FMMU，浪费启动时间
        // Clear FMMUs - see ETG1000.4 Table 57
        // Some devices aren't able to blank the entire region so we loop through all offsets.
        for fmmu_idx in 0..16 {
            self.blank_memory::<{ Fmmu::PACKED_LEN }>(RegisterAddress::fmmu(fmmu_idx)) //Fmmu::PACKED_LEN 16
                .await?;
        }

        // 没必要重置所有SM，浪费启动时间
        // Clear SMs - see ETG1000.4 Table 59
        // Some devices aren't able to blank the entire region so we loop through all offsets.
        for sm_idx in 0..16 {
            self.blank_memory::<{ SyncManager::PACKED_LEN }>(RegisterAddress::sync_manager(sm_idx))
                .await?;
        }

        // 重置0x980、0x0910、0x0920、0x0928、0x092C、0x0981、0x0990、0x09A0、0x09A4
        // Set DC control back to EtherCAT
        self.blank_memory::<{ size_of::<u8>() }>(RegisterAddress::DcCyclicUnitControl)
            .await?;
        self.blank_memory::<{ size_of::<u64>() }>(RegisterAddress::DcSystemTime)
            .await?;
        self.blank_memory::<{ size_of::<u64>() }>(RegisterAddress::DcSystemTimeOffset)
            .await?;
        self.blank_memory::<{ size_of::<u32>() }>(RegisterAddress::DcSystemTimeTransmissionDelay)
            .await?;
        self.blank_memory::<{ size_of::<u32>() }>(RegisterAddress::DcSystemTimeDifference)
            .await?;
        self.blank_memory::<{ size_of::<u8>() }>(RegisterAddress::DcSyncActive)
            .await?;
        self.blank_memory::<{ size_of::<u32>() }>(RegisterAddress::DcSyncStartTime)
            .await?;
        self.blank_memory::<{ size_of::<u32>() }>(RegisterAddress::DcSync0CycleTime)
            .await?;
        self.blank_memory::<{ size_of::<u32>() }>(RegisterAddress::DcSync1CycleTime)
            .await?;

        // ETG1020 Section 22.2.4 defines these initial parameters. The data types are defined in
        // ETG1000.4 Table 60 – Distributed clock local time parameter, helpfully named "Control
        // Loop Parameter 1" to 3.
        //
        // According to ETG1020, we'll use the mode where the DC reference clock is adjusted to the
        // master clock.
        // BWR 0x0934
        Command::bwr(RegisterAddress::DcControlLoopParam3.into())
            .ignore_wkc()
            .send(self, 0x0c00u16)
            .await?;
        // BWR 0x0930
        // Must be after param 3 so DC control unit is reset
        Command::bwr(RegisterAddress::DcControlLoopParam1.into())
            .ignore_wkc()
            .send(self, 0x1000u16)
            .await?;

        fmt::debug!("--> Reset complete");

        Ok(())
    }

    // 检测子设备，设置其配置的站地址，分配到组，从 EEPROM 配置子设备。
    /// Detect SubDevices, set their configured station addresses, assign to groups, configure
    /// SubDevices from EEPROM.
    ///
    /// This method will request and wait for all SubDevices to be in `PRE-OP` before returning.
    ///
    /// To transition groups into different states, see [`SubDeviceGroup::into_safe_op`] or
    /// [`SubDeviceGroup::into_op`].
    ///
    /// The `group_filter` closure should return a [`&dyn
    /// SubDeviceGroupHandle`](crate::subdevice_group::SubDeviceGroupHandle) to add the SubDevice
    /// to. All SubDevices must be assigned to a group even if they are unused.
    ///
    /// If a SubDevice cannot or should not be added to a group for some reason (e.g. an
    /// unrecognised SubDevice was detected on the network), an
    /// [`Err(Error::UnknownSubDevice)`](Error::UnknownSubDevice) should be returned.
    ///
    /// `MAX_SUBDEVICES` must be a power of 2 greater than 1.
    ///
    /// Note that the sum of the PDI data length for all [`SubDeviceGroup`]s must not exceed the
    /// value of `MAX_PDU_DATA`.
    ///
    /// # Examples
    ///
    /// ## Multiple groups
    ///
    /// This example groups SubDevices into two different groups.
    ///
    /// ```rust,no_run
    /// use ethercrab::{
    ///     error::Error, std::{ethercat_now, tx_rx_task}, MainDevice, MainDeviceConfig, PduStorage,
    ///     SubDeviceGroup, Timeouts, subdevice_group
    /// };
    ///
    /// const MAX_SUBDEVICES: usize = 2;
    /// const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// const MAX_FRAMES: usize = 16;
    ///
    /// static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();
    ///
    /// /// A custom struct containing two groups to assign SubDevices into.
    /// #[derive(Default)]
    /// struct Groups {
    ///     /// 2 SubDevices, totalling 1 byte of PDI.
    ///     group_1: SubDeviceGroup<2, 1>,
    ///     /// 1 SubDevice, totalling 4 bytes of PDI
    ///     group_2: SubDeviceGroup<1, 4>,
    /// }
    ///
    /// let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
    ///
    /// let maindevice = MainDevice::new(pdu_loop, Timeouts::default(), MainDeviceConfig::default());
    ///
    /// # async {
    /// let groups = maindevice
    ///     .init::<MAX_SUBDEVICES, _>(ethercat_now, |groups: &Groups, subdevice| {
    ///         match subdevice.name() {
    ///             "COUPLER" | "IO69420" => Ok(&groups.group_1),
    ///             "COOLSERVO" => Ok(&groups.group_2),
    ///             _ => Err(Error::UnknownSubDevice),
    ///         }
    ///     },)
    ///     .await
    ///     .expect("Init");
    /// # };
    /// ```
    // 检测网络上的从站设备数量
    // 重置所有从站设备到初始状态
    // 为每个从站设备分配配置地址
    // 配置分布式时钟(DC)拓扑和同步
    // 使用提供的 group_filter 闭包将设备分配到不同组
    // 配置每个组的PDI(过程数据映像)偏移量
    // 配置邮箱
    // 等待所有设备进入PRE-OP状态
    pub async fn init<const MAX_SUBDEVICES: usize, G>(
        &self,
        now: impl Fn() -> u64 + Copy,
        mut group_filter: impl for<'g> FnMut(
            //group_filter：分组过滤器闭包，用于决定每个从站设备分配到哪个组
            // 如果要分为多组，则需要设置init函数的过滤器group_filter
            // 哪里设置每个组的逻辑地址？
            &'g G, // 传入闭包时已经指定了G的类型
            &SubDevice,
        ) -> Result<&'g dyn SubDeviceGroupHandle, Error>,
    ) -> Result<G, Error>
    where
        G: Default, //表示分组容器类型的泛型参数，必须实现 Default trait
                    // 可以从调用这个函数的函数init_single_group的返回值推断出来G就是SubDeviceGroup吗？
    {
        let groups = G::default();

        // Each SubDevice increments working counter, so we can use it as a total count of
        // SubDevices
        let num_subdevices = self.count_subdevices().await?;

        fmt::debug!("Discovered {} SubDevices", num_subdevices);

        if num_subdevices == 0 {
            fmt::warn!(
                "No SubDevices were discovered. Check NIC device, connections and PDU response timeouts"
            );

            return Ok(groups);
        }

        // 初始化所有从站：请求init，重置寄存器
        self.reset_subdevices().await?;

        // This is the only place we store the number of SubDevices, so the ordering can be
        // pretty much anything.
        self.num_subdevices.store(num_subdevices, Ordering::Relaxed);

        // 使用heapless库提供的双端队列实现
        // 创建了一个固定容量的双端队列(deque)来存储从站设备(SubDevice)实例
        let mut subdevices = heapless::Deque::<SubDevice, MAX_SUBDEVICES>::new();

        // 设置从站配置地址，确认从站在init状态；从EEPROM读取从站名称和标识信息；从寄存器读取ESC支持功能，地址别名，端口，创建从站
        // Set configured address for all discovered SubDevices
        for subdevice_idx in 0..num_subdevices {
            // 配置地址从0x1000开始
            let configured_address = BASE_SUBDEVICE_ADDRESS.wrapping_add(subdevice_idx);

            // APWR 0x0010 设置从站配置地址，没检查WKC
            Command::apwr(
                subdevice_idx,
                RegisterAddress::ConfiguredStationAddress.into(),
            )
            .send(self, configured_address)
            .await?;

            // 确认从站在init状态；从EEPROM读取从站名称和标识信息；从寄存器读取ESC支持功能，地址别名，端口，创建从站
            let subdevice = SubDevice::new(self, subdevice_idx, configured_address).await?;

            subdevices
                .push_back(subdevice)
                .map_err(|_| Error::Capacity(Item::SubDevice))?;
        }

        fmt::debug!("Configuring topology/distributed clocks");

        // Configure distributed clock offsets/propagation delays, perform static drift
        // compensation. We need the SubDevices in a single list so we can read the topology.
        // 配置分布时钟偏移/传播延迟，执行静态漂移补偿。我们需要将子设备放在一个列表中，以便读取拓扑结构。
        let dc_master = dc::configure_dc(self, subdevices.as_mut_slices().0, now).await?;
        // 应该叫做参考时钟从站 Reference Clock Slave

        // If there are SubDevices that support distributed clocks, run static drift compensation
        // 保存参考时钟地址到主站结构体中，进行时钟漂移补偿
        if let Some(dc_master) = dc_master {
            self.dc_reference_configured_address
                .store(dc_master.configured_address(), Ordering::Relaxed);

            dc::run_dc_static_sync(self, dc_master, self.config.dc_static_sync_iterations).await?;
        }

        // This block is to reduce the lifetime of the groups map references
        // 此块用于减少组映射引用的生命周期
        {
            // A unique list of groups so we can iterate over them and assign consecutive PDIs to each
            // one.
            // 一个唯一的组列表，以便我们可以对它们进行迭代并为每个组分配连续的 PDI。
            // 创建一个固定容量的映射表，用于存储组 ID 到组的映射
            let mut group_map = FnvIndexMap::<_, _, MAX_SUBDEVICES>::new();

            while let Some(subdevice) = subdevices.pop_front() {
                let group = group_filter(&groups, &subdevice)?;

                // SAFETY: This mutates the internal SubDevice list, so a reference to `group` may not be
                // held over this line.
                unsafe { group.push(subdevice)? };

                group_map
                    .insert(usize::from(group.id()), UnsafeCell::new(group))
                    .map_err(|_| Error::Capacity(Item::Group))?;
            }

            // 默认为0？
            let mut offset = PdiOffset::default();

            for (id, group) in group_map.into_iter() {
                let group = unsafe { *group.get() };

                // 将 SubDeviceGroup 转换为 SubDeviceGroupRef 类型，同时擦除其常量泛型参数
                // 初始化组内的所有从设备（SubDevice），并将它们置于 PRE-OP状态，同时配置组内从设备在过程数据映像（PDI）中的映射。这里会修改pdi_position的start_address
                // 通过EEOROM数据配置邮箱，切换到PreOp，读取对象字典中的0x1c00同步管理器类型，保存邮箱配置
                offset = group.as_ref().into_pre_op(offset, self).await?;

                fmt::debug!("After group ID {} offset: {:?}", id, offset);
            }

            fmt::debug!("Total PDI {} bytes", offset.start_address);
        }

        // Check that all SubDevices reached PRE-OP
        // 等待所有从站切换到指定状态，如果出现故障则打印错误码，返回
        self.wait_for_state(SubDeviceState::PreOp).await?;

        Ok(groups)
    }

    //此方法将请求并等待所有子设备处于“PRE-OP”状态后再返回。
    /// A convenience method to allow the quicker creation of a single group containing all
    /// discovered SubDevices.
    ///
    /// This method will request and wait for all SubDevices to be in `PRE-OP` before returning.
    ///
    /// To transition groups into different states, see [`SubDeviceGroup::into_safe_op`] or
    /// [`SubDeviceGroup::into_op`].
    ///
    /// For multiple groups, see [`MainDevice::init`].
    ///
    /// # Examples
    ///
    /// ## Create a single SubDevice group with no `PREOP -> SAFEOP` configuration
    ///
    /// ```rust,no_run
    /// use ethercrab::{
    ///     error::Error, MainDevice, MainDeviceConfig, PduStorage, Timeouts, std::ethercat_now
    /// };
    ///
    /// const MAX_SUBDEVICES: usize = 2;
    /// const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// const MAX_FRAMES: usize = 16;
    /// const MAX_PDI: usize = 8;
    ///
    /// static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();
    ///
    /// let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
    ///
    /// let maindevice = MainDevice::new(pdu_loop, Timeouts::default(), MainDeviceConfig::default());
    ///
    /// # async {
    /// let group = maindevice
    ///     .init_single_group::<MAX_SUBDEVICES, MAX_PDI>(ethercat_now)
    ///     .await
    ///     .expect("Init");
    /// # };
    /// ```
    ///
    /// ## Create a single SubDevice group with `PREOP -> SAFEOP` configuration of SDOs
    ///
    /// ```rust,no_run
    /// use ethercrab::{
    ///     error::Error, MainDevice, MainDeviceConfig, PduStorage, Timeouts, std::ethercat_now
    /// };
    ///
    /// const MAX_SUBDEVICES: usize = 2;
    /// const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// const MAX_FRAMES: usize = 16;
    /// const MAX_PDI: usize = 8;
    ///
    /// static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();
    ///
    /// let (_tx, _rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");
    ///
    /// let maindevice = MainDevice::new(pdu_loop, Timeouts::default(), MainDeviceConfig::default());
    ///
    /// # async {
    /// let mut group = maindevice
    ///     .init_single_group::<MAX_SUBDEVICES, MAX_PDI>(ethercat_now)
    ///     .await
    ///     .expect("Init");
    ///
    /// for subdevice in group.iter(&maindevice) {
    ///     if subdevice.name() == "EL3004" {
    ///         log::info!("Found EL3004. Configuring...");
    ///
    ///         subdevice.sdo_write(0x1c12, 0, 0u8).await?;
    ///         subdevice.sdo_write(0x1c13, 0, 0u8).await?;
    ///
    ///         subdevice.sdo_write(0x1c13, 1, 0x1a00u16).await?;
    ///         subdevice.sdo_write(0x1c13, 2, 0x1a02u16).await?;
    ///         subdevice.sdo_write(0x1c13, 3, 0x1a04u16).await?;
    ///         subdevice.sdo_write(0x1c13, 4, 0x1a06u16).await?;
    ///         subdevice.sdo_write(0x1c13, 0, 4u8).await?;
    ///     }
    /// }
    ///
    /// let mut group = group.into_safe_op(&maindevice).await.expect("PRE-OP -> SAFE-OP");
    /// # Ok::<(), ethercrab::error::Error>(())
    /// # };
    /// ```
    // 快速创建一个包含所有已发现从站设备的单一组
    pub async fn init_single_group<const MAX_SUBDEVICES: usize, const MAX_PDI: usize>(
        &self,
        now: impl Fn() -> u64 + Copy,
    ) -> Result<SubDeviceGroup<MAX_SUBDEVICES, MAX_PDI, subdevice_group::PreOp>, Error> {
        self.init::<MAX_SUBDEVICES, _>(now, |group, _subdevice| Ok(group))
            .await
    }

    // BRD 0x0000
    /// Count the number of SubDevices on the network.
    async fn count_subdevices(&self) -> Result<u16, Error> {
        Command::brd(RegisterAddress::Type.into())
            //带有泛型参数的函数或结构体在使用时，当类型无法推断时，必须显式指定具体类型
            .receive_wkc::<u8>(self) // 指定读取长度为 1 字节(u8)，对应 Type 寄存器的大小
            // 必须指定 u8 因为返回值不携带类型信息
            .await
    }

    /// Get the number of discovered SubDevices in the EtherCAT network.
    ///
    /// As [`init`](crate::MainDevice::init) runs SubDevice autodetection, it must be called before this
    /// method to get an accurate count.
    pub fn num_subdevices(&self) -> usize {
        usize::from(self.num_subdevices.load(Ordering::Relaxed))
    }

    /// Get the configured address of the designated DC reference subdevice.
    pub(crate) fn dc_ref_address(&self) -> Option<u16> {
        let addr = self.dc_reference_configured_address.load(Ordering::Relaxed);

        if addr > 0 { Some(addr) } else { None }
    }

    /// Wait for all SubDevices on the network to reach a given state.
    // 等待所有从站切换到指定状态，如果出现故障则打印错误码，返回
    pub async fn wait_for_state(&self, desired_state: SubDeviceState) -> Result<(), Error> {
        let num_subdevices = self.num_subdevices.load(Ordering::Relaxed);

        async {
            loop {
                // BRD 0x0130 读取状态
                let status = Command::brd(RegisterAddress::AlStatus.into())
                    .with_wkc(num_subdevices)
                    .receive::<AlControl>(self)
                    .await?;

                fmt::trace!("Global AL status {:?}", status);

                // 如果状态切换出错
                if status.error {
                    fmt::error!(
                        "Error occurred transitioning all SubDevices to {:?}",
                        desired_state,
                    );

                    // FPRD 0x0134 读取每个从站的故障码，打印
                    for subdevice_addr in BASE_SUBDEVICE_ADDRESS
                        ..(BASE_SUBDEVICE_ADDRESS + self.num_subdevices() as u16)
                    {
                        let status =
                            Command::fprd(subdevice_addr, RegisterAddress::AlStatusCode.into())
                                .ignore_wkc()
                                .receive::<AlStatusCode>(self)
                                .await
                                .unwrap_or(AlStatusCode::UnspecifiedError);

                        fmt::error!(
                            "--> SubDevice {:#06x} status code {}",
                            subdevice_addr,
                            status
                        );
                    }

                    return Err(Error::StateTransition);
                }

                if status.state == desired_state {
                    break Ok(());
                }

                // TODO：这个超时时间应该单独一个
                self.timeouts.loop_tick().await;
            }
        }
        .timeout(self.timeouts.state_transition)
        .await
    }

    #[allow(unused)]
    pub(crate) const fn max_frame_data(&self) -> usize {
        self.pdu_loop.max_frame_data()
    }

    // 发送一个只包含一个EtherCAT数据报的帧
    /// Send a single PDU in a frame.
    pub(crate) async fn single_pdu(
        &'sto self,
        command: Command,
        data: impl EtherCrabWireWrite,
        len_override: Option<u16>,
    ) -> Result<ReceivedPdu<'sto>, Error> {
        // 从预分配的帧存储池中找到一个可用的帧，并将其标记为"已创建"状态，以便后续用于发送 PDU 数据
        let mut frame = self.pdu_loop.alloc_frame()?;

        // 在帧中插入一个数据报，没有处理帧空间不足的情况。如果空间不足，就会失败
        // 因为上文获取了新的帧，所以空间一定是足够的，如果不够，是数据报创建有问题。数据报的数据区应该检查总长度
        let handle = frame.push_pdu(command, data, len_override)?;

        // 帧设置为可发送状态Sendable，返回一个 Future，当收到对已发送帧的响应时，该 Future 将被执行。
        // 前文已经写入帧的数据报，以太网帧头，本函数会写入EtherCAT帧头，组帧完成。
        let frame = frame.mark_sendable(
            &self.pdu_loop,
            self.timeouts.pdu,
            self.config.retry_behaviour.retry_count(),
        );

        // 唤醒Tx任务
        self.pdu_loop.wake_sender();

        // 开始轮询 ReceiveFrameFut。成功后，帧状态为 RxProcessing
        // 从返回的帧中获取数据报的报头和WKC，没有检查WKC是否正确
        frame.await?.first_pdu(handle)
    }

    /// Release the [`PduLoop`] storage **without** resetting it.
    ///
    /// To reset the released `PduLoop`, call [`PduLoop::reset`]. This method does not release the
    /// network TX/RX handles created by e.g.
    /// [`PduStorage::try_split`](crate::PduStorage::try_split) to allow a new `MainDevice` to be
    /// created while reusing an existing network interface. To release the TX and RX handles as
    /// well, call [`release_all`](MainDevice::release_all).
    ///
    /// The application should ensure that no EtherCAT data is in flight when this method is called,
    /// i.e. all frames must have either returned to the MainDevice or timed out. If a frame is
    /// received after this method has been called, the [`PduRx`](crate::PduRx) instance handling
    /// that frame will most likely produce an error as the underlying storage for that frame has
    /// been freed.
    ///
    /// # Safety
    ///
    /// Any groups configured using the previous `MainDevice` instance **must not** be used again
    /// with any `MainDevice`s created with the `PduLoop` returned by this method. The group state
    /// (PDI addresses, offsets, sizes, etc) are only valid with the `MainDevice` the group was
    /// initialised with.
    pub unsafe fn release(mut self) -> PduLoop<'sto> {
        // Clear out any in-use frames.
        self.pdu_loop.reset();

        self.pdu_loop
    }

    /// Release the [`PduLoop`] storage and signal the TX/RX handles to release their resources.
    ///
    /// This method is useful to close down a TX/RX loop and the network interface associated with
    /// it.
    ///
    /// To reuse the TX/RX loop and only free the `PduLoop` for reuse in another `MainDevice`
    /// instance, call [`release`](MainDevice::release).
    ///
    /// The application should ensure that no EtherCAT data is in flight when this method is called,
    /// i.e. all frames must have either returned to the MainDevice or timed out. If a frame is
    /// received after this method has been called, the [`PduRx`](crate::PduRx) instance handling
    /// that frame will most likely produce an error as the underlying storage for that frame has
    /// been freed.
    ///
    /// # Safety
    ///
    /// Any groups configured using the previous `MainDevice` instance **must not** be used again
    /// with any `MainDevice`s created with the `PduLoop` returned by this method. The group state
    /// (PDI addresses, offsets, sizes, etc) are only valid with the `MainDevice` the group was
    /// initialised with.
    pub unsafe fn release_all(mut self) -> PduLoop<'sto> {
        self.pdu_loop.reset_all();

        // Wake the TX/RX loop up so it can check for the stop flag
        self.pdu_loop.wake_sender();

        self.pdu_loop
    }
}
