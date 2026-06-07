use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::{PathMap, QueryResult, Record};
use crate::ast::{CopyStatement, Expr};
use crate::exec::expr::evaluate_expr;
use issundb_core::Graph;

pub(super) fn execute_copy(
    graph: &Graph,
    stmt: &CopyStatement,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, String> {
    // 1. Evaluate options
    let eval_opt =
        |expr: &Expr| -> Option<Value> { evaluate_expr(graph, &PathMap::new(), expr, params).ok() };

    let mut has_header = true;
    let mut delimiter = ',';
    let mut format = None;

    if let Some(ref opts) = stmt.options {
        if let Some(expr) = opts.get("header") {
            if let Some(Value::Bool(b)) = eval_opt(expr) {
                has_header = b;
            }
        }
        if let Some(expr) = opts.get("delimiter").or_else(|| opts.get("delim")) {
            if let Some(Value::String(s)) = eval_opt(expr) {
                if let Some(c) = s.chars().next() {
                    delimiter = c;
                }
            }
        }
        if let Some(expr) = opts.get("format") {
            if let Some(Value::String(s)) = eval_opt(expr) {
                format = Some(s.to_lowercase());
            }
        }
    }

    // 2. Open and parse file
    let file = File::open(&stmt.filepath)
        .map_err(|e| format!("failed to open file '{}': {}", stmt.filepath, e))?;
    let reader = BufReader::new(file);

    let inferred_format = format.unwrap_or_else(|| {
        let path = Path::new(&stmt.filepath);
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("jsonl") | Some("ndjson") => "jsonl".to_string(),
            _ => "csv".to_string(),
        }
    });

    let mut count = 0usize;

    if inferred_format == "jsonl" {
        let mut entries = Vec::new();
        for (i, line_res) in reader.lines().enumerate() {
            let line = line_res.map_err(|e| format!("error reading line {}: {}", i + 1, e))?;
            if line.trim().is_empty() {
                continue;
            }
            let val: Value = serde_json::from_str(&line)
                .map_err(|e| format!("JSON parse error on line {}: {}", i + 1, e))?;

            let props = if let Value::Object(ref obj) = val {
                if let Some(p) = obj.get("props") {
                    p.clone()
                } else {
                    val.clone()
                }
            } else {
                val
            };
            entries.push(props);
        }

        // Perform bulk insert in a single transaction
        graph
            .update(|txn| {
                for props in &entries {
                    txn.add_node(&stmt.target, props)?;
                    count += 1;
                }
                Ok(())
            })
            .map_err(|e| format!("JSONL import failed: {}", e))?;
    } else {
        // CSV format
        let mut lines = reader.lines().enumerate();
        let mut headers = Vec::new();

        if has_header {
            if let Some((_, line_res)) = lines.next() {
                let line = line_res.map_err(|e| format!("failed to read CSV header: {}", e))?;
                headers = parse_csv_line(&line, delimiter);
            } else {
                return Err("CSV file is empty".to_string());
            }
        }

        let mut entries = Vec::new();
        for (i, line_res) in lines {
            let line = line_res.map_err(|e| format!("error reading CSV line {}: {}", i + 1, e))?;
            if line.trim().is_empty() {
                continue;
            }
            let cols = parse_csv_line(&line, delimiter);
            if headers.is_empty() {
                // Generate default headers col0, col1, ...
                headers = (0..cols.len()).map(|idx| format!("col{}", idx)).collect();
            }

            let mut props = serde_json::Map::new();
            for (j, header) in headers.iter().enumerate() {
                let val_str = cols.get(j).map(|s| s.as_str()).unwrap_or("");
                let val = if val_str.is_empty() {
                    Value::Null
                } else if let Ok(n) = val_str.parse::<i64>() {
                    Value::Number(n.into())
                } else if let Ok(f) = val_str.parse::<f64>() {
                    serde_json::json!(f)
                } else if val_str.eq_ignore_ascii_case("true") {
                    Value::Bool(true)
                } else if val_str.eq_ignore_ascii_case("false") {
                    Value::Bool(false)
                } else {
                    Value::String(val_str.to_owned())
                };
                props.insert(header.clone(), val);
            }
            entries.push(Value::Object(props));
        }

        // Perform bulk insert in a single transaction
        graph
            .update(|txn| {
                for props in &entries {
                    txn.add_node(&stmt.target, props)?;
                    count += 1;
                }
                Ok(())
            })
            .map_err(|e| format!("CSV import failed: {}", e))?;
    }

    // Rebuild the CSR snapshot cache so the imported nodes are available immediately
    graph
        .rebuild_csr()
        .map_err(|e| format!("failed to rebuild CSR after import: {}", e))?;

    Ok(QueryResult {
        columns: vec!["nodes_imported".to_string()],
        records: vec![Record {
            values: vec![Value::Number(count.into())],
        }],
    })
}

fn parse_csv_line(s: &str, delimiter: char) -> Vec<String> {
    let mut cols = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '"' {
            if in_quotes && chars.peek() == Some(&'"') {
                chars.next();
                current.push('"');
            } else {
                in_quotes = !in_quotes;
            }
        } else if c == delimiter && !in_quotes {
            cols.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    cols.push(current.trim().to_owned());
    cols
}
