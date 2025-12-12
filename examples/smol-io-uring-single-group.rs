//! Use blocking io_uring-based TX/RX loop with async tasks executed with `smol` and a single group.
//!
//! This example pins the TX/RX loop to core 0, starts two other `smol` tasks on the main thread.
//!
//! You may need to increase `INTERVAL` as 100us can be challenging for some PCs. That said, a
//! Raspberry Pi 4 with a realtime kernel and some tweaking can run 2x 100us tasks _ok_.
//!
//! This example requires a Linux with `io_uring` support and a realtime kernel (e.g. `PREEMPT_RT`).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This example is only supported on Linux systems");
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), ethercrab::error::Error> {
    use env_logger::{Env, TimestampPrecision};
    use ethercrab::{
        MainDevice, MainDeviceConfig, PduStorage, Timeouts,
        std::{ethercat_now, tx_rx_task_io_uring},
    };
    use futures_lite::StreamExt;
    use std::{sync::Arc, time::Duration};
    use thread_priority::{
        RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };

    /// Maximum number of SubDevices that can be stored. This must be a power of 2 greater than 1.
    const MAX_SUBDEVICES: usize = 16;
    /// Maximum PDU data payload size - set this to the max PDI size or higher.
    const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// Maximum number of EtherCAT frames that can be in flight at any one time.
    const MAX_FRAMES: usize = 16;
    /// Maximum total PDI length.
    const PDI_LEN: usize = 64;
    /// Interval in microseconds.
    const INTERVAL: u64 = 100;

    static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp(Some(TimestampPrecision::Nanos))
        .init();

    let interface = std::env::args()
        .nth(1)
        .expect("Provide network interface as first argument.");

    log::info!("Starting single group demo with io_uring...");
    log::info!(
        "Ensure an EK1100 or EK1501 is the first SubDevice, with any number of modules connected after"
    );
    log::info!("Run with RUST_LOG=ethercrab=debug or =trace for debug information");

    let (tx, rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

    let core_ids = core_affinity::get_core_ids().expect("Couldn't get core IDs");

    let tx_rx_core = core_ids
        .first()
        .copied()
        .expect("At least one core is required. Are you running on a potato?");

    thread_priority::ThreadBuilder::default()
        .name("tx-rx-thread")
        // Might need to set `<user> hard rtprio 99` and `<user> soft rtprio 99` in `/etc/security/limits.conf`
        // Check limits with `ulimit -Hr` or `ulimit -Sr`
        .priority(ThreadPriority::Crossplatform(
            ThreadPriorityValue::try_from(49u8).unwrap(),
        ))
        // NOTE: Requires a realtime kernel
        .policy(ThreadSchedulePolicy::Realtime(
            RealtimeThreadSchedulePolicy::Fifo,
        ))
        .spawn(move |_| {
            core_affinity::set_for_current(tx_rx_core)
                .then_some(())
                .expect("Set TX/RX thread core");

            // Blocking io_uring
            tx_rx_task_io_uring(&interface, tx, rx).expect("TX/RX task");
        })
        .unwrap();

    let maindevice = MainDevice::new(
        pdu_loop,
        Timeouts {
            // Enormous timeout so we can still keep going even with very high system load
            // preventing processing from happening.
            pdu: Duration::from_millis(1000),
            ..Timeouts::default()
        },
        MainDeviceConfig::default(),
    );

    let maindevice = Arc::new(maindevice);

    // Read configurations from SubDevice EEPROMs and configure devices.
    let group =
        smol::block_on(maindevice.init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now))
            .expect("Init");

    log::info!("Discovered {} SubDevices", group.len());

    // for subdevice in group.iter(&maindevice) {
    //     if subdevice.name() == "EL3004" {
    //         log::info!("Found EL3004. Configuring...");

    //         smol::block_on(subdevice.sdo_write(0x1c12, 0, 0u8)).expect("SDO write");

    //         smol::block_on(subdevice.sdo_write_array(0x1c13, &[0x1a00u16, 0x1a02, 0x1a04, 0x1a06]))
    //             .expect("SDO write array");

    //         // The `sdo_write_array` call above is equivalent to the following
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 0, 0u8)).expect("SDO write");
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 1, 0x1a00u16)).expect("SDO write");
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 2, 0x1a02u16)).expect("SDO write");
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 3, 0x1a04u16)).expect("SDO write");
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 4, 0x1a06u16)).expect("SDO write");
    //         // smol::block_on(subdevice.sdo_write(0x1c13, 0, 4u8)).expect("SDO write");
    //     }
    // }

    let group = smol::block_on(group.into_op(&maindevice)).expect("PRE-OP -> OP");

    for subdevice in group.iter(&maindevice) {
        let io = subdevice.io_raw();

        log::info!(
            "-> SubDevice {:#06x} {} inputs: {} bytes, outputs: {} bytes",
            subdevice.configured_address(),
            subdevice.name(),
            io.inputs().len(),
            io.outputs().len()
        );
    }

    let maindevice_clone = maindevice.clone();

    let task = smol::spawn(async move {
        let mut cycle_time = smol::Timer::interval(Duration::from_micros(INTERVAL));

        // 添加时间测量
        let mut last_time = std::time::Instant::now();

        loop {
            let Ok(_) = group.tx_rx(&maindevice_clone).await else {
                break;
            };

            // 测量实际周期时间
            let current_time = std::time::Instant::now();
            let elapsed = current_time.duration_since(last_time);
            last_time = current_time;

            // [2025-12-12T02:41:07.465343268Z INFO  smol_io_uring_single_group] Actual cycle time: 246.557µs (expected: 100µs)
            // 抓包和打印都证明没有达到100µs
            log::info!(
                "Actual cycle time: {:?} (expected: {:?})",
                elapsed,
                Duration::from_micros(INTERVAL)
            );

            // Increment every output byte for every SubDevice by one
            for subdevice in group.iter(&maindevice_clone) {
                let mut o = subdevice.outputs_raw_mut();

                for byte in o.iter_mut() {
                    *byte = byte.wrapping_add(1);
                }
            }

            cycle_time.next().await;
        }
    });

    smol::block_on(task);

    Ok(())
}
