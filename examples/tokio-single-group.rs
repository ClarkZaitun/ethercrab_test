//! Use blocking socket-based TX/RX loop with async tasks executed with `tokio` and a single group.
//!
//! This example pins the TX/RX loop to core 0, starts tasks on the main thread.
//!
//! You may need to increase `INTERVAL` as 100us can be challenging for some PCs. That said, a
//! Raspberry Pi 4 with a realtime kernel and some tweaking can run 2x 100us tasks _ok_.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This example is only supported on Linux systems");
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), ethercrab::error::Error> {
    use env_logger::{Env, TimestampPrecision};
    use ethercrab::{MainDevice, MainDeviceConfig, PduStorage, Timeouts, std::ethercat_now};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::time::MissedTickBehavior;

    /// Maximum number of SubDevices that can be stored. This must be a power of 2 greater than 1.
    const MAX_SUBDEVICES: usize = 16;
    /// Maximum PDU data payload size - set this to the max PDI size or higher.
    const MAX_PDU_DATA: usize = PduStorage::element_size(1100);
    /// Maximum number of EtherCAT frames that can be in flight at any one time.
    const MAX_FRAMES: usize = 16;
    /// Maximum total PDI length.
    const PDI_LEN: usize = 64;
    /// Interval in microseconds.
    const INTERVAL: u64 = 1000;

    static PDU_STORAGE: PduStorage<MAX_FRAMES, MAX_PDU_DATA> = PduStorage::new();

    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp(Some(TimestampPrecision::Nanos))
        .init();

    let interface = std::env::args()
        .nth(1)
        .expect("Provide network interface as first argument.");

    log::info!("Starting single group demo with tokio (similar to ek1100.rs)...");
    log::info!(
        "Ensure an EK1100 or EK1501 is the first SubDevice, with any number of modules connected after"
    );
    log::info!("Run with RUST_LOG=ethercrab=debug or =trace for debug information");

    let (tx, rx, pdu_loop) = PDU_STORAGE.try_split().expect("can only split once");

    // Spawn the TX/RX task similar to ek1100.rs
    tokio::spawn(ethercrab::std::tx_rx_task(&interface, tx, rx).expect("spawn TX/RX task"));

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
    let group = maindevice
        .init_single_group::<MAX_SUBDEVICES, PDI_LEN>(ethercat_now)
        .await
        .expect("Init");

    log::info!("Discovered {} SubDevices", group.len());

    let group = group.into_op(&maindevice).await.expect("PRE-OP -> OP");

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

    // Create interval timer for cyclic task
    let mut interval = tokio::time::interval(Duration::from_micros(INTERVAL));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 添加时间测量
    let mut last_time = Instant::now();

    loop {
        let Ok(_) = group.tx_rx(&maindevice_clone).await else {
            break;
        };

        // 测量实际周期时间
        let current_time = Instant::now();
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

        interval.tick().await;
    }

    Ok(())
}

