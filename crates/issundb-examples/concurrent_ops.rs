//! An example that shows concurrent read and write operations in IssunDB.
//!
//! Because the `Graph` handle is cheap to clone (all internal state is behind `Arc`),
//! it is perfectly safe to share and clone across multiple threads.
//!
//! This example:
//!   1. Opens a temporary graph database.
//!   2. Spawns multiple concurrent reader threads that query the graph using Cypher.
//!   3. Spawns a writer thread that continuously inserts and updates nodes.
//!   4. Demonstrates transactional isolation (readers get consistent snapshots).

use issundb::{Graph, GraphQueryExt};
use serde_json::json;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open a temporary database
    let dir = TempDir::new()?;
    let graph = Graph::open(dir.path(), 1)?;

    println!("IssunDB Concurrent Operations Showcase");
    println!("======================================\n");

    // Initialize with some seed data
    let sensor_a = graph.add_node(
        "Sensor",
        &json!({ "id": "A", "status": "active", "reading": 22.5 }),
    )?;
    let sensor_b = graph.add_node(
        "Sensor",
        &json!({ "id": "B", "status": "active", "reading": 18.0 }),
    )?;
    graph.add_edge(sensor_a, sensor_b, "CONNECTED_TO", &json!({}))?;
    graph.rebuild_csr()?;

    println!("Database initialized with Sensor nodes A and B.");
    println!("Starting concurrent readers and writers...\n");

    // Create an Arc wrapper around Graph to share it with threads (even though Graph itself is Clone,
    // wrapping it in Arc or simply cloning it is identical since Graph has internal Arcs).
    // Let's just clone the `Graph` directly to show it's cheap to clone.
    let reader_graph_1 = graph.clone();
    let reader_graph_2 = graph.clone();
    let writer_graph = graph.clone();

    // Flag to coordinate thread shutdown
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_reader1 = running.clone();
    let running_reader2 = running.clone();
    let running_writer = running.clone();

    // Spawn Reader Thread 1: queries sensor readings using Cypher
    let reader1 = thread::spawn(move || {
        let mut query_count = 0;
        while running_reader1.load(std::sync::atomic::Ordering::Relaxed) {
            match reader_graph_1.query("MATCH (s:Sensor) RETURN s.id, s.reading ORDER BY s.id") {
                Ok(result) => {
                    query_count += 1;
                    if query_count % 5 == 0 {
                        print!("[Reader 1] Query #{}: ", query_count);
                        for record in result.records {
                            let id = record
                                .values
                                .first()
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let reading =
                                record.values.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                            print!("{}={:.1}°C  ", id, reading);
                        }
                        println!();
                    }
                }
                Err(e) => eprintln!("[Reader 1] Error: {:?}", e),
            }
            thread::sleep(Duration::from_millis(50));
        }
        println!("[Reader 1] Stopped. Total queries: {}", query_count);
    });

    // Spawn Reader Thread 2: queries average sensor reading
    let reader2 = thread::spawn(move || {
        let mut query_count = 0;
        while running_reader2.load(std::sync::atomic::Ordering::Relaxed) {
            // Using a Cypher match for all sensors
            match reader_graph_2.query("MATCH (s:Sensor) RETURN s.reading") {
                Ok(result) => {
                    query_count += 1;
                    let values: Vec<f64> = result
                        .records
                        .iter()
                        .filter_map(|r| r.values.first().and_then(|v| v.as_f64()))
                        .collect();

                    if !values.is_empty() && query_count % 5 == 0 {
                        let avg: f64 = values.iter().sum::<f64>() / values.len() as f64;
                        println!(
                            "[Reader 2] Query #{}: Calculated average temperature of {} sensors: {:.2}°C",
                            query_count,
                            values.len(),
                            avg
                        );
                    }
                }
                Err(e) => eprintln!("[Reader 2] Error: {:?}", e),
            }
            thread::sleep(Duration::from_millis(60));
        }
        println!("[Reader 2] Stopped. Total queries: {}", query_count);
    });

    // Spawn Writer Thread: updates sensor readings and adds new sensors periodically
    let writer = thread::spawn(move || {
        let mut step = 0;
        while running_writer.load(std::sync::atomic::Ordering::Relaxed) {
            step += 1;

            // 1. Update existing sensor readings
            let temp_a = 20.0 + (step as f64 * 0.3).sin() * 5.0;
            let temp_b = 18.0 + (step as f64 * 0.4).cos() * 3.0;

            if let Err(e) = writer_graph.update_node(
                sensor_a,
                &json!({ "id": "A", "status": "active", "reading": temp_a }),
            ) {
                eprintln!("[Writer] Failed to update Sensor A: {:?}", e);
            }
            if let Err(e) = writer_graph.update_node(
                sensor_b,
                &json!({ "id": "B", "status": "active", "reading": temp_b }),
            ) {
                eprintln!("[Writer] Failed to update Sensor B: {:?}", e);
            }

            // 2. Every 10 steps, add a temporary sensor and delete it 5 steps later
            if step % 10 == 0 {
                let sensor_id = format!("C-{}", step);
                println!("[Writer] Adding new Sensor {}...", sensor_id);
                match writer_graph.add_node(
                    "Sensor",
                    &json!({ "id": sensor_id, "status": "temporary", "reading": 21.0 }),
                ) {
                    Ok(new_sensor) => {
                        // Sleep a bit, then delete it to simulate dynamic topological changes
                        thread::sleep(Duration::from_millis(200));
                        println!("[Writer] Deleting Sensor {}...", sensor_id);
                        if let Err(e) = writer_graph.delete_node(new_sensor) {
                            eprintln!("[Writer] Failed to delete Sensor {}: {:?}", sensor_id, e);
                        }
                    }
                    Err(e) => eprintln!("[Writer] Failed to add sensor: {:?}", e),
                }
            }

            thread::sleep(Duration::from_millis(100));
        }
        println!("[Writer] Stopped.");
    });

    // Let the workload run for 2 seconds
    thread::sleep(Duration::from_secs(2));

    // Signal shutdown and join threads
    println!("\nShutting down threads...");
    running.store(false, std::sync::atomic::Ordering::Relaxed);

    reader1.join().unwrap();
    reader2.join().unwrap();
    writer.join().unwrap();

    println!("\nWorkload complete. All threads successfully terminated.");
    Ok(())
}
