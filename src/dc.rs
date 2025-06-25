//! Distributed Clocks (DC).
//!
//! SubDevice propagation time measurement and DC offset calculation documentation can be found in
//! [Hardware Data Sheet Section
//! I](https://download.beckhoff.com/download/document/io/ethercat-development-products/ethercat_esc_datasheet_sec1_technology_2i3.pdf).

use core::num::NonZeroU16;

use crate::{
    MainDevice, SubDeviceRef,
    command::Command,
    error::Error,
    fmt,
    register::RegisterAddress,
    subdevice::{SubDevice, ports::Topology},
};

// 获取每个DC从站的端口接收时间
/// Send a broadcast to all SubDevices to latch in DC receive time, then store it on the SubDevice
/// structs.
async fn latch_dc_times(
    maindevice: &MainDevice<'_>,
    subdevices: &mut [SubDevice],
) -> Result<(), Error> {
    // 过滤出支持DC的从站数量
    let num_subdevices_with_dc: usize = subdevices
        .iter()
        .filter(|subdevice| subdevice.dc_support().any())
        .count();

    // BWR 0x0900 锁存端口时间，会检查WKC
    // Latch receive times into all ports of all SubDevices.
    Command::bwr(RegisterAddress::DcTimePort0.into())
        .with_wkc(num_subdevices_with_dc as u16)
        .send(maindevice, 0u32)
        .await?;

    // Read receive times for all SubDevices and store on SubDevice structs
    for subdevice in subdevices
        .iter_mut()
        .filter(|subdevice| subdevice.dc_support().any())
    {
        let mut subdevice =
            SubDeviceRef::new(maindevice, subdevice.configured_address(), subdevice);

        // FPRD 0x0918 EtherCAT帧处理单元时间，实际上就是端口0时间。忽略WKC
        // ECAT 处理单元接收到的帧开始时的本地时间（前导码的第一位），包含对寄存器 0x​​0900 的写访问
        // 注意：例如，如果端口 0 处于打开状态，则该寄存器将端口 0 的接收时间反映为 64 位值。
        // 对寄存器 0x​​0900 的任何有效 EtherCAT 写访问都会触发锁存，而不仅仅是寄存器 0x​​0900 的 BWR/FPWR 命令。
        let dc_receive_time = subdevice
            .read(RegisterAddress::DcReceiveTime)
            .ignore_wkc()
            .receive::<u64>(maindevice)
            .await?;

        // FPRD 0x0900 读取端口时间
        let [time_p0, time_p1, time_p2, time_p3] = subdevice
            .read(RegisterAddress::DcTimePort0)
            .receive::<[u32; 4]>(maindevice)
            .await
            .inspect_err(|&e| {
                fmt::error!(
                    "Failed to read DC times for SubDevice {:#06x}: {}",
                    subdevice.configured_address(),
                    e
                );
            })?;

        subdevice.dc_receive_time = dc_receive_time;

        fmt::trace!(
            "SubDevice {:#06x} DC receive time {} ns",
            subdevice.configured_address(),
            subdevice.dc_receive_time
        );

        subdevice
            .ports
            .set_receive_times(time_p0, time_p3, time_p1, time_p2);
    }

    Ok(())
}

// 设置时钟偏移和传播延迟
/// Write DC system time offset and propagation delay to the SubDevice memory.
async fn write_dc_parameters(
    maindevice: &MainDevice<'_>,
    subdevice: &SubDevice,
    dc_system_time: u64,
    now_nanos: u64,
) -> Result<(), Error> {
    // 计算时钟偏移
    let system_time_offset = -(subdevice.dc_receive_time as i64) + now_nanos as i64;

    fmt::trace!(
        "Setting SubDevice {:#06x} system time offset to {} ns (system time is {} ns, DC receive time is {}, now is {} ns)",
        subdevice.configured_address(),
        system_time_offset,
        dc_system_time,
        subdevice.dc_receive_time,
        now_nanos
    );

    // FPWR 0x0920 时钟偏移，不检查WKC
    Command::fpwr(
        subdevice.configured_address(),
        RegisterAddress::DcSystemTimeOffset.into(),
    )
    .ignore_wkc()
    .send(maindevice, system_time_offset)
    .await?;

    // FPWR 0x0928 传播延迟补偿，不检查WKC
    Command::fpwr(
        subdevice.configured_address(),
        RegisterAddress::DcSystemTimeTransmissionDelay.into(),
    )
    .ignore_wkc()
    .send(maindevice, subdevice.propagation_delay)
    .await?;

    Ok(())
}

