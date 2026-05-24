use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use issundb::GraphQueryExt;

#[test]
fn test_opencypher_conformance() {
    // Gate conformance tests on ISSUNDB_CONFORMANCE=1 to keep default cargo test fast
    if std::env::var("ISSUNDB_CONFORMANCE").is_err() {
        println!("Skipping openCypher conformance tests. Set ISSUNDB_CONFORMANCE=1 to execute.");
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let features_dir = PathBuf::from(manifest_dir)
        .join("tests")
        .join("conformance")
        .join("features");

    if !features_dir.exists() {
        panic!(
            "Conformance features directory not found at: {:?}",
            features_dir
        );
    }

    let entries = fs::read_dir(&features_dir)
        .unwrap_or_else(|e| panic!("failed to read features directory: {}", e));

    let mut parsed_scenarios_count = 0;
    let mut passed_scenarios_count = 0;

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "feature") {
            println!(
                "Running conformance feature: {:?}",
                path.file_name().unwrap()
            );
            let scenarios = parse_feature_file(&path).unwrap_or_else(|e| {
                panic!("failed to parse Gherkin feature file {:?}: {}", path, e)
            });

            for scenario in &scenarios {
                parsed_scenarios_count += 1;
                print!("  Scenario: {} ... ", scenario.name);
                match run_scenario(scenario) {
                    Ok(_) => {
                        println!("passed");
                        passed_scenarios_count += 1;
                    }
                    Err(e) => {
                        println!("FAILED\n\nError: {}\n", e);
                        panic!("Scenario '{}' in {:?} failed", scenario.name, path);
                    }
                }
            }
        }
    }

    println!(
        "\nConformance summary: {} scenarios parsed, {} scenarios passed.",
        parsed_scenarios_count, passed_scenarios_count
    );
    assert!(
        parsed_scenarios_count > 0,
        "No conformance scenarios were parsed!"
    );
}

#[derive(Debug)]
struct Scenario {
    name: String,
    setup_queries: Vec<String>,
    query: String,
    expected_columns: Vec<String>,
    expected_rows: Vec<Vec<serde_json::Value>>,
}

fn parse_feature_file(path: &Path) -> Result<Vec<Scenario>, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut scenarios = Vec::new();
    let mut current_scenario: Option<Scenario> = None;

    let mut in_query_block = false;
    let mut query_block_accumulator = Vec::new();
    let mut query_type = ""; // "setup" or "when"
    let mut pending_query_type: Option<&'static str> = None;

    let mut in_table = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if in_query_block {
            if trimmed == "\"\"\"" {
                in_query_block = false;
                let full_query = query_block_accumulator.join("\n");
                query_block_accumulator.clear();

                if let Some(ref mut scen) = current_scenario {
                    if query_type == "setup" {
                        scen.setup_queries.push(full_query);
                    } else if query_type == "when" {
                        scen.query = full_query;
                    }
                }
            } else {
                query_block_accumulator.push(line.to_string());
            }
            continue;
        }

        if trimmed.starts_with("Scenario:") {
            if let Some(scen) = current_scenario.take() {
                scenarios.push(scen);
            }
            let name = trimmed["Scenario:".len()..].trim().to_string();
            current_scenario = Some(Scenario {
                name,
                setup_queries: Vec::new(),
                query: String::new(),
                expected_columns: Vec::new(),
                expected_rows: Vec::new(),
            });
            in_table = false;
            pending_query_type = None;
            continue;
        }

        if trimmed.starts_with("Given ") || trimmed.starts_with("And having executed:") {
            if trimmed.contains("\"\"\"") {
                in_query_block = true;
                query_type = "setup";
            } else {
                pending_query_type = Some("setup");
            }
            in_table = false;
            continue;
        }

        if trimmed.starts_with("When executing query:") {
            if trimmed.contains("\"\"\"") {
                in_query_block = true;
                query_type = "when";
            } else {
                pending_query_type = Some("when");
            }
            in_table = false;
            continue;
        }

        if trimmed == "\"\"\"" {
            if let Some(pq) = pending_query_type.take() {
                in_query_block = true;
                query_type = pq;
            }
            continue;
        }

        if trimmed.starts_with("Then the result should be:") {
            in_table = true;
            continue;
        }

        if in_table && trimmed.starts_with('|') && trimmed.ends_with('|') {
            let parts: Vec<&str> = trimmed[1..trimmed.len() - 1].split('|').collect();
            if let Some(ref mut scen) = current_scenario {
                if scen.expected_columns.is_empty() {
                    scen.expected_columns = parts.iter().map(|p| p.trim().to_string()).collect();
                } else {
                    let row_vals = parts.iter().map(|p| parse_table_value(p)).collect();
                    scen.expected_rows.push(row_vals);
                }
            }
            continue;
        }
    }

    if let Some(scen) = current_scenario {
        scenarios.push(scen);
    }

    Ok(scenarios)
}

fn parse_table_value(s: &str) -> serde_json::Value {
    let trimmed = s.trim();
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        serde_json::Value::String(trimmed[1..trimmed.len() - 1].to_string())
    } else if trimmed.eq_ignore_ascii_case("true") {
        serde_json::Value::Bool(true)
    } else if trimmed.eq_ignore_ascii_case("false") {
        serde_json::Value::Bool(false)
    } else if trimmed.eq_ignore_ascii_case("null") {
        serde_json::Value::Null
    } else if let Ok(val) = trimmed.parse::<i64>() {
        serde_json::Value::Number(val.into())
    } else if let Ok(val) = trimmed.parse::<f64>() {
        serde_json::Number::from_f64(val)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::String(trimmed.to_string())
    }
}

fn run_scenario(scenario: &Scenario) -> Result<(), String> {
    let temp_dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let graph = issundb::Graph::open(temp_dir.path(), 1).map_err(|e| e.to_string())?;

    // 1. Run setup queries
    for setup_query in &scenario.setup_queries {
        let params = HashMap::new();
        let _ = graph.query_with_params(setup_query, &params)?;
    }

    // Rebuild CSR after setups to ensure optimized plans traverse CSR cleanly
    graph.rebuild_csr().map_err(|e| e.to_string())?;

    // 2. Run target query
    let params = HashMap::new();
    let res = graph.query_with_params(&scenario.query, &params)?;

    // 3. Assert columns match
    if res.columns != scenario.expected_columns {
        return Err(format!(
            "Column mismatch.\nExpected: {:?}\nActual: {:?}",
            scenario.expected_columns, res.columns
        ));
    }

    // 4. Assert records match (order-insensitive)
    let mut actual_rows: Vec<Vec<serde_json::Value>> =
        res.records.into_iter().map(|r| r.values).collect();

    let mut expected_rows = scenario.expected_rows.clone();

    // Sort actual and expected rows to compare them in an order-insensitive way.
    actual_rows.sort_by_key(|r| {
        r.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|")
    });
    expected_rows.sort_by_key(|r| {
        r.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|")
    });

    if actual_rows != expected_rows {
        return Err(format!(
            "Row values mismatch.\nExpected: {:#?}\nActual: {:#?}",
            expected_rows, actual_rows
        ));
    }

    Ok(())
}
