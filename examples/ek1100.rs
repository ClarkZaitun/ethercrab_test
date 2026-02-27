//! Demonstrate setting outputs using a Beckhoff EK1100/EK1501 and modules.
//!
//! Run with e.g.
//!
//! Linux
//!
//! ```bash
//! RUST_LOG=debug cargo run --example ek1100 --release -- eth0
//! ```
//!
//! Windows
//!
//! ```ps
//! $env:RUST_LOG="debug" ; cargo run --example ek1100 --release -- '\Device\NPF_{FF0ACEE6-E8CD-48D5-A399-619CD2340465}'
//! ```

use env_logger::Env;
use ethercrab::{
    MainDevice, MainDeviceConfig, PduStorage, Timeouts, error::Error, std::ethercat_now,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::MissedTickBehavior;

/// Maximum number of SubDevices that can be stored. This must be a power of 2 greater than 1.
const MAX_SUBDEVICES: usize = 16;
/// Maximum PDU data payload size - set this to the max PDI size or higher.
const MAX_PDU_DATA: usize = PduStorage::element_size(1100); //1100+28
/// Maximum number of EtherCAT frames that can be in flight at any one time.
const MAX_FRAMES: usize = 16;
/// Maximum total PDI length.
// 每个组的过程数据映像（PDI）的最大字节数。用于设置 SubDeviceGroup 的 max_pdi_len 字段
const PDI_LEN: usize = 64;

static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    //从命令行参数中获取网络接口名称，并将其赋值给变量 interface。
    //如果没有提供网络接口名称，则会触发 panic 并显示错误消息 "Provide network interface as first argument."。
    let interface = std::env::args()
        .nth(1)
        .expect("Provide network interface as first argument.");

    log::info!("Starting EK1100/EK1501 demo...");
    log::info!(
        "Ensure an EK1100 or EK1501 is the first SubDevice, with any number of modules connected after"
    );
    log::info!("Run with RUST_LOG=ethercrab=debug or =trace for debug information");

    //从 PDU_STORAGE 实例中拆分出发送通道、接收通道和 PDU 循环处理对象，用于后续的 EtherCAT 通信
    let (tx, rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

    //创建一个 MainDevice 实例，并使用 Arc （原子引用计数）进行包装，以便在多线程环境下安全地共享该实例。
    let maindevice = Arc::new(MainDevice::new(
        pdu_loop,
        Timeouts {
            wait_loop_delay: Duration::from_millis(2),
            mailbox_response: Duration::from_millis(1000),
            ..Default::default()
        },
        MainDeviceConfig::default(),
    ));

    #[cfg(target_os = "windows")]
    std::thread::spawn(move || {
        ethercrab::std::tx_rx_task_blocking(
            &interface,
            tx,
            rx,
            ethercrab::std::TxRxTaskConfig { spinloop: false },
        )
        .expect("TX/RX task")
    });

    //异步地启动一个任务，该任务负责处理 EtherCAT 数据的发送和接收
    //tx_rx_task返回的TxRxFut从表面看它只是一个结构体，但 Rust 里借助实现 Future trait 能把结构体转变为异步任务
    //Future trait会实现 poll 方法：负责推进异步计算。
    //在每次调用 poll 方法时，Future 会检查自己的状态，如果状态已经就绪（即完成），则返回 Poll::Ready 结果；如果状态未就绪，则返回 Poll::Pending 结果。
    #[cfg(not(target_os = "windows"))]
    tokio::spawn(ethercrab::std::tx_rx_task(&interface, tx, rx).expect("spawn TX/RX task"));

    // 请求并等待所有子设备处于“PRE-OP”状态后再返回。只返回一个组
    let group = maindevice
        .init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now)
        .await
        .expect("Init");

    log::info!("Discovered {} SubDevices", group.len());

    // 对每个从站配置PDO
    // TODO 配置PDO的过程应该封装成接口，用户只需要设置PDO的Entry，具体SDO如何发送配置由库实现。
    // 此时库可以知道PDO映射关系，得到PDI分布，从而可以配置FMMU
    for subdevice in group.iter(&maindevice) {
        // 改为匹配vendor id和product code更合理
        if subdevice.name() == "EL3004" {
            log::info!("Found EL3004. Configuring...");

            // 配置PDO
            subdevice.sdo_write(0x1c12, 0, 0u8).await?;

            subdevice
                .sdo_write_array(0x1c13, &[0x1a00u16, 0x1a02, 0x1a04, 0x1a06])
                .await?;

            // The `sdo_write_array` call above is equivalent to the following
            // subdevice.sdo_write(0x1c13, 0, 0u8).await?;
            // subdevice.sdo_write(0x1c13, 1, 0x1a00u16).await?;
            // subdevice.sdo_write(0x1c13, 2, 0x1a02u16).await?;
            // subdevice.sdo_write(0x1c13, 3, 0x1a04u16).await?;
            // subdevice.sdo_write(0x1c13, 4, 0x1a06u16).await?;
            // subdevice.sdo_write(0x1c13, 0, 4u8).await?;
        }
    }

    // 从pre op切换到op
    let group = group.into_op(&maindevice).await.expect("PRE-OP -> OP");

    for subdevice in group.iter(&maindevice) {
        // 通过 io_raw 方法获取 SubDevice 的输入输出PDO数据
        let io = subdevice.io_raw();

        // 打印IO信息
        log::info!(
            "-> SubDevice {:#06x} {} inputs: {} bytes, outputs: {} bytes",
            subdevice.configured_address(),
            subdevice.name(),
            io.inputs().len(),
            io.outputs().len()
        );
    }

    // 创建一个 tokio 定时器，用于按5ms间隔执行任务。
    let mut tick_interval = tokio::time::interval(Duration::from_millis(5));
    // 设置定时器错过预定时间点时的处理策略：当定时器错过多个预定 tick 时间点时，直接跳过这些错过的 tick，只在后续正常的时间点产生 tick
    tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let shutdown = Arc::new(AtomicBool::new(false));
    // 设置一个信号处理机制，用于在接收到 SIGINT 信号（通常是用户按下 Ctrl + C 时发送的信号）时，标记程序需要进行优雅关闭
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .expect("Register hook");
    // signal_hook 是一个第三方库，用于在 Rust 程序中处理 Unix 信号
    // register 函数用于注册信号处理钩子，当接收到 SIGINT 信号时，会将 shutdown 中的 AtomicBool 值设置为 true

    // 实时任务
    loop {
        // Graceful shutdown on Ctrl + C
        if shutdown.load(Ordering::Relaxed) {
            log::info!("Shutting down...");

            break;
        }

        group.tx_rx(&maindevice).await.expect("TX/RX");

        // Increment every output byte for every SubDevice by one
        for subdevice in group.iter(&maindevice) {
            let mut o = subdevice.outputs_raw_mut();

            for byte in o.iter_mut() {
                *byte = byte.wrapping_add(1);
            }
        }

        // 这里使用tokio定时器来做实时任务，发帧抖动无法保证
        tick_interval.tick().await;
    }

    // 退出时，切换网络状态到init
    let group = group
        .into_safe_op(&maindevice)
        .await
        .expect("OP -> SAFE-OP");

    log::info!("OP -> SAFE-OP");

    let group = group
        .into_pre_op(&maindevice)
        .await
        .expect("SAFE-OP -> PRE-OP");

    log::info!("SAFE-OP -> PRE-OP");

    let _group = group.into_init(&maindevice).await.expect("PRE-OP -> INIT");

    log::info!("PRE-OP -> INIT, shutdown complete");

    Ok(())
}