// 查找与当前从站连接的前一个从站（父节点）
/// Find the SubDevice parent device in the list of SubDevices before it in the linear topology.
///
/// # Implementation detail
///
/// ## Special case: parent is a branch
///
/// Given the following topology, we want to find the parent device of the LAN9252 (index 5).
/// According to EtherCAT topology rules, the parent device is the EK1100 (index 1), however the
/// SubDevices are discovered in network order (denoted here by `#N`), meaning there is some
/// special behaviour to consider when traversing backwards through the previous nodes before the
/// LAN9252.
///
/// ```text
/// ┌─────────────┐
/// │   Master    │
/// │     #0      │
/// │             │
/// └─────────────┘
///        ▲
///        ║
///        ╚0▶┌────────────┐      ┌────────────┐      ┌────────────┐──┐
///           │   EK1100   │      │   EL2004   │      │   EL3004   │  │
///           │     #1     │◀3══0▶│     #3     │◀3══0▶│     #4     │  │
///           │            │      │            │      │            │  │
///        ╔1▶└────────────┘      └────────────┘      └────────────┘◀─┘
///        ║
///        ║  ┌────────────┐──┐
///        ║  │  LAN9252   │  │
///        ╚0▶│     #5     │  │
///           │            │  │
///           └────────────┘◀─┘
/// ```
fn find_subdevice_parent(
    parents: &[SubDevice],
    subdevice: &SubDevice,
) -> Result<Option<u16>, Error> {
    // No parent if we're first in the network, e.g. the EK1100 in the diagram above
    // 第一个从站没有父节点
    if parents.is_empty() {
        return Ok(None);
    }

    // iter().rev() 会创建一个反向迭代器 parents_it，从后往前遍历 parents 切片
    let mut parents_it = parents.iter().rev();

    // 尝试从反向迭代器中获取下一个元素（即最后一个父设备）
    if let Some(parent) = parents_it.next() {
        // If the previous parent in the chain is a leaf node in the tree, we need to continue
        // iterating to find the common parent, i.e. the split point.
        //
        // Using the doc example above, say we're at #5, the previous device is #4 which is a
        // `LineEnd`. This means we traverse backwards until we find a `Fork` (the EK1100) and use
        // that as the parent.
        // 如果上一个父节点是树中的叶节点，需要继续迭代以找到分叉点。
        // 文档示例，假设我们现在位于 #5，上一个设备是 #4，它是一个 `LineEnd`。需要向后遍历，直到找到一个 `Fork`（EK1100）并将其用作父节点
        if parent.ports.topology() == Topology::LineEnd {
            // 找到分叉节点或者交叉节点
            let split_point = parents_it
                .find(|subdevice| subdevice.ports.topology().is_junction())
                .ok_or_else(|| {
                    fmt::error!(
                        "Did not find fork parent for SubDevice {:#06x}",
                        subdevice.configured_address()
                    );

                    Error::Topology
                })?;

            Ok(Some(split_point.index))
        }
        // Otherwise the parent is just the previous node in the tree
        // 如果上一个父节点不是树中的叶节点，则它就是父节点
        else {
            Ok(Some(parent.index))
        }
    } else {
        fmt::error!(
            "Did not find parent for SubDevice {:#06x}",
            subdevice.configured_address()
        );

        Err(Error::Topology)
    }
}

