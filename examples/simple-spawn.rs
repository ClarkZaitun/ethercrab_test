//! Simple test to measure tokio::spawn overhead

use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Simple tokio::spawn overhead test...\n");

    // Warm up the runtime
    for _ in 0..100 {
        let h = tokio::spawn(async {});
        h.await.unwrap();
    }

    // Test spawn overhead with minimal work
    println!("Testing spawn overhead...");
    let iterations = 10000;
    let start = Instant::now();

    for i in 0..iterations {
        let h = tokio::spawn(async move { i });
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

    // Compare with direct function call
    println!("\nTesting direct function call (baseline)...");
    let start = Instant::now();

    for i in 0..iterations {
        let result = i;
        assert_eq!(result, i);
    }

    let duration = start.elapsed();
    let avg_duration = duration / iterations;

    println!("Total time for {} direct calls: {:?}", iterations, duration);
    println!("Average time per call: {:?}", avg_duration);
    println!(
        "Calls per second: {:.0}",
        iterations as f64 / duration.as_secs_f64()
    );

    Ok(())
}
