use super::{SubDevice, SubDeviceRef};
use crate::{
    coe::{SdoExpedited, SubIndex},
    eeprom::types::{
        CoeDetails, DefaultMailbox, FmmuUsage, MailboxProtocols, SiiGeneral, SiiOwner, SyncManager,
        SyncManagerEnable, SyncManagerType,
    },
    error::{Error, IgnoreNoCategory, Item},
    fmmu::Fmmu,
    fmt,
    pdi::{PdiOffset, PdiSegment},
    register::RegisterAddress,
    subdevice::types::{Mailbox, MailboxConfig},
    subdevice_state::SubDeviceState,
    sync_manager_channel::{Enable, SM_BASE_ADDRESS, Status, SyncManagerChannel},
};
use core::ops::DerefMut;

/// Configuation from EEPROM methods.
impl<S> SubDeviceRef<'_, S>
where
    S: DerefMut<Target = SubDevice>,
{
    /// First stage configuration (INIT -> PRE-OP).
    ///
    /// Continue configuration by calling
    /// [`configure_fmmus`](crate::SubDeviceGroup::configure_fmmus).
    // 通过EEOROM数据配置邮箱，切换到PreOp，读取对象字典中的0x1c00同步管理器类型，保存邮箱配置
    pub(crate) async fn configure_mailboxes(&mut self) -> Result<(), Error> {
        // Force EEPROM into master mode. Some SubDevices require PDI mode for INIT -> PRE-OP
        // transition. This is mentioned in ETG2010 p. 146 under "Eeprom/@AssignToPd". We'll reset
        // to master mode here, now that the transition is complete.
        // 某些子设备需要 PDI 模式才能进行 INIT -> PRE-OP 转换。这在ETG2010第 146 页的“Eeprom/@AssignToPd”中提到。现在过渡已完成，我们将在此处重置为主站模式。
        // 设置EEPROM访问为主站
        self.set_eeprom_mode(SiiOwner::Master).await?;

        // 读取EEPROM中的SM区,得到SM数组（所有SM）
        // 这里重新创建了一个SubDeviceEeprom
        // 可优化：EEPROM统一读取，之后如果需要更新再更新
        let sync_managers = self.eeprom().sync_managers().await?;

        // Mailboxes must be configured in INIT state
        // 配置SM0和SM1给邮箱。会检查是否支持邮箱，不支持就不配置。生成邮箱的配置信息保存到从站结构体
        self.configure_mailbox_sms(&sync_managers).await?;

        // Some SubDevices must be in PDI EEPROM mode to transition from INIT to PRE-OP. This is
        // mentioned in ETG2010 p. 146 under "Eeprom/@AssignToPd"
        self.set_eeprom_mode(SiiOwner::Pdi).await?;

        fmt::debug!(
            "SubDevice {:#06x} mailbox SMs configured. Transitioning to PRE-OP",
            self.configured_address
        );

        // 切换到PreOp
        self.request_subdevice_state(SubDeviceState::PreOp).await?;

        self.set_eeprom_mode(SiiOwner::Master).await?;

        Ok(())
    }

    // 读取从站PDO配置后，设置FMMU，得到从站PDO在PDI中的地址范围
    /// Second state configuration (PRE-OP -> SAFE-OP).
    ///
    /// PDOs must be configured in the PRE-OP state.
    pub(crate) async fn configure_fmmus(
        &mut self,
        mut global_offset: PdiOffset, // 当前从站PDO在PDI中的起始地址，也就是逻辑地址
        group_start_address: u32,     // 组的PDI起始地址
        direction: PdoDirection,
    ) -> Result<PdiOffset, Error> //  返回更新后的  global_offset
    {
        let eeprom = self.eeprom();

        // 读取EEPROM中的SM区,得到SM数组
        // TODO：需要优化掉，再次重复读取
        let sync_managers = eeprom.sync_managers().await?;
        // 从EEPROM中读取所有FMMU的值
        let fmmu_usage = eeprom.fmmus().await?;

        // 读取从站状态
        let state = self.state().await?;

        // 检查从站是否在PreOp状态，不是就报错
        // TODO 这个检查可以放到周期帧中，定期检查从站状态
        if state != SubDeviceState::PreOp {
            fmt::error!(
                "SubDevice {:#06x} is in invalid state {}. Expected {}",
                self.configured_address,
                state,
                SubDeviceState::PreOp
            );

            // 错误的枚举InvalidState中还能继续包含成员变量
            return Err(Error::InvalidState {
                expected: SubDeviceState::PreOp,
                actual: state,
                configured_address: self.configured_address,
            });
        }

        let has_coe = self.state.config.mailbox.has_coe;

        fmt::debug!(
            "SubDevice {:#06x} has CoE: {:?}",
            self.configured_address,
            has_coe
        );

        // 在运行这个函数之前，用户在用户的程序中已经通过SDO配置了PDO，因此可以根据PDO配置，设置FMMU
        // 发送配置PDO 的命令，修改 global_offset
        // TODO 这里可能有问题：如果从站支持可变PDO，则不能通过EEPROM信息配置FMMU，因为用户可能通过CoE或者SoE动态配置PDO
        let range = if has_coe {
            // 通过读取CoE对象字典，配置PDO，返回PDO在PDI的地址范围
            // TODO 优化：用户配置PDO时，就可以知道IO配置。因此此函数中不需要读取PDO配置，直接根据FMMU配置即可
            self.configure_pdos_coe(&sync_managers, &fmmu_usage, direction, &mut global_offset)
                .await?
        } else {
            // 通过读取EEPROM，配置PDO，返回PDO在PDI的地址范围
            self.configure_pdos_eeprom(&sync_managers, direction, &mut global_offset)
                .await?
        };

        // 计算PDO范围
        // 给从站结构体的config.io赋值
        match direction {
            PdoDirection::MasterRead => {
                self.state.config.io.input = PdiSegment {
                    bytes: (range.bytes.start - group_start_address as usize)
                        ..(range.bytes.end - group_start_address as usize),
                };
            }
            PdoDirection::MasterWrite => {
                self.state.config.io.output = PdiSegment {
                    bytes: (range.bytes.start - group_start_address as usize)
                        ..(range.bytes.end - group_start_address as usize),
                };
            }
        };

        fmt::debug!(
            "SubDevice {:#06x} PDI inputs: {:?} ({} bytes), outputs: {:?} ({} bytes)",
            self.configured_address,
            self.state.config.io.input,
            self.state.config.io.input.len(),
            self.state.config.io.output,
            self.state.config.io.output.len(),
        );

        Ok(global_offset)
    }

    // 根据传入的SM索引和EEPROM的数据，设置对应SM寄存器
    async fn write_sm_config(
        &self,
        sync_manager_index: u8,
        sync_manager: &SyncManager,
        length_bytes: u16, // // Rx/Tx PDO 的总长度，单位：字节
    ) -> Result<SyncManagerChannel, Error> {
        // 从EEPROM数据生成SM寄存器的值
        let sm_config = SyncManagerChannel {
            physical_start_address: sync_manager.start_addr,
            // Bit length, rounded up to the nearest byte
            length_bytes,
            control: sync_manager.control,
            status: Status::default(),
            enable: Enable {
                enable: sync_manager.enable.contains(SyncManagerEnable::ENABLE),
                ..Enable::default()
            },
        };

        // FPWR 0x0800+8*sync_manager_index
        self.write(RegisterAddress::sync_manager(sync_manager_index))
            .send(self.maindevice, &sm_config)
            .await?;

        fmt::debug!(
            "SubDevice {:#06x} SM{}: {}",
            self.configured_address,
            sync_manager_index,
            sm_config
        );
        fmt::trace!("{:#?}", sm_config);

        Ok(sm_config)
    }

    // 配置SM0和SM1给邮箱。会检查是否支持邮箱，不支持就不配置。生成邮箱的配置信息保存到从站
    // 这里重复读取了general Category
    // TODO：16个SM通道，是否可能其他通道也作为邮箱SM？
    /// Configure SM0 and SM1 for mailbox communication.
    async fn configure_mailbox_sms(&mut self, sync_managers: &[SyncManager]) -> Result<(), Error> {
        let eeprom = self.eeprom();

        // Read default mailbox configuration from SubDevice information area
        let mailbox_config = eeprom
            .mailbox_config() // 读取EEPROM中的标准邮箱区域
            .await
            .ignore_no_category()?
            .unwrap_or_else(|| {
                fmt::debug!(
                    "{:#06x} has no EEPROM mailbox config, using default",
                    self.configured_address()
                );

                DefaultMailbox::default()
            });

        let general = eeprom
            .general() // 读取general Category
            .await
            .ignore_no_category()?
            .unwrap_or_else(|| {
                fmt::debug!(
                    "{:#06x} has no EEPROM general category, using default",
                    self.configured_address()
                );

                SiiGeneral::default()
            });

        fmt::trace!(
            "SubDevice {:#06x} Mailbox configuration: {:#?}",
            self.configured_address,
            mailbox_config
        );

        // 确认支持邮箱，不支持则退出
        if !mailbox_config.has_mailbox() {
            fmt::trace!(
                "SubDevice {:#06x} has no valid mailbox configuration",
                self.configured_address
            );

            return Ok(());
        }

        let mut read_mailbox = None;
        let mut write_mailbox = None;

        for (sync_manager_index, sync_manager) in sync_managers.iter().enumerate() {
            let sync_manager_index = sync_manager_index as u8;

            // Mailboxes are configured in INIT state
            // 确定 SyncManager 的使用类型；若 usage_type 未知，通过其他字段推断类型
            match sync_manager.usage_type() {
                SyncManagerType::MailboxWrite => {
                    // 根据传入的SM索引和EEPROM的数据，设置 邮箱 写 对应的SM寄存器
                    self.write_sm_config(
                        sync_manager_index,
                        sync_manager,
                        mailbox_config.subdevice_receive_size,
                    )
                    .await?;

                    write_mailbox = Some(Mailbox {
                        address: sync_manager.start_addr,
                        len: mailbox_config.subdevice_receive_size,
                        sync_manager: sync_manager_index,
                    });
                }
                SyncManagerType::MailboxRead => {
                    // 根据传入的SM索引和EEPROM的数据，设置 邮箱 读 对应的SM寄存器
                    self.write_sm_config(
                        sync_manager_index,
                        sync_manager,
                        mailbox_config.subdevice_send_size,
                    )
                    .await?;

                    read_mailbox = Some(Mailbox {
                        address: sync_manager.start_addr,
                        len: mailbox_config.subdevice_send_size,
                        sync_manager: sync_manager_index,
                    });
                }
                _ => continue,
            }
        }

        // 给从站结构中的 config 的邮箱赋值
        self.state.config.mailbox = MailboxConfig {
            read: read_mailbox,
            write: write_mailbox,
            supported_protocols: mailbox_config.supported_protocols,
            has_coe: mailbox_config
                .supported_protocols
                .contains(MailboxProtocols::COE)
                && read_mailbox.is_some_and(|mbox| mbox.len > 0),
            complete_access: general
                .coe_details
                .contains(CoeDetails::ENABLE_COMPLETE_ACCESS),
        };

        Ok(())
    }

    // 通过读取CoE对象字典，配置PDO，返回PDO在PDI的地址范围
    /// Configure PDOs from CoE registers.
    async fn configure_pdos_coe(
        &self,
        sync_managers: &[SyncManager],
        fmmu_usage: &[FmmuUsage],
        direction: PdoDirection,
        global_offset: &mut PdiOffset, // 当前从站PDO在PDI中的起始地址，也就是逻辑地址
    ) -> Result<PdiSegment, Error> {
        if !self.state.config.mailbox.has_coe {
            fmt::warn!("Invariant: attempting to configure PDOs from COE with no SOE support");
        }

        // 根据 PDO（过程数据对象）的方向，返回对应的同步管理器类型和 FMMU（现场总线内存管理单元）使用类型
        let (desired_sm_type, desired_fmmu_type) = direction.filter_terms();

        // TODO
        // NOTE: Commented out because this causes a timeout on various SubDevices, possibly due
        // to querying 0x1c00 after we enter PRE-OP but I'm unsure. See
        // <https://github.com/ethercrab-rs/ethercrab/issues/49>. Complete access also causes the
        // same issue.
        // // ETG1000.6 Table 67 – CoE Communication Area
        // let num_sms = self
        //     .sdo_read::<u8>(SM_TYPE_ADDRESS, SubIndex::Index(0))
        //     .await?;

        // 记录当前从站PDO在PDI中的起始地址
        let start_offset = *global_offset;
        // let mut total_bit_len = 0;

        for (sync_manager_index, sync_manager) in sync_managers.iter().enumerate() {
            let sync_manager_index = sync_manager_index as u8;

            // 0x1c10+SM索引
            // 子索引0为分配的PDO数量
            let sm_address = SM_BASE_ADDRESS + u16::from(sync_manager_index);

            // 从SM切片获取对应index的SM
            // TODO：为什么新版本删除这个？
            // let sync_manager =
            //     sync_managers
            //         .get(usize::from(sync_manager_index))
            //         .ok_or(Error::NotFound {
            //             item: Item::SyncManager,
            //             index: Some(usize::from(sync_manager_index)),
            //         })?;

            if sync_manager.usage_type() != desired_sm_type {
                continue;
            }

            // Total number of PDO assignments for this sync manager
            // 读取这个SM通道分配的PDO数量（即子索引0保存的数字）
            let num_sm_assignments = self
                // SDO快速传输上传
                .sdo_read_expedited::<u8>(sm_address, SubIndex::Index(0))
                .await?;

            fmt::trace!(
                "SDO sync manager {}  {:#06x} {:?}, sub indices: {}",
                sync_manager_index,
                sm_address,
                sync_manager.usage_type(),
                num_sm_assignments
            );

            // Rx/Tx PDO 的总bit长度
            let mut sm_bit_len = 0u16;

            for i in 1..=num_sm_assignments {
                // 读取（0x1c10+SM索引）每个子索引的值。即PDO映射对象索引
                let pdo = self
                    .sdo_read_expedited::<u16>(sm_address, SubIndex::Index(i))
                    .await?;
                // 读取PDO映射对象索引包含的Entry数量（即子索引0保存的数字）
                let num_mappings = self
                    .sdo_read_expedited::<u8>(pdo, SubIndex::Index(0))
                    .await?;

                fmt::trace!(
                    "--> {:#04x} data: {:#06x} ({} mappings):",
                    i,
                    pdo,
                    num_mappings
                );

                // 读取PDO映射对象中每个Entry，累计Entry的bit长度
                for i in 1..=num_mappings {
                    /// Defined in ETG1000.6 Table 74/Table 75 Receive PDO Mapping.
                    ///
                    /// Note that this struct order is opposite to the specification as the data is
                    /// big-endian in EEPROM, but little endian on the wire.
                    // PDO映射对象Entry在EEOROM中是大端，在网络中是小端
                    #[derive(ethercrab_wire::EtherCrabWireRead)]
                    #[wire(bytes = 4)]
                    struct Mapping {
                        #[wire(bytes = 1)]
                        mapping_bit_len: u8,
                        #[wire(bytes = 1)]
                        sub_index: u8,
                        #[wire(bytes = 2)]
                        index: u16,
                    }

                    impl SdoExpedited for Mapping {}

                    // 读取PDO映射对象中每个Entry
                    let Mapping {
                        index,
                        sub_index,
                        mapping_bit_len,
                    } = self
                        .sdo_read_expedited::<Mapping>(pdo, SubIndex::Index(i))
                        .await?;

                    fmt::trace!(
                        "----> index {:#06x}, sub index {}, bit length {}",
                        index,
                        sub_index,
                        mapping_bit_len,
                    );

                    // 累计Entry的bit长度
                    sm_bit_len += u16::from(mapping_bit_len);
                }
            }

            fmt::trace!(
                "----= total SM bit length {} ({} bytes)",
                sm_bit_len,
                (sm_bit_len + 7) / 8
            );

            // 根据传入的SM索引和EEPROM的数据，设置对应SM寄存器
            let sm_config = self
                .write_sm_config(sync_manager_index, sync_manager, (sm_bit_len + 7) / 8)
                .await?;

            if sm_bit_len > 0 {
                // 查找合适的 FMMU 索引
                let fmmu_index = fmmu_usage //FmmuUsage 枚举值的切片，记录了所有 FMMU 的使用类型
                    .iter()
                    // 找到符合期望FMMU类型的FMMU索引
                    .position(|usage| *usage == desired_fmmu_type)
                    .ok_or(Error::NotFound {
                        item: Item::Fmmu,
                        index: None,
                    })?;

                // 设置FMMU，将PDI地址（逻辑地址）累加bit数转换的字节数
                self.write_fmmu_config(
                    sm_bit_len,
                    fmmu_index,
                    global_offset, // 当前从站PDO在PDI中的起始地址，也就是逻辑地址
                    desired_sm_type,
                    &sm_config,
                )
                .await?;
            }

            // total_bit_len += sm_bit_len;
        }

        // 返回当前从站PDO在PDI中的起始地址（逻辑地址）范围
        Ok(PdiSegment {
            // bit_len: total_bit_len.into(),
            bytes: start_offset.up_to(*global_offset),
        })
    }

    // 设置FMMU，将PDI地址（逻辑地址）累加bit数转换的字节数
    async fn write_fmmu_config(
        &self,
        sm_bit_len: u16, // Rx/Tx PDO 的总bit长度
        fmmu_index: usize,
        global_offset: &mut PdiOffset, // 当前从站PDO在PDI中的起始地址，也就是逻辑地址。函数调用时会累加这个从站的PDO长度
        desired_sm_type: SyncManagerType,
        sm_config: &SyncManagerChannel,
    ) -> Result<(), Error> {
        // Multiple SMs may use the same FMMU, so we'll read the existing config from the SubDevice
        // 多个 SM 可能使用相同的 FMMU，因此我们将从 SubDevice 读取现有配置
        // 读取FMMU配置
        // TODO：这里重复读取
        let mut fmmu_config = self
            .read(RegisterAddress::fmmu(fmmu_index as u8))
            .receive::<Fmmu>(self.maindevice)
            .await?;

        // We can use the enable flag as a sentinel for existing config because EtherCrab inits
        // FMMUs to all zeroes on startup.
        let fmmu_config = if fmmu_config.enable {
            // 如果已经设置了FMMU，则只修改长度
            fmmu_config.length_bytes += sm_config.length_bytes;

            fmmu_config
        } else {
            // 还没设置FMMU，就生成FMMU配置
            Fmmu {
                logical_start_address: global_offset.start_address,
                length_bytes: sm_config.length_bytes,
                // Mapping into PDI is byte-aligned until/if we support bit-oriented SubDevices
                logical_start_bit: 0,
                // Always byte-aligned
                logical_end_bit: 7,
                physical_start_address: sm_config.physical_start_address,
                physical_start_bit: 0x0,
                read_enable: desired_sm_type == SyncManagerType::ProcessDataRead,
                write_enable: desired_sm_type == SyncManagerType::ProcessDataWrite,
                enable: true,
            }
        };

        // 写入FMMU设置
        self.write(RegisterAddress::fmmu(fmmu_index as u8))
            .send(self.maindevice, &fmmu_config)
            .await?;

        fmt::debug!(
            "SubDevice {:#06x} FMMU{}: {}",
            self.configured_address,
            fmmu_index,
            fmmu_config
        );

        // 将PDI地址累加bit数转换的字节数
        *global_offset = global_offset.increment_byte_aligned(sm_bit_len);

        Ok(())
    }

    // 通过读取EEPROM，配置PDO，返回PDO在PDI的地址范围
    /// Configure PDOs from EEPROM
    async fn configure_pdos_eeprom(
        &self,
        sync_managers: &[SyncManager],
        direction: PdoDirection,
        offset: &mut PdiOffset,
    ) -> Result<PdiSegment, Error> {
        let eeprom = self.eeprom();

        let pdos = match direction {
            PdoDirection::MasterRead => {
                // 读取EEPROM中TxPDO的所有PDO及其Entry
                let read_pdos = eeprom.maindevice_read_pdos().await?;

                fmt::trace!("SubDevice inputs PDOs {:#?}", read_pdos);

                read_pdos
            }
            PdoDirection::MasterWrite => {
                // 读取EEPROM中RxPDO的所有PDO及其Entry
                let write_pdos = eeprom.maindevice_write_pdos().await?;

                fmt::trace!("SubDevice outputs PDOs {:#?}", write_pdos);

                write_pdos
            }
        };

        // 读取FMMU ex category
        let fmmu_sm_mappings = eeprom.fmmu_mappings().await?;

        let start_offset = *offset;
        // let mut total_bit_len = 0;

        // 根据 PDO（过程数据对象）的方向，返回对应的同步管理器类型和 FMMU（现场总线内存管理单元）使用类型
        let (sm_type, _fmmu_type) = direction.filter_terms();

        for (sync_manager_index, sync_manager) in sync_managers
            .iter()
            .enumerate()
            .filter(|(_idx, sm)| sm.usage_type() == sm_type)
        {
            let sync_manager_index = sync_manager_index as u8;

            let bit_len = pdos
                .iter()
                .filter(|pdo| pdo.sync_manager == sync_manager_index)
                .map(|pdo| pdo.bit_len)
                .sum();

            // total_bit_len += bit_len;

            // Look for FMMU index using FMMU_EX section in EEPROM. If it's empty, default
            // to looking through FMMU usage list and picking out the appropriate kind
            // (Inputs, Outputs)
            let fmmu_index = fmmu_sm_mappings
                .iter()
                .find(|fmmu| fmmu.sync_manager == sync_manager_index)
                .map(|fmmu| fmmu.sync_manager)
                .unwrap_or_else(|| {
                    fmt::trace!(
                        "Could not find FMMU for PDO SM{} in EEPROM, using SM index to pick FMMU instead",
                        sync_manager_index,
                    );

                    sync_manager_index
                });

            // 根据传入的SM索引和EEPROM的数据，设置对应SM寄存器
            let sm_config = self
                .write_sm_config(sync_manager_index, sync_manager, (bit_len + 7) / 8)
                .await?;

            // 设置FMMU，将PDI地址累加bit数转换的字节数
            self.write_fmmu_config(
                bit_len,
                usize::from(fmmu_index),
                offset,
                sm_type,
                &sm_config,
            )
            .await?;
        }

        Ok(PdiSegment {
            // bit_len: total_bit_len.into(),
            bytes: start_offset.up_to(*offset),
        })
    }
}

// 配置FMMU时用到的PDO方向
#[derive(Copy, Clone)]
pub enum PdoDirection {
    MasterRead,
    MasterWrite,
}

impl PdoDirection {
    // 根据 PDO（过程数据对象）的方向，返回对应的同步管理器类型和 FMMU（现场总线内存管理单元）使用类型
    fn filter_terms(self) -> (SyncManagerType, FmmuUsage) {
        match self {
            PdoDirection::MasterRead => (SyncManagerType::ProcessDataRead, FmmuUsage::Inputs),
            PdoDirection::MasterWrite => (SyncManagerType::ProcessDataWrite, FmmuUsage::Outputs),
        }
    }
}