// 打印从站设备（SubDevice）端口的调试信息，包括端口拓扑类型、各端口接收时间以及端口间接收时间的差值
fn debug_print_ports(subdevice: &SubDevice) {
    let time_p0 = subdevice.ports.0[0].dc_receive_time;
    let time_p3 = subdevice.ports.0[1].dc_receive_time;
    let time_p1 = subdevice.ports.0[2].dc_receive_time;
    let time_p2 = subdevice.ports.0[3].dc_receive_time;
    // Deltas between port receive times
    // saturating_sub 方法在结果为负数时会返回 0，避免了整数下溢的问题
    let d03 = time_p3.saturating_sub(time_p0);
    let d31 = time_p1.saturating_sub(time_p3);
    let d12 = time_p2.saturating_sub(time_p1);
    // let loop_propagation_time = subdevice.ports.total_propagation_time();
    // let child_delay = subdevice.ports.fork_child_delay();
    fmt::debug!(
        "--> Topology {:?}, {}",
        subdevice.ports.topology(),
        subdevice.ports
    );
    fmt::debug!(
        "--> Receive times {} ns ({} ns) {} ({} ns) {} ({} ns) {} (total {})",
        time_p0,
        if subdevice.ports.0[1].active { d03 } else { 0 },
        // d03,
        time_p3,
        if subdevice.ports.0[2].active { d31 } else { 0 },
        // d31,
        time_p1,
        if subdevice.ports.0[3].active { d12 } else { 0 },
        // d12,
        time_p2,
        // 计算从站4个端口最大和最小接收时间的差值，就是帧在从站之后网络传输的时间
        subdevice.ports.total_propagation_time().unwrap_or(0)
    );
}

// 计算传播延迟
/// Calculate and assign a SubDevice's propagation delay, i.e. the time it takes for a packet to
/// reach it when sent from the MainDevice.
#[deny(clippy::arithmetic_side_effects)]
fn configure_subdevice_offsets(
    subdevice: &mut SubDevice,
    parents: &[SubDevice],
    delay_accum: &mut u32,
) {
    // Just for debug
    debug_print_ports(subdevice);

    // 通过索引匹配出父从站
    let parent = subdevice
        .parent_index
        .and_then(|parent_index| parents.iter().find(|parent| parent.index == parent_index));

    let Some(parent) = parent else {
        return;
    };

    // 找到在当前从站里分配给指定从站的端口
    let parent_port = fmt::unwrap_opt!(
        parent.ports.port_assigned_to(subdevice),
        "Parent assigned port"
    );

    // The port the MainDevice is connected to on this SubDevice. Must always be entry port.
    let this_port = subdevice.ports.entry_port();

    fmt::debug!(
        "--> Parent ({:?}) {} port {} assigned to {} port {} (SubDevice is child of parent: {:?})",
        parent.ports.topology(),
        parent.name(),
        parent_port.number,
        subdevice.name(),
        this_port.number,
        subdevice.is_child_of(parent)
    );

    let parent_prop_time = parent.ports.total_propagation_time().unwrap_or(0);
    let this_prop_time = subdevice.ports.total_propagation_time().unwrap_or(0);

    let parent_delta = parent_prop_time.saturating_sub(this_prop_time);

    fmt::debug!(
        "--> Parent propagation time {}, my prop. time {} delta {}",
        parent_prop_time,
        this_prop_time,
        parent_delta
    );

    // Divide by two here as the propagation times are for a loop, i.e. out _and back again_. We
    // only want one side of the loop.
    let propagation_delay = match parent.ports.topology() {
        Topology::Passthrough => parent_delta / 2,
        Topology::Fork => {
            if subdevice.is_child_of(parent) {
                let children_loop_time = parent.ports.propagation_time_to(parent_port).unwrap_or(0);

                children_loop_time.saturating_sub(this_prop_time) / 2
            } else {
                parent_delta / 2
            }
        }
        Topology::Cross => {
            if subdevice.is_child_of(parent) {
                let children_loop_time = parent.ports.intermediate_propagation_time_to(parent_port);

                children_loop_time.saturating_sub(this_prop_time) / 2
            } else {
                parent_prop_time.saturating_sub(*delay_accum)
            }
        }
        // A parent of any device cannot have a `LineEnd` topology as it will always have at
        // least 2 ports open (1 for comms to master, 1 to current SubDevice)
        Topology::LineEnd => 0,
    };

    *delay_accum = delay_accum.saturating_add(propagation_delay);

    fmt::debug!(
        "--> Propagation delay {} (delta {}) ns",
        delay_accum,
        propagation_delay,
    );

    subdevice.propagation_delay = *delay_accum;
}

