use crate::{SubDevice, fmt};
use core::{fmt::Debug, num::NonZeroU16};

/// Flags showing which ports are active or not on the SubDevice.
#[derive(Default, Debug, PartialEq, Eq, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Port {
    // 是否激活
    pub active: bool,
    // 端口的接收时间
    pub dc_receive_time: u32,
    /// The EtherCAT port number, ordered as 0 -> 3 -> 1 -> 2.
    // 端口序号
    pub number: u8,
    /// Holds the index of the downstream SubDevice this port is connected to.
    // 保存此端口连接到的下游子设备的地址，用于还原拓扑
    pub downstream_to: Option<NonZeroU16>,
}

impl Port {
    // TODO: Un-pub
    // 端口序号转换到端口数组Ports的下标
    pub(crate) fn index(&self) -> usize {
        match self.number {
            0 => 0,
            3 => 1,
            1 => 2,
            2 => 3,
            n => unreachable!("Invalid port number {}", n),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Topology {
    /// The SubDevice has two open ports, with only upstream and downstream subdevices.
    Passthrough,
    /// The SubDevice is the last device in its fork of the topology tree, with only one open port.
    LineEnd,
    /// The SubDevice forms a fork in the topology, with 3 open ports.
    Fork,
    /// The SubDevice forms a cross in the topology, with 4 open ports.
    Cross,
}

impl Topology {
    // 判断是否为分叉节点或者交叉节点
    pub fn is_junction(&self) -> bool {
        matches!(self, Self::Fork | Self::Cross)
    }
}

#[derive(Default, Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Ports(pub [Port; 4]);

impl core::fmt::Display for Ports {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ports [ ")?;

        for p in self.0 {
            if p.active {
                f.write_str("open ")?;
            } else {
                f.write_str("closed ")?;
            }
        }

        f.write_str("]")?;

        Ok(())
    }
}

impl Ports {
    pub(crate) fn new(active0: bool, active3: bool, active1: bool, active2: bool) -> Self {
        Self([
            Port {
                active: active0,
                number: 0,
                ..Port::default()
            },
            Port {
                active: active3,
                number: 3,
                ..Port::default()
            },
            Port {
                active: active1,
                number: 1,
                ..Port::default()
            },
            Port {
                active: active2,
                number: 2,
                ..Port::default()
            },
        ])
    }

    /// Set port DC receive times, given in EtherCAT port order 0 -> 3 -> 1 -> 2
    pub(crate) fn set_receive_times(
        &mut self,
        time_p0: u32,
        time_p3: u32,
        time_p1: u32,
        time_p2: u32,
    ) {
        // NOTE: indexes vs EtherCAT port order
        // 注意端口的顺序
        self.0[0].dc_receive_time = time_p0;
        self.0[1].dc_receive_time = time_p3;
        self.0[2].dc_receive_time = time_p1;
        self.0[3].dc_receive_time = time_p2;
    }

    /// TEST ONLY: Set downstream ports.
    #[cfg(test)]
    pub(crate) fn set_downstreams(
        &mut self,
        d0: Option<u16>,
        d3: Option<u16>,
        d1: Option<u16>,
        d2: Option<u16>,
    ) -> Self {
        self.0[0].downstream_to = d0.map(|idx| NonZeroU16::new(idx).unwrap());
        self.0[1].downstream_to = d3.map(|idx| NonZeroU16::new(idx).unwrap());
        self.0[2].downstream_to = d1.map(|idx| NonZeroU16::new(idx).unwrap());
        self.0[3].downstream_to = d2.map(|idx| NonZeroU16::new(idx).unwrap());

        *self
    }

    // 如果写成C风格代码，不优雅，但性能很好
    // 计算从站激活端口数量
    fn open_ports(&self) -> u8 {
        // 调用迭代器的 count 方法，该方法会遍历迭代器中的所有元素并返回元素的数量，类型为 usize
        self.active_ports().count() as u8
    }

    // 返回一个迭代器，该迭代器会遍历 Ports 结构体中所有处于激活状态的端口
    fn active_ports(&self) -> impl Iterator<Item = &Port> + Clone {
        // Ports 结构体是一个元组结构体，self.0 访问其内部存储的 [Port; 4] 数组
        // .iter()：调用数组的 iter 方法，返回一个迭代器，该迭代器会遍历数组中的每个元素，产生元素的不可变引用
        // .filter(|port| port.active)：调用迭代器的 filter 方法，传入一个闭包 |port| port.active。
        // filter 方法会对迭代器中的每个元素应用该闭包，只保留闭包返回 true 的元素。这里的闭包检查 Port 结构体的 active 字段是否为 true，即端口是否处于激活状态
        self.0.iter().filter(|port| port.active)
    }

    /// The port of the SubDevice that first sees EtherCAT traffic.
    // 获取当前设备中第一个接收到 EtherCAT 流量的端口，该端口是流量进入设备的入口
    // 正确的网线连接都是端口0。如果网线反接才是其他端口。这个库允许反接的情况?
    pub fn entry_port(&self) -> Port {
        fmt::unwrap_opt!(
            self.active_ports() // 返回一个迭代器，该迭代器会遍历 Ports 结构体中所有处于激活状态的端口
                .min_by_key(|port| port.dc_receive_time) // 找到端口接收时间最小的端口
                .copied() // 将 Option<&Port> 类型转换为 Option<Port> 类型
        )
    }

    // 获取最后一个开放端口
    // 这里的最后一个的意思是ESC激活端口顺序的最后一个，不是按照端口接收时间最大判断出的物理连接顺序最后的那个
    /// Get the last open port.
    pub fn last_port(&self) -> Option<&Port> {
        self.active_ports().last()
    }

    /// Find the next port that hasn't already been assigned as the upstream port of another
    /// SubDevice.
    // 找出下一个还未被指定为其他从设备（SubDevice）上游端口的端口
    fn next_assignable_port(&mut self, this_port: &Port) -> Option<&mut Port> {
        // 端口序号转换到端口数组Ports的下标
        let this_port_index = this_port.index();

        let next_port_index = self
            .active_ports()
            .cycle() // 将迭代器转换为循环迭代器，当遍历到激活端口列表末尾时，会重新从列表开头开始遍历
            // Start at the next port
            // 从下一个端口开始查找
            .skip(this_port_index + 1)
            .take(4) // 最多只查看 4 个端口，避免无限循环
            .find(|next_port| next_port.downstream_to.is_none())? // 查找第一个 downstream_to 字段为 None 的端口
            .index(); // 获取找到的端口在 Ports 结构体内部数组中的索引

        self.0.get_mut(next_port_index)
    }

    /// Link a downstream device to the current device using the next open port from the entry port.
    // 使用入口端口的下一个开放端口将下游设备链接到当前设备，返回端口序号
    pub fn assign_next_downstream_port(
        &mut self,
        downstream_subdevice_index: NonZeroU16,
    ) -> Option<u8> {
        // 获取当前设备中第一个接收到 EtherCAT 流量的端口，该端口是流量进入设备的入口
        let entry_port = self.entry_port();

        // 找出下一个还未被指定为其他从设备（SubDevice）上游端口的端口
        let next_port = self.next_assignable_port(&entry_port)?;

        // 将本端口分配给下游从站
        // 需要考证为什么下一个还未被指定为其他从设备（SubDevice）上游端口的端口，就是寻找的要连接的端口
        // 接错的情况下又会发生什么？
        next_port.downstream_to = Some(downstream_subdevice_index);

        // 返回端口序号
        Some(next_port.number)
    }

    /// Find the port assigned to the given SubDevice.
    // 通过父从站的端口连接的从站索引找到在当前 Ports 实例里分配给指定从站的端口
    pub fn port_assigned_to(&self, subdevice: &SubDevice) -> Option<&Port> {
        self.active_ports()
            .find(|port| port.downstream_to.map(|idx| idx.get()) == Some(subdevice.index))
    }

    // 根据端口数量，判断从站是什么类型的拓扑节点
    pub fn topology(&self) -> Topology {
        // 计算从站激活端口数量
        match self.open_ports() {
            1 => Topology::LineEnd,     // 叶子
            2 => Topology::Passthrough, // 直线
            3 => Topology::Fork,        // 分叉
            4 => Topology::Cross,       // 交叉
            n => unreachable!("Invalid topology {}", n),
        }
    }

    // 判断当前端口是否为从站的最后一个端口（ESC顺序）
    pub fn is_last_port(&self, port: &Port) -> bool {
        // 获取最后一个开放端口，和当前端口对比
        self.last_port().filter(|p| *p == port).is_some()
    }

    /// The time in nanoseconds for a packet to completely traverse all active ports of a SubDevice.
    // 计算从站4个端口最大和最小接收时间的差值，就是帧在从站之后网络传输的时间
    // 如果假设线缆延迟均匀，并且所有从站设备的处理和转发延迟一样
    #[deny(clippy::arithmetic_side_effects)] // 禁止可能产生意外算术副作用（如整数溢出、下溢）的操作
    pub fn total_propagation_time(&self) -> Option<u32> {
        // 得到一个只包含激活端口接收时间的迭代器
        let times = self
            .0
            .iter()
            .filter_map(|port| port.active.then_some(port.dc_receive_time));

        // 计算最大和最小接收时间的差值
        times
            .clone() // 由于迭代器是消耗性的，调用 max 方法会消耗迭代器
            .max() // 在克隆的迭代器中查找最大的接收时间
            .and_then(|max| times.min().map(|min| max.saturating_sub(min)))
            .filter(|t| *t > 0)
    }

    /// Propagation time between active ports in this SubDevice.
    #[deny(clippy::arithmetic_side_effects)]
    pub fn intermediate_propagation_time_to(&self, port: &Port) -> u32 {
        // If a pair of ports is open, they have a propagation delta between them, and we can sum
        // these deltas up to get the child delays of this SubDevice (fork or cross have children)
        self.0
            .windows(2)
            .map(|window| {
                // Silly Rust
                let [a, b] = window else { return 0 };

                // Stop iterating as we've summed everything before the target port
                if a.index() >= port.index() {
                    return 0;
                }

                // Both ports must be active to have a delta
                if a.active && b.active {
                    b.dc_receive_time.saturating_sub(a.dc_receive_time)
                } else {
                    0
                }
            })
            .sum::<u32>()
    }

    // 计算 EtherCAT 流量进入当前从站的入口端口，到指定端口的传播时间
    /// Get the propagation time taken from entry to this SubDevice up to the given port.
    #[deny(clippy::arithmetic_side_effects)] // 禁止可能产生意外算术副作用（如整数溢出、下溢）的操作
    pub fn propagation_time_to(&self, this_port: &Port) -> Option<u32> {
        // 获取当前设备中第一个接收到 EtherCAT 流量的端口，该端口是流量进入设备的入口
        let entry_port = self.entry_port();

        // Find active ports between entry and this one
        // 返回map,其中包含各端口帧接收时间
        let times = self
            .active_ports()
            // 只要端口的数组下标大于等于入口端口下标,小于等于指定端口下标的端口
            // TODO:这个筛选规则是否合理?如果入口端口一定是0(不能适应网线反接的情况),则正确
            // TODO:如果网线反接,则端口的数组应该设置为一个循环数组,这个判断规则需要修改
            .filter(|port| port.index() >= entry_port.index() && port.index() <= this_port.index())
            .map(|port| port.dc_receive_time);

        times
            .clone()
            .max() // 在克隆的迭代器中查找最大的接收时间
            // 若找到最大接收时间，再在原迭代器中查找最小接收时间
            // 用最大接收时间减去最小接收时间得到传播时间，saturating_sub 确保减法结果不会为负数
            .and_then(|max| times.min().map(|min| max.saturating_sub(min)))
            .filter(|t| *t > 0) // 筛选出传播时间大于 0 的结果，若传播时间为 0 则返回 None
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const ENTRY_RECEIVE: u32 = 1234;

    pub(crate) fn make_ports(active0: bool, active3: bool, active1: bool, active2: bool) -> Ports {
        let mut ports = Ports::new(active0, active3, active1, active2);

        ports.0[0].dc_receive_time = ENTRY_RECEIVE;
        ports.0[1].dc_receive_time = ENTRY_RECEIVE + 100;
        ports.0[2].dc_receive_time = ENTRY_RECEIVE + 200;
        ports.0[3].dc_receive_time = ENTRY_RECEIVE + 300;

        ports
    }

    #[test]
    fn open_ports() {
        // EK1100 with children attached to port 3 and downstream devices on port 1
        let ports = make_ports(true, true, true, false);
        // Normal SubDevice has no children, so no child delay
        let passthrough = make_ports(true, true, false, false);

        assert_eq!(ports.open_ports(), 3);
        assert_eq!(passthrough.open_ports(), 2);
    }

    #[test]
    fn topologies() {
        let passthrough = make_ports(true, true, false, false);
        let passthrough_skip_port = make_ports(true, false, true, false);
        let fork = make_ports(true, true, true, false);
        let line_end = make_ports(true, false, false, false);
        let cross = make_ports(true, true, true, true);

        assert_eq!(passthrough.topology(), Topology::Passthrough);
        assert_eq!(passthrough_skip_port.topology(), Topology::Passthrough);
        assert_eq!(fork.topology(), Topology::Fork);
        assert_eq!(line_end.topology(), Topology::LineEnd);
        assert_eq!(cross.topology(), Topology::Cross);
    }

    #[test]
    fn entry_port() {
        // EK1100 with children attached to port 3 and downstream devices on port 1
        let ports = make_ports(true, true, true, false);

        assert_eq!(
            ports.entry_port(),
            Port {
                active: true,
                number: 0,
                dc_receive_time: ENTRY_RECEIVE,
                ..Port::default()
            }
        );
    }

    #[test]
    fn propagation_time() {
        // Passthrough SubDevice
        let ports = make_ports(true, true, false, false);

        assert_eq!(ports.total_propagation_time(), Some(100));
    }

    #[test]
    fn propagation_time_fork() {
        // Fork, e.g. EK1100 with modules AND downstream devices
        let ports = make_ports(true, true, true, false);

        assert_eq!(ports.total_propagation_time(), Some(200));
    }

    #[test]
    fn propagation_time_cross() {
        // Cross, e.g. EK1122 in a module chain with both ports connected
        let ports = make_ports(true, true, true, true);

        assert_eq!(ports.topology(), Topology::Cross);
        assert_eq!(ports.total_propagation_time(), Some(300));
    }

    #[test]
    fn assign_downstream_port() {
        let mut ports = make_ports(true, true, true, false);

        assert_eq!(
            ports.entry_port(),
            Port {
                active: true,
                dc_receive_time: ENTRY_RECEIVE,
                number: 0,
                downstream_to: None
            }
        );

        let port_number = ports.assign_next_downstream_port(NonZeroU16::new(1).unwrap());

        assert_eq!(port_number, Some(3), "assign SubDevice idx 1");

        let port_number = ports.assign_next_downstream_port(NonZeroU16::new(2).unwrap());

        assert_eq!(port_number, Some(1), "assign SubDevice idx 2");

        pretty_assertions::assert_eq!(
            ports,
            Ports([
                // Entry port
                Port {
                    active: true,
                    dc_receive_time: ENTRY_RECEIVE,
                    number: 0,
                    downstream_to: None,
                },
                Port {
                    active: true,
                    dc_receive_time: ENTRY_RECEIVE + 100,
                    number: 3,
                    downstream_to: Some(NonZeroU16::new(1).unwrap()),
                },
                Port {
                    active: true,
                    dc_receive_time: ENTRY_RECEIVE + 200,
                    number: 1,
                    downstream_to: Some(NonZeroU16::new(2).unwrap()),
                },
                Port {
                    active: false,
                    dc_receive_time: ENTRY_RECEIVE + 300,
                    number: 2,
                    downstream_to: None,
                }
            ])
        )
    }

    #[test]
    fn propagation_time_to_last_port() {
        let ports = make_ports(true, false, true, true);

        let last = ports.last_port().unwrap();

        assert_eq!(
            ports.propagation_time_to(last),
            ports.total_propagation_time()
        );
    }

    #[test]
    fn propagation_time_to_intermediate_port() {
        // Cross topology
        let ports = make_ports(true, true, true, true);

        let up_to = &ports.0[2];

        assert_eq!(ports.propagation_time_to(up_to), Some(200));
    }

    #[test]
    fn propagation_time_cross_first() {
        // Cross topology, e.g. EK1122
        let mut ports = make_ports(true, true, true, true);

        // Deltas are 1340ns, 1080ns and 290ns
        ports.set_receive_times(3699944655, 3699945995, 3699947075, 3699947365);

        // Device connected to EtherCAT port number 3 (second index)
        let up_to = &ports.0[1];

        assert_eq!(ports.propagation_time_to(up_to), Some(1340));
    }

    #[test]
    fn propagation_time_cross_second() {
        // Cross topology, e.g. EK1122
        let mut ports = make_ports(true, true, true, true);

        // Deltas are 1340ns, 1080ns and 290ns
        ports.set_receive_times(3699944655, 3699945995, 3699947075, 3699947365);

        // Device connected to EtherCAT port number 3 (second index)
        let up_to = &ports.0[2];

        assert_eq!(ports.propagation_time_to(up_to), Some(1340 + 1080));
    }
}