// [2025-12-12T03:34:07.143426648Z INFO  tokio_single_group] Actual cycle time: 249.566µs (expected: 1ms)
// [2025-12-12T03:34:07.144631559Z INFO  tokio_single_group] Actual cycle time: 1.206475ms (expected: 1ms)
// [2025-12-12T03:34:07.145820230Z INFO  tokio_single_group] Actual cycle time: 1.188835ms (expected: 1ms)
// [2025-12-12T03:34:07.147016619Z INFO  tokio_single_group] Actual cycle time: 1.196439ms (expected: 1ms)
// [2025-12-12T03:34:07.147260305Z INFO  tokio_single_group] Actual cycle time: 243.639µs (expected: 1ms)
// [2025-12-12T03:34:07.148463229Z INFO  tokio_single_group] Actual cycle time: 1.203214ms (expected: 1ms)
// [2025-12-12T03:34:07.149644394Z INFO  tokio_single_group] Actual cycle time: 1.181051ms (expected: 1ms)
// [2025-12-12T03:34:07.150832679Z INFO  tokio_single_group] Actual cycle time: 1.188226ms (expected: 1ms)
// [2025-12-12T03:34:07.152035344Z INFO  tokio_single_group] Actual cycle time: 1.202635ms (expected: 1ms)
// [2025-12-12T03:34:07.152276389Z INFO  tokio_single_group] Actual cycle time: 241.291µs (expected: 1ms)
// [2025-12-12T03:34:07.153477794Z INFO  tokio_single_group] Actual cycle time: 1.201155ms (expected: 1ms)
// [2025-12-12T03:34:07.154667881Z INFO  tokio_single_group] Actual cycle time: 1.190314ms (expected: 1ms)
// [2025-12-12T03:34:07.155847031Z INFO  tokio_single_group] Actual cycle time: 1.179287ms (expected: 1ms)
// [2025-12-12T03:34:07.157055849Z INFO  tokio_single_group] Actual cycle time: 1.208417ms (expected: 1ms)
// [2025-12-12T03:34:07.157277022Z INFO  tokio_single_group] Actual cycle time: 221.226µs (expected: 1ms)
// [2025-12-12T03:34:07.158464089Z INFO  tokio_single_group] Actual cycle time: 1.186992ms (expected: 1ms)
// [2025-12-12T03:34:07.159640661Z INFO  tokio_single_group] Actual cycle time: 1.17691ms (expected: 1ms)
// [2025-12-12T03:34:07.160820008Z INFO  tokio_single_group] Actual cycle time: 1.179497ms (expected: 1ms)
// [2025-12-12T03:34:07.162026604Z INFO  tokio_single_group] Actual cycle time: 1.204205ms (expected: 1ms)
// [2025-12-12T03:34:07.162251140Z INFO  tokio_single_group] Actual cycle time: 225.512µs (expected: 1ms)
// [2025-12-12T03:34:07.163458043Z INFO  tokio_single_group] Actual cycle time: 1.207868ms (expected: 1ms)
// [2025-12-12T03:34:07.164650743Z INFO  tokio_single_group] Actual cycle time: 1.192939ms (expected: 1ms)
// [2025-12-12T03:34:07.165822402Z INFO  tokio_single_group] Actual cycle time: 1.171825ms (expected: 1ms)
// [2025-12-12T03:34:07.167018325Z INFO  tokio_single_group] Actual cycle time: 1.193614ms (expected: 1ms)
// [2025-12-12T03:34:07.167248208Z INFO  tokio_single_group] Actual cycle time: 231.632µs (expected: 1ms)
// [2025-12-12T03:34:07.168460412Z INFO  tokio_single_group] Actual cycle time: 1.211455ms (expected: 1ms)
// [2025-12-12T03:34:07.169676597Z INFO  tokio_single_group] Actual cycle time: 1.215445ms (expected: 1ms)
// [2025-12-12T03:34:07.170875012Z INFO  tokio_single_group] Actual cycle time: 1.200017ms (expected: 1ms)
// [2025-12-12T03:34:07.172075763Z INFO  tokio_single_group] Actual cycle time: 1.19993ms (expected: 1ms)
// [2025-12-12T03:34:07.172306985Z INFO  tokio_single_group] Actual cycle time: 232.279µs (expected: 1ms)
// [2025-12-12T03:34:07.173499157Z INFO  tokio_single_group] Actual cycle time: 1.191909ms (expected: 1ms)
// [2025-12-12T03:34:07.174697042Z INFO  tokio_single_group] Actual cycle time: 1.196394ms (expected: 1ms)
// [2025-12-12T03:34:07.175894862Z INFO  tokio_single_group] Actual cycle time: 1.199158ms (expected: 1ms)
// [2025-12-12T03:34:07.177106653Z INFO  tokio_single_group] Actual cycle time: 1.210064ms (expected: 1ms)
// [2025-12-12T03:34:07.177338850Z INFO  tokio_single_group] Actual cycle time: 234.148µs (expected: 1ms)
// [2025-12-12T03:34:07.178541602Z INFO  tokio_single_group] Actual cycle time: 1.202164ms (expected: 1ms)
// [2025-12-12T03:34:07.179719974Z INFO  tokio_single_group] Actual cycle time: 1.178971ms (expected: 1ms)
// [2025-12-12T03:34:07.180890400Z INFO  tokio_single_group] Actual cycle time: 1.170781ms (expected: 1ms)
// [2025-12-12T03:34:07.182071738Z INFO  tokio_single_group] Actual cycle time: 1.181143ms (expected: 1ms)
// [2025-12-12T03:34:07.182317189Z INFO  tokio_single_group] Actual cycle time: 245.635µs (expected: 1ms)
// [2025-12-12T03:34:07.183516622Z INFO  tokio_single_group] Actual cycle time: 1.197981ms (expected: 1ms)
// [2025-12-12T03:34:07.184702597Z INFO  tokio_single_group] Actual cycle time: 1.187361ms (expected: 1ms)
// [2025-12-12T03:34:07.185877018Z INFO  tokio_single_group] Actual cycle time: 1.174397ms (expected: 1ms)
// [2025-12-12T03:34:07.187080512Z INFO  tokio_single_group] Actual cycle time: 1.200824ms (expected: 1ms)
// [2025-12-12T03:34:07.187338672Z INFO  tokio_single_group] Actual cycle time: 258.723µs (expected: 1ms)
// [2025-12-12T03:34:07.188569789Z INFO  tokio_single_group] Actual cycle time: 1.231707ms (expected: 1ms)
// [2025-12-12T03:34:07.189780451Z INFO  tokio_single_group] Actual cycle time: 1.211618ms (expected: 1ms)
// [2025-12-12T03:34:07.190962214Z INFO  tokio_single_group] Actual cycle time: 1.182002ms (expected: 1ms)
// [2025-12-12T03:34:07.192164338Z INFO  tokio_single_group] Actual cycle time: 1.200168ms (expected: 1ms)
// [2025-12-12T03:34:07.192399296Z INFO  tokio_single_group] Actual cycle time: 236.864µs (expected: 1ms)
// [2025-12-12T03:34:07.193601576Z INFO  tokio_single_group] Actual cycle time: 1.201414ms (expected: 1ms)
// [2025-12-12T03:34:07.194791336Z INFO  tokio_single_group] Actual cycle time: 1.190773ms (expected: 1ms)
// [2025-12-12T03:34:07.196001790Z INFO  tokio_single_group] Actual cycle time: 1.210189ms (expected: 1ms)
// [2025-12-12T03:34:07.197219343Z INFO  tokio_single_group] Actual cycle time: 1.216004ms (expected: 1ms)
// [2025-12-12T03:34:07.197445097Z INFO  tokio_single_group] Actual cycle time: 227.691µs (expected: 1ms)
// [2025-12-12T03:34:07.198662123Z INFO  tokio_single_group] Actual cycle time: 1.215136ms (expected: 1ms)
// [2025-12-12T03:34:07.199870841Z INFO  tokio_single_group] Actual cycle time: 1.208972ms (expected: 1ms)
// [2025-12-12T03:34:07.201060682Z INFO  tokio_single_group] Actual cycle time: 1.191452ms (expected: 1ms)
// [2025-12-12T03:34:07.201300960Z INFO  tokio_single_group] Actual cycle time: 240.499µs (expected: 1ms)
// [2025-12-12T03:34:07.202505781Z INFO  tokio_single_group] Actual cycle time: 1.203653ms (expected: 1ms)
// [2025-12-12T03:34:07.203799460Z INFO  tokio_single_group] Actual cycle time: 1.292884ms (expected: 1ms)
// [2025-12-12T03:34:07.205003515Z INFO  tokio_single_group] Actual cycle time: 1.20543ms (expected: 1ms)
// [2025-12-12T03:34:07.206173847Z INFO  tokio_single_group] Actual cycle time: 1.170714ms (expected: 1ms)
// [2025-12-12T03:34:07.206431940Z INFO  tokio_single_group] Actual cycle time: 256.652µs (expected: 1ms)
// [2025-12-12T03:34:07.207636736Z INFO  tokio_single_group] Actual cycle time: 1.204321ms (expected: 1ms)
// [2025-12-12T03:34:07.208875212Z INFO  tokio_single_group] Actual cycle time: 1.239004ms (expected: 1ms)
// [2025-12-12T03:34:07.210105783Z INFO  tokio_single_group] Actual cycle time: 1.231272ms (expected: 1ms)
// [2025-12-12T03:34:07.210337786Z INFO  tokio_single_group] Actual cycle time: 232.673µs (expected: 1ms)
// [2025-12-12T03:34:07.211515183Z INFO  tokio_single_group] Actual cycle time: 1.177444ms (expected: 1ms)
// [2025-12-12T03:34:07.212768589Z INFO  tokio_single_group] Actual cycle time: 1.251841ms (expected: 1ms)
// [2025-12-12T03:34:07.214009994Z INFO  tokio_single_group] Actual cycle time: 1.240882ms (expected: 1ms)
// [2025-12-12T03:34:07.214228662Z INFO  tokio_single_group] Actual cycle time: 218.813µs (expected: 1ms)
// [2025-12-12T03:34:07.215444977Z INFO  tokio_single_group] Actual cycle time: 1.217101ms (expected: 1ms)
// [2025-12-12T03:34:07.216625137Z INFO  tokio_single_group] Actual cycle time: 1.180927ms (expected: 1ms)
// [2025-12-12T03:34:07.217795769Z INFO  tokio_single_group] Actual cycle time: 1.170776ms (expected: 1ms)
// [2025-12-12T03:34:07.218988628Z INFO  tokio_single_group] Actual cycle time: 1.192278ms (expected: 1ms)
// [2025-12-12T03:34:07.220181202Z INFO  tokio_single_group] Actual cycle time: 1.192971ms (expected: 1ms)
// [2025-12-12T03:34:07.220438846Z INFO  tokio_single_group] Actual cycle time: 256.141µs (expected: 1ms)
// [2025-12-12T03:34:07.221636245Z INFO  tokio_single_group] Actual cycle time: 1.198728ms (expected: 1ms)
// [2025-12-12T03:34:07.222821106Z INFO  tokio_single_group] Actual cycle time: 1.185421ms (expected: 1ms)
// [2025-12-12T03:34:07.224037439Z INFO  tokio_single_group] Actual cycle time: 1.21438ms (expected: 1ms)
// [2025-12-12T03:34:07.224267635Z INFO  tokio_single_group] Actual cycle time: 231.769µs (expected: 1ms)
// [2025-12-12T03:34:07.225484963Z INFO  tokio_single_group] Actual cycle time: 1.215973ms (expected: 1ms)
// [2025-12-12T03:34:07.226677704Z INFO  tokio_single_group] Actual cycle time: 1.194223ms (expected: 1ms)
// [2025-12-12T03:34:07.227855750Z INFO  tokio_single_group] Actual cycle time: 1.178515ms (expected: 1ms)
// [2025-12-12T03:34:07.229070334Z INFO  tokio_single_group] Actual cycle time: 1.212378ms (expected: 1ms)
// [2025-12-12T03:34:07.229296762Z INFO  tokio_single_group] Actual cycle time: 227.874µs (expected: 1ms)
// [2025-12-12T03:34:07.230507191Z INFO  tokio_single_group] Actual cycle time: 1.20903ms (expected: 1ms)
// [2025-12-12T03:34:07.231703689Z INFO  tokio_single_group] Actual cycle time: 1.198388ms (expected: 1ms)
// [2025-12-12T03:34:07.232886613Z INFO  tokio_single_group] Actual cycle time: 1.183014ms (expected: 1ms)
// [2025-12-12T03:34:07.234086328Z INFO  tokio_single_group] Actual cycle time: 1.198189ms (expected: 1ms)
// [2025-12-12T03:34:07.234319312Z INFO  tokio_single_group] Actual cycle time: 232.883µs (expected: 1ms)
// [2025-12-12T03:34:07.235522135Z INFO  tokio_single_group] Actual cycle time: 1.203209ms (expected: 1ms)
// [2025-12-12T03:34:07.236704518Z INFO  tokio_single_group] Actual cycle time: 1.18347ms (expected: 1ms)
// [2025-12-12T03:34:07.237879586Z INFO  tokio_single_group] Actual cycle time: 1.175199ms (expected: 1ms)
// [2025-12-12T03:34:07.239073245Z INFO  tokio_single_group] Actual cycle time: 1.192107ms (expected: 1ms)
// [2025-12-12T03:34:07.239299562Z INFO  tokio_single_group] Actual cycle time: 227.902µs (expected: 1ms)
// [2025-12-12T03:34:07.240502539Z INFO  tokio_single_group] Actual cycle time: 1.202394ms (expected: 1ms)
// [2025-12-12T03:34:07.241686061Z INFO  tokio_single_group] Actual cycle time: 1.184165ms (expected: 1ms)
// [2025-12-12T03:34:07.242862080Z INFO  tokio_single_group] Actual cycle time: 1.176145ms (expected: 1ms)
// [2025-12-12T03:34:07.244065198Z INFO  tokio_single_group] Actual cycle time: 1.202932ms (expected: 1ms)
// [2025-12-12T03:34:07.244287513Z INFO  tokio_single_group] Actual cycle time: 222.485µs (expected: 1ms)
// [2025-12-12T03:34:07.245477368Z INFO  tokio_single_group] Actual cycle time: 1.189343ms (expected: 1ms)
// [2025-12-12T03:34:07.246658152Z INFO  tokio_single_group] Actual cycle time: 1.181276ms (expected: 1ms)
// [2025-12-12T03:34:07.247842487Z INFO  tokio_single_group] Actual cycle time: 1.184326ms (expected: 1ms)
// [2025-12-12T03:34:07.249032394Z INFO  tokio_single_group] Actual cycle time: 1.189783ms (expected: 1ms)
// [2025-12-12T03:34:07.249266307Z INFO  tokio_single_group] Actual cycle time: 233.782µs (expected: 1ms)
// [2025-12-12T03:34:07.250506804Z INFO  tokio_single_group] Actual cycle time: 1.238699ms (expected: 1ms)