// 计算传播延迟
// 没有考虑网线接反的情况？
/// Assign parent/child relationships and compute propagation delays for all SubDevices.
#[deny(clippy::arithmetic_side_effects)]
fn assign_parent_relationships(subdevices: &mut [SubDevice]) -> Result<(), Error> {
    // 用于累积从站设备的传播延迟
    let mut delay_accum = 0;

    for i in 0..subdevices.len() {
        // 将 subdevices 切片在索引 i 处分割为两部分，parents 包含索引 i 之前的所有从站设备，rest 包含索引 i 及其之后的所有从站设备
        let (parents, rest) = subdevices.split_at_mut(i);
        // 获取 rest 切片的第一个元素（即当前正在处理的从站设备）
        let subdevice = rest.first_mut().ok_or(Error::Internal)?;

        // 查找与当前从站连接的前一个从站（父节点）
        subdevice.parent_index = find_subdevice_parent(parents, subdevice)?;

        fmt::debug!(
            "SubDevice {:#06x} {} DC support {:?}",
            subdevice.configured_address(),
            subdevice.name,
            subdevice.dc_support()
        );

        // If this SubDevice has a parent, find it, then assign the parent's next open port to this
        // SubDevice, establishing the relationship between them by setting the SubDevice index on
        // the parent port.
        // 如果此子设备有父设备，则找到它，并将父设备的下一个开放端口分配给此子设备，并通过在父端口上设置子设备索引来建立它们之间的关系。
        if let Some(parent_idx) = subdevice.parent_index {
            // 在parents切片中找到当前从站的父从站
            let parent =
                fmt::unwrap_opt!(parents.iter_mut().find(|parent| parent.index == parent_idx));

            fmt::unwrap_opt!(
                // NonZeroU16 表示非零的 u16 值，若 subdevice.index 为 0 则返回 None，否则返回 Some(NonZeroU16)
                NonZeroU16::new(subdevice.index)
                    // 父从站入口端口的下一个开放端口和当前从站链接
                    .and_then(|index| parent.ports.assign_next_downstream_port(index)),
                "no free ports on parent"
            );
        }

        // 针对DC从站，计算传播延迟
        if subdevice.dc_support().any() {
            configure_subdevice_offsets(subdevice, parents, &mut delay_accum);
        } else {
            fmt::trace!(
                "--> Skipping DC config for slSubDeviceave {:#06x}: DC not supported",
                subdevice.configured_address()
            );
        }
    }

    // Extremely crude output to generate test cases with. `rustfmt` makes it look nice in the test.
    #[cfg(feature = "std")] // 只有在启用 std 特性时，这部分代码才会被编译
    // 检查 PRINT_TEST_CASE 环境变量是否被设置。若设置了该环境变量，代码块内的逻辑会被执行。
    if option_env!("PRINT_TEST_CASE").is_some() {
        // 打印从站设备配置信息
        for subdevice in subdevices.iter() {
            let p = subdevice.ports.0;

            let ports = format!(
                "{}, {}, {}, {}, {}, {}, {}, {}",
                p[0].active,
                p[0].dc_receive_time,
                p[1].active,
                p[1].dc_receive_time,
                p[2].active,
                p[2].dc_receive_time,
                p[3].active,
                p[3].dc_receive_time,
            );

            println!(
                r#"SubDevice {{
                index: {},
                configured_address: {:#06x},
                name: "{}".try_into().unwrap(),
                ports: ports(
                    {}
                ),
                dc_receive_time: {},
                ..defaults.clone()
            }},"#,
                subdevice.index,
                subdevice.configured_address(),
                subdevice.name,
                ports,
                subdevice.dc_receive_time,
            );
        }

        // 打印从站设备拓扑信息
        for subdevice in subdevices.iter() {
            println!(
                "// Index {}: {} ({:?})",
                subdevice.index,
                subdevice.name(),
                subdevice.ports.topology()
            );

            print!("(");

            print!("[");
            for p in subdevice.ports.0 {
                print!("{:?}, ", p.downstream_to);
            }
            print!("],");

            print!(
                "{:?}, {}, ",
                subdevice.parent_index, subdevice.propagation_delay
            );

            print!("),");

            println!();
        }
    }

    Ok(())
}

