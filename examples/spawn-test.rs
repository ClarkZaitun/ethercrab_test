//! Test program to measure the overhead of tokio::spawn operation

use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing tokio::spawn overhead...");

    // Warm up the runtime
    for _ in 0..100 {
        let h = tokio::spawn(async {});
        h.await.unwrap();
    }

    // Measure spawn overhead
    let iterations = 10000;
    let start = Instant::now();

    for i in 0..iterations {
        let h = tokio::spawn(async move {
            // Minimal work - just return the iteration number
            i
        });
        let result = h.await.unwrap();
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("Total time for {} spawns: {:?}", iterations, duration);
    println!("Average time per spawn: {:?}", avg_duration);
    println!(
        "Spawns per second: {:.0}",
        iterations as f64 / duration.as_secs_f64()
    );

    // Measure spawn overhead with sleep
    println!("\nTesting spawn with sleep...");
    let start = Instant::now();

    for i in 0..1000 {
        let h = tokio::spawn(async move {
            // Sleep for a tiny amount of time
            tokio::time::sleep(Duration::from_nanos(1)).await;
            i
        });
        let result = h.await.unwrap();
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    println!("Total time for 1000 spawns with sleep: {:?}", duration);

    Ok(())
}

// Alternative implementation using explicit runtime
fn main_alt() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        println!("Testing tokio::spawn overhead (alternative)...");

        // Warm up the runtime
        for _ in 0..100 {
            let h = tokio::spawn(async {});
            h.await.unwrap();
        }

        // Measure spawn overhead
        let iterations = 10000;
        let start = Instant::now();

        for i in 0..iterations {
            let h = tokio::spawn(async move {
                // Minimal work - just return the iteration number
                i
            });
            let result = h.await.unwrap();
            assert_eq!(result, i);
        }

        let duration = start.elapsed();
        let avg_duration = duration / iterations;

        println!("Total time for {} spawns: {:?}", iterations, duration);
        println!("Average time per spawn: {:?}", avg_duration);
        println!(
            "Spawns per second: {:.0}",
            iterations as f64 / duration.as_secs_f64()
        );
    });

    Ok(())
}