/// Configure distributed clocks.
///
/// This method walks through the discovered list of devices and sets the system time offset and
/// transmission delay of each device.
pub(crate) async fn configure_dc<'subdevices>(
    maindevice: &MainDevice<'_>,
    subdevices: &'subdevices mut [SubDevice],
    now: impl Fn() -> u64,
) -> Result<Option<&'subdevices SubDevice>, Error> {
    // 获取每个DC从站的端口接收时间
    // 只测量一次太粗糙了
    latch_dc_times(maindevice, subdevices).await?;

    assign_parent_relationships(subdevices)?;

    // 查找第一个支持 DC 的从站
    // 优化：开发接口允许指定参考时钟
    let first_dc_subdevice = subdevices
        .iter()
        .find(|subdevice| subdevice.dc_support().any());

    if let Some(first_dc_subdevice) = first_dc_subdevice.as_ref() {
        // 获取当前时间（纳秒）
        let now_nanos = now();

        // 对支持DC的从站，设置时钟偏移和传播延迟
        // 非DC从站的行为会如何？
        for subdevice in subdevices.iter().filter(|sl| sl.dc_support().any()) {
            // 设置时钟偏移和传播延迟
            write_dc_parameters(
                maindevice,
                subdevice,
                first_dc_subdevice.dc_receive_time,
                now_nanos,
            )
            .await?;
        }
    } else {
        fmt::debug!("No SubDevices with DC support found");
    }

    fmt::debug!("Distributed clock config complete");

    Ok(first_dc_subdevice)
}

/// Static drift compensation: send `iterations` FRMW frames to synchronise the network with the
/// reference clock in the designated DC SubDevice.
///
/// This will distributed the System Clock to all SubDevices and allow them to set an initial offset
/// and drift compensation against the reference clock. This does not account for drift over time,
/// but that is handled later by sending FRMW frames in the process data cycle.
///
/// The spec recommends sending ~15000 frames, but this number can be higher or lower depending on
/// whether the application is optimised for startup time or initial synchronisation accuracy. The
/// clocks will align more accurately during OP as long as more FRMW frames are sent, so exact
/// alignment at this stage isn't critical.
pub(crate) async fn run_dc_static_sync(
    maindevice: &MainDevice<'_>,
    dc_reference_subdevice: &SubDevice,
    iterations: u32,
) -> Result<(), Error> {
    fmt::debug!(
        "Performing static drift compensation using SubDevice {:#06x} {} as reference. This can take some time...",
        dc_reference_subdevice.configured_address(),
        dc_reference_subdevice.name
    );

    for _ in 0..iterations {
        Command::frmw(
            dc_reference_subdevice.configured_address(),
            RegisterAddress::DcSystemTime.into(),
        )
        .receive_wkc::<u64>(maindevice)
        .await?;
    }

    fmt::debug!("Static drift compensation complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DcSupport,
        subdevice::ports::{Port, Ports, tests::make_ports},
    };

    // A SubDevice in the middle of the chain
    fn ports_passthrough() -> Ports {
        make_ports(true, true, false, false)
    }

    // EK1100 for example, with in/out ports and a bunch of SubDevices connected to it
    fn ports_fork() -> Ports {
        make_ports(true, true, true, false)
    }

    // Last SubDevice in the network
    fn ports_eol() -> Ports {
        make_ports(true, false, false, false)
    }

    // Test for topology including an EK1100 that creates a fork in the tree.
    #[test]
    fn parent_is_ek1100() {
        let subdevice_defaults = SubDevice {
            configured_address: 0x0000,
            ports: Ports::default(),
            name: "Default".try_into().unwrap(),
            ..Default::default()
        };

        let parents = [
            SubDevice {
                configured_address: 0x1000,
                ports: ports_passthrough(),
                name: "LAN9252".try_into().unwrap(),
                index: 0,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x1100,
                ports: ports_fork(),
                name: "EK1100".try_into().unwrap(),
                index: 1,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x2004,
                ports: ports_passthrough(),
                name: "EL2004".try_into().unwrap(),
                index: 2,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x3004,
                ports: ports_eol(),
                name: "EL3004".try_into().unwrap(),
                index: 3,
                ..subdevice_defaults.clone()
            },
        ];

        let me = SubDevice {
            configured_address: 0x9252,
            ports: ports_eol(),
            name: "LAN9252".try_into().unwrap(),
            index: 4,
            ..subdevice_defaults
        };

        let parent_index = find_subdevice_parent(&parents, &me);

        assert_eq!(parent_index.unwrap(), Some(1));
    }

    // Two forks in the tree
    #[test]
    fn two_ek1100() {
        let subdevice_defaults = SubDevice {
            configured_address: 0x0000,
            ports: Ports::default(),
            name: "Default".try_into().unwrap(),
            ..Default::default()
        };

        let parents = [
            SubDevice {
                configured_address: 0x1100,
                ports: ports_fork(),
                name: "EK1100".try_into().unwrap(),
                index: 1,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x2004,
                ports: ports_passthrough(),
                name: "EL2004".try_into().unwrap(),
                index: 2,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x3004,
                ports: ports_eol(),
                name: "EL3004".try_into().unwrap(),
                index: 3,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x1100,
                ports: ports_fork(),
                name: "EK1100_2".try_into().unwrap(),
                index: 4,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x2004,
                ports: ports_passthrough(),
                name: "EL2828".try_into().unwrap(),
                index: 5,
                ..subdevice_defaults.clone()
            },
            SubDevice {
                configured_address: 0x3004,
                ports: ports_eol(),
                name: "EL2889".try_into().unwrap(),
                index: 6,
                ..subdevice_defaults.clone()
            },
        ];

        let ek1100_2_parents = &parents[0..3];
        let ek1100_2 = &parents[3];

        let el2828_parents = &parents[0..4];
        let el2828 = &parents[4];

        assert_eq!(
            find_subdevice_parent(ek1100_2_parents, ek1100_2).unwrap(),
            Some(1)
        );
        assert_eq!(
            find_subdevice_parent(el2828_parents, el2828).unwrap(),
            Some(ek1100_2.index)
        );
    }

    #[test]
    fn first_in_chain() {
        let subdevice_defaults = SubDevice {
            configured_address: 0x1000,
            ports: Ports::default(),
            name: "Default".try_into().unwrap(),
            ..Default::default()
        };

        let parents = [];

        let me = SubDevice {
            configured_address: 0x1100,
            ports: ports_eol(),
            name: "EK1100".try_into().unwrap(),
            index: 4,
            ..subdevice_defaults
        };

        let parent_index = find_subdevice_parent(&parents, &me);

        assert_eq!(parent_index.unwrap(), None);
    }

    #[test]
    fn p0_p1_times_only() {
        // From EC400 in test rig
        let ports = Ports([
            Port {
                active: true,
                dc_receive_time: 641524306,
                number: 0,
                downstream_to: None,
            },
            Port {
                active: false,
                dc_receive_time: 1413563250,
                number: 1,
                downstream_to: None,
            },
            Port {
                active: false,
                dc_receive_time: 0,
                number: 2,
                downstream_to: None,
            },
            Port {
                active: false,
                dc_receive_time: 0,
                number: 3,
                downstream_to: None,
            },
        ]);

        assert_eq!(ports.topology(), Topology::LineEnd);

        let mut subdevice = SubDevice {
            configured_address: 0x1000,
            ports,
            name: "Default".try_into().unwrap(),
            ..Default::default()
        };

        let parents = [];

        let mut delay_accum = 0u32;

        configure_subdevice_offsets(&mut subdevice, &parents, &mut delay_accum);

        assert_eq!(subdevice.dc_receive_time, 0u64);
    }

    /// Create a ports object with active flags and DC receive times.
    #[allow(clippy::too_many_arguments)]
    fn ports(
        active0: bool,
        t0: u32,
        active3: bool,
        t3: u32,
        active1: bool,
        t1: u32,
        active2: bool,
        t2: u32,
    ) -> Ports {
        let mut ports = Ports::new(active0, active3, active1, active2);

        ports.set_receive_times(t0, t3, t1, t2);

        ports
    }

    // Test that SubDevice parent/child relationships are established, and that propagation delays
    // are computed correctly.
    #[test]
    fn propagation_delay_calc_fork() {
        crate::test_logger();

        let defaults = SubDevice {
            configured_address: 0x999,
            name: "CHANGEME".try_into().unwrap(),
            ports: Ports::default(),
            dc_receive_time: 0,
            index: 0,
            dc_support: DcSupport::Bits64,
            ..SubDevice::default()
        };

        // Input data represents the following topology
        //
        // EK1100
        // --> EK1122
        // --> EL9560
        // EK1914
        // --> EL1008
        let mut subdevices = [
            SubDevice {
                index: 0,
                configured_address: 0x1000,
                name: "EK1100".try_into().unwrap(),
                ports: ports(
                    true, 3380373882, false, 1819436374, true, 3380374482, true, 3380375762,
                ),
                dc_receive_time: 402812332410,
                ..defaults.clone()
            },
            SubDevice {
                index: 1,
                configured_address: 0x1001,
                name: "EK1122".try_into().unwrap(),
                ports: ports(
                    true, 3384116362, false, 1819436374, false, 1717989224, true, 3384116672,
                ),
                dc_receive_time: 402816074890,
                ..defaults.clone()
            },
            SubDevice {
                index: 2,
                configured_address: 0x1002,
                name: "EL9560".try_into().unwrap(),
                ports: ports(
                    true, 3383862982, false, 1819436374, false, 1717989224, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
            SubDevice {
                index: 3,
                configured_address: 0x1003,
                name: "EK1914".try_into().unwrap(),
                ports: ports(
                    true, 3373883962, false, 1819436374, true, 3373884272, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
            SubDevice {
                index: 4,
                configured_address: 0x1004,
                name: "EL1008".try_into().unwrap(),
                ports: ports(
                    true, 3375060602, false, 1819436374, false, 1717989224, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
        ];

        let downstreams = [
            // Index 0: EK1100 (Fork)
            ([None, None, Some(1), Some(3)], None, 0),
            // Index 1: EK1122 (Passthrough)
            ([None, None, None, Some(2)], Some(0), 145),
            // Index 2: EL9560 (LineEnd)
            ([None, None, None, None], Some(1), 300),
            // Index 3: EK1914 (Passthrough)
            ([None, None, Some(4), None], Some(0), 1085),
            // Index 4: EL1008 (LineEnd)
            ([None, None, None, None], Some(3), 1240),
        ];

        let expected = {
            let mut expected = subdevices.clone();

            expected.iter_mut().zip(downstreams).for_each(
                |(subdevice, ([d0, d3, d1, d2], parent_index, propagation_delay))| {
                    subdevice.ports.set_downstreams(d0, d3, d1, d2);

                    subdevice.parent_index = parent_index;
                    subdevice.propagation_delay = propagation_delay;
                },
            );

            expected
        };

        assign_parent_relationships(&mut subdevices).expect("assign");

        pretty_assertions::assert_eq!(subdevices, expected);
    }

    #[test]
    fn propagation_delay_calc_cross() {
        crate::test_logger();

        let defaults = SubDevice {
            configured_address: 0x999,
            name: "CHANGEME".try_into().unwrap(),
            ports: Ports::default(),
            dc_receive_time: 0,
            index: 0,
            dc_support: DcSupport::Bits64,
            ..SubDevice::default()
        };

        let mut subdevices = [
            SubDevice {
                index: 0,
                configured_address: 0x1000,
                name: "EK1100".try_into().unwrap(),
                ports: ports(
                    true, 3493061450, false, 1819436374, true, 3493064460, false, 0,
                ),
                dc_receive_time: 3493061450,
                ..defaults.clone()
            },
            SubDevice {
                index: 1,
                configured_address: 0x1001,
                name: "EK1122".try_into().unwrap(),
                ports: ports(
                    true, 3493293220, true, 3493294570, true, 3493295650, true, 3493295940,
                ),
                dc_receive_time: 3493293220,
                ..defaults.clone()
            },
            SubDevice {
                index: 2,
                configured_address: 0x1002,
                name: "EK1914".try_into().unwrap(),
                ports: ports(
                    true, 3485337450, false, 1819436374, true, 3485337760, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
            SubDevice {
                index: 3,
                configured_address: 0x1003,
                name: "EL1008".try_into().unwrap(),
                ports: ports(
                    true, 3488375400, false, 1819436374, false, 1717989224, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
            SubDevice {
                index: 4,
                configured_address: 0x1004,
                name: "EK1101".try_into().unwrap(),
                ports: ports(
                    true, 3485087810, false, 1819436374, false, 1717989224, false, 0,
                ),
                dc_receive_time: 3485087810,
                ..defaults.clone()
            },
            SubDevice {
                index: 5,
                configured_address: 0x1005,
                name: "EL9560".try_into().unwrap(),
                ports: ports(
                    true, 3494335890, false, 1819436374, false, 1717989224, false, 0,
                ),
                dc_receive_time: 0,
                ..defaults.clone()
            },
        ];

        let downstreams = [
            // Index 0: EK1100 (Passthrough)
            ([None, None, Some(1), None], None, 0),
            // Index 1: EK1122 (Cross)
            ([None, Some(2), Some(4), Some(5)], Some(0), 145),
            // Index 2: EK1914 (Passthrough)
            ([None, None, Some(3), None], Some(1), 665),
            // Index 3: EL1008 (LineEnd)
            ([None, None, None, None], Some(2), 820),
            // Index 4: EK1101 (LineEnd)
            ([None, None, None, None], Some(1), 2035),
            // Index 5: EL9560 (LineEnd)
            ([None, None, None, None], Some(1), 2720),
        ];

        let expected = {
            let mut expected = subdevices.clone();

            expected.iter_mut().zip(downstreams).for_each(
                |(subdevice, ([d0, d3, d1, d2], parent_index, propagation_delay))| {
                    subdevice.ports.set_downstreams(d0, d3, d1, d2);

                    subdevice.parent_index = parent_index;
                    subdevice.propagation_delay = propagation_delay;
                },
            );

            expected
        };

        assign_parent_relationships(&mut subdevices).expect("assign");

        pretty_assertions::assert_eq!(subdevices, expected);
    }

    #[test]
    fn issue_302() {
        crate::test_logger();

        let defaults = SubDevice {
            configured_address: 0x999,
            name: "CHANGEME".try_into().unwrap(),
            ports: Ports::default(),
            dc_receive_time: 0,
            index: 0,
            dc_support: DcSupport::Bits64,
            ..SubDevice::default()
        };

        let mut subdevices = [
            SubDevice {
                index: 0,
                configured_address: 0x1000,
                name: "EK1100".try_into().unwrap(),
                ports: ports(
                    // Topology Passthrough, ports [ open closed open closed ]
                    // Receive times 2684027691 ns (0 ns) 1819436374 (864592197 ns) 2684028571 (0 ns) 0 (total 880)
                    true, 2684027691, false, 1819436374, true, 2684028571, false, 0,
                ),
                ..defaults.clone()
            },
            SubDevice {
                index: 1,
                configured_address: 0x1001,
                name: "EL2002".try_into().unwrap(),
                ports: ports(
                    // Topology Passthrough, ports [ open closed open closed ]
                    // Receive times 2680777793 ns (0 ns) 1819436374 (861342019 ns) 2680778393 (0 ns) 0 (total 600)
                    true, 2680777793, false, 1819436374, true, 2680778393, false, 0,
                ),
                ..defaults.clone()
            },
            SubDevice {
                index: 2,
                configured_address: 0x1002,
                name: "EL3001".try_into().unwrap(),
                ports: ports(
                    // Topology Passthrough, ports [ open closed open closed ]
                    // Receive times 2675710949 ns (0 ns) 1819436374 (856274885 ns) 2675711259 (0 ns) 0 (total 310)
                    true, 2675710949, false, 1819436374, true, 2675711259, false, 0,
                ),
                ..defaults.clone()
            },
            SubDevice {
                index: 3,
                configured_address: 0x1003,
                name: "EL7041-0052".try_into().unwrap(),
                ports: ports(
                    // Topology Passthrough, ports [ open closed open closed ]
                    // Receive times 2351926151 ns (0 ns) 1819436374 (0 ns) 1717989224 (0 ns) 0 (total 633936927)
                    true, 2351926151, false, 1819436374, true, 1717989224, false, 0,
                ),
                ..defaults.clone()
            },
        ];

        assert_eq!(assign_parent_relationships(&mut subdevices), Ok(()));
    }
}
