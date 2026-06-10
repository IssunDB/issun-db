use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use super::{PathMap, QueryResult, Record};
use crate::ast::{CopyStatement, ExportDatabaseStatement, Expr, ImportDatabaseStatement};
use crate::exec::expr::evaluate_expr;
use issundb_core::Graph;

pub(super) fn execute_copy(
    graph: &Graph,
    stmt: &CopyStatement,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, String> {
    let mut id_map = HashMap::new();
    let count = execute_copy_internal(graph, stmt, params, &mut id_map)?;

    // Rebuild the CSR snapshot cache so the imported nodes/edges are available immediately.
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

pub(super) fn execute_copy_internal(
    graph: &Graph,
    stmt: &CopyStatement,
    params: &HashMap<String, Value>,
    id_map: &mut HashMap<u64, u64>,
) -> Result<usize, String> {
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
    let inferred_format = format.unwrap_or_else(|| {
        let path = Path::new(&stmt.filepath);
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("jsonl") | Some("ndjson") => "jsonl".to_string(),
            Some("parquet") => "parquet".to_string(),
            _ => "csv".to_string(),
        }
    });

    let mut count = 0usize;
    let mut entries = Vec::new();
    let mut headers_found = Vec::new();

    if inferred_format == "parquet" {
        entries = read_parquet_entries(Path::new(&stmt.filepath))?;
    } else if inferred_format == "jsonl" {
        let file = File::open(&stmt.filepath)
            .map_err(|e| format!("failed to open file '{}': {}", stmt.filepath, e))?;
        let reader = BufReader::new(file);
        for (i, line_res) in reader.lines().enumerate() {
            let line = line_res.map_err(|e| format!("error reading line {}: {}", i + 1, e))?;
            if line.trim().is_empty() {
                continue;
            }
            let val: Value = serde_json::from_str(&line)
                .map_err(|e| format!("JSON parse error on line {}: {}", i + 1, e))?;

            let mut obj = val
                .as_object()
                .ok_or_else(|| format!("line {}: JSONL row must be a JSON object", i + 1))?
                .clone();

            if let Some(props_val) = obj.get("props") {
                if let Some(props_obj) = props_val.as_object() {
                    let props_obj = props_obj.clone();
                    obj.remove("props");
                    for (k, v) in props_obj {
                        obj.insert(k, v);
                    }
                }
            }

            entries.push(obj);
        }
    } else {
        // CSV format
        let file = File::open(&stmt.filepath)
            .map_err(|e| format!("failed to open file '{}': {}", stmt.filepath, e))?;
        let reader = BufReader::new(file);
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

        for (i, line_res) in lines {
            let line = line_res.map_err(|e| format!("error reading CSV line {}: {}", i + 1, e))?;
            if line.trim().is_empty() {
                continue;
            }
            let cols = parse_csv_line(&line, delimiter);
            if headers.is_empty() {
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
                } else if (val_str.starts_with('[') && val_str.ends_with(']'))
                    || (val_str.starts_with('{') && val_str.ends_with('}'))
                {
                    serde_json::from_str(val_str)
                        .unwrap_or_else(|_| Value::String(val_str.to_owned()))
                } else {
                    Value::String(val_str.to_owned())
                };
                props.insert(header.clone(), val);
            }
            entries.push(props);
        }
        headers_found = headers;
    }

    if entries.is_empty() {
        return Ok(0);
    }

    // 3. Determine if it is a relationship import
    let is_relationship = if inferred_format == "csv" {
        (headers_found.contains(&"_from".to_string())
            || headers_found.contains(&"from".to_string()))
            && (headers_found.contains(&"_to".to_string())
                || headers_found.contains(&"to".to_string()))
    } else if let Some(first) = entries.first() {
        (first.contains_key("_from") || first.contains_key("from"))
            && (first.contains_key("_to") || first.contains_key("to"))
    } else {
        false
    };

    if is_relationship {
        graph
            .update(|txn| {
                for obj in &entries {
                    let from_raw = obj
                        .get("_from")
                        .or_else(|| obj.get("from"))
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| custom_err("missing or invalid _from ID"))?;

                    let to_raw = obj
                        .get("_to")
                        .or_else(|| obj.get("to"))
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| custom_err("missing or invalid _to ID"))?;

                    let from_id = *id_map.get(&from_raw).unwrap_or(&from_raw);
                    let to_id = *id_map.get(&to_raw).unwrap_or(&to_raw);

                    let etype_val = obj
                        .get("_type")
                        .or_else(|| obj.get("_etype"))
                        .or_else(|| obj.get("type"));

                    let etype = etype_val.and_then(|v| v.as_str()).unwrap_or(&stmt.target);

                    let mut props_filtered = serde_json::Map::new();
                    for (k, v) in obj {
                        if k != "_from"
                            && k != "from"
                            && k != "_to"
                            && k != "to"
                            && k != "_type"
                            && k != "_etype"
                            && k != "type"
                        {
                            props_filtered.insert(k.clone(), v.clone());
                        }
                    }

                    txn.add_edge(from_id, to_id, etype, &Value::Object(props_filtered))?;
                    count += 1;
                }
                Ok(())
            })
            .map_err(|e| format!("relationship import failed: {}", e))?;
    } else {
        graph
            .update(|txn| {
                for obj in &entries {
                    let old_id = obj
                        .get("_id")
                        .or_else(|| obj.get("id"))
                        .and_then(|v| v.as_u64());

                    let labels = if let Some(labels_val) =
                        obj.get("_labels").or_else(|| obj.get("labels"))
                    {
                        if let Some(arr) = labels_val.as_array() {
                            arr.iter().filter_map(|v| v.as_str()).collect::<Vec<&str>>()
                        } else if let Some(s) = labels_val.as_str() {
                            s.split([':', ';'])
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<&str>>()
                        } else {
                            vec![stmt.target.as_str()]
                        }
                    } else {
                        vec![stmt.target.as_str()]
                    };

                    let mut props_filtered = serde_json::Map::new();
                    for (k, v) in obj {
                        if k != "_id"
                            && k != "_labels"
                            && k != "labels"
                            && k != "_label"
                            && k != "label"
                        {
                            props_filtered.insert(k.clone(), v.clone());
                        }
                    }

                    let new_id = txn.add_node_multi(&labels, &Value::Object(props_filtered))?;
                    if let Some(old) = old_id {
                        id_map.insert(old, new_id);
                    }
                    count += 1;
                }
                Ok(())
            })
            .map_err(|e| format!("node import failed: {}", e))?;
    }

    Ok(count)
}

pub(super) fn execute_export_db(
    graph: &Graph,
    stmt: &ExportDatabaseStatement,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, String> {
    let eval_opt =
        |expr: &Expr| -> Option<Value> { evaluate_expr(graph, &PathMap::new(), expr, params).ok() };

    let mut format = "jsonl".to_string();
    if let Some(ref opts) = stmt.options {
        if let Some(expr) = opts.get("format") {
            if let Some(Value::String(s)) = eval_opt(expr) {
                format = s.to_lowercase();
            }
        }
    }

    let dir = Path::new(&stmt.filepath);
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create export directory: {}", e))?;

    // Export nodes
    let all_nodes = graph.all_nodes().map_err(|e| e.to_string())?;

    let nodes_file_name = if format == "csv" {
        "nodes.csv"
    } else if format == "parquet" {
        "nodes.parquet"
    } else {
        "nodes.jsonl"
    };
    let nodes_path = dir.join(nodes_file_name);

    if format == "parquet" {
        let mut entries = Vec::new();
        for &nid in &all_nodes {
            let record = graph
                .get_node(nid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node {} not found", nid))?;

            let mut labels = Vec::new();
            for &lid in &record.labels {
                if let Some(lname) = graph.label_name(lid).map_err(|e| e.to_string())? {
                    labels.push(lname);
                }
            }

            let props: Value = rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

            let mut obj = serde_json::Map::new();
            obj.insert("_id".to_string(), Value::Number(nid.into()));
            obj.insert(
                "_labels".to_string(),
                Value::Array(labels.into_iter().map(Value::String).collect()),
            );
            if let Some(props_obj) = props.as_object() {
                for (k, v) in props_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
            entries.push(obj);
        }

        let mut node_keys = BTreeSet::new();
        for obj in &entries {
            for k in obj.keys() {
                if k != "_id" && k != "_labels" {
                    node_keys.insert(k.clone());
                }
            }
        }

        let col_types = determine_column_types(&entries, &node_keys);
        let batch = build_record_batch(&entries, &node_keys, &col_types, false)?;
        write_parquet_file(&nodes_path, &batch)?;
    } else if format == "csv" {
        let mut node_keys = BTreeSet::new();
        for &nid in &all_nodes {
            let record = graph
                .get_node(nid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node {} not found", nid))?;
            let props: Value = rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
            if let Some(obj) = props.as_object() {
                for k in obj.keys() {
                    node_keys.insert(k.clone());
                }
            }
        }

        let mut nodes_file =
            File::create(&nodes_path).map_err(|e| format!("failed to create nodes file: {}", e))?;

        let mut header_cols = vec!["_id".to_string(), "_labels".to_string()];
        header_cols.extend(node_keys.iter().cloned());
        writeln!(nodes_file, "{}", header_cols.join(",")).map_err(|e| e.to_string())?;

        for &nid in &all_nodes {
            let record = graph
                .get_node(nid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node {} not found", nid))?;

            let mut labels = Vec::new();
            for &lid in &record.labels {
                if let Some(lname) = graph.label_name(lid).map_err(|e| e.to_string())? {
                    labels.push(lname);
                }
            }
            let labels_str = labels.join(":");

            let props: Value = rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
            let props_obj = props.as_object();

            let mut row = vec![nid.to_string(), escape_csv_string(&labels_str)];
            for k in &node_keys {
                let val = props_obj.and_then(|obj| obj.get(k)).unwrap_or(&Value::Null);
                row.push(format_csv_cell(val));
            }
            writeln!(nodes_file, "{}", row.join(",")).map_err(|e| e.to_string())?;
        }
    } else {
        let mut nodes_file =
            File::create(&nodes_path).map_err(|e| format!("failed to create nodes file: {}", e))?;

        for &nid in &all_nodes {
            let record = graph
                .get_node(nid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("node {} not found", nid))?;

            let mut labels = Vec::new();
            for &lid in &record.labels {
                if let Some(lname) = graph.label_name(lid).map_err(|e| e.to_string())? {
                    labels.push(lname);
                }
            }

            let props: Value = rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

            let mut obj = serde_json::Map::new();
            obj.insert("_id".to_string(), Value::Number(nid.into()));
            obj.insert(
                "_labels".to_string(),
                Value::Array(labels.into_iter().map(Value::String).collect()),
            );
            if let Some(props_obj) = props.as_object() {
                for (k, v) in props_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
            let line = serde_json::to_string(&obj).map_err(|e| e.to_string())?;
            writeln!(nodes_file, "{}", line).map_err(|e| e.to_string())?;
        }
    }

    // Export edges
    let edges_file_name = if format == "csv" {
        "edges.csv"
    } else if format == "parquet" {
        "edges.parquet"
    } else {
        "edges.jsonl"
    };
    let edges_path = dir.join(edges_file_name);

    if format == "parquet" {
        let mut entries = Vec::new();
        for &nid in &all_nodes {
            let neighbors = graph.out_neighbors(nid).map_err(|e| e.to_string())?;
            for entry in neighbors {
                let record = graph
                    .get_edge(entry.edge)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge {} not found", entry.edge))?;

                let type_name = graph
                    .type_name(entry.edge_type)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| "RELATED_TO".to_string());

                let props: Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

                let mut obj = serde_json::Map::new();
                obj.insert("_from".to_string(), Value::Number(nid.into()));
                obj.insert("_to".to_string(), Value::Number(entry.node.into()));
                obj.insert("_type".to_string(), Value::String(type_name));
                if let Some(props_obj) = props.as_object() {
                    for (k, v) in props_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                entries.push(obj);
            }
        }

        let mut edge_keys = BTreeSet::new();
        for obj in &entries {
            for k in obj.keys() {
                if k != "_from" && k != "_to" && k != "_type" {
                    edge_keys.insert(k.clone());
                }
            }
        }

        let col_types = determine_column_types(&entries, &edge_keys);
        let batch = build_record_batch(&entries, &edge_keys, &col_types, true)?;
        write_parquet_file(&edges_path, &batch)?;
    } else if format == "csv" {
        let mut edge_keys = BTreeSet::new();
        for &nid in &all_nodes {
            let neighbors = graph.out_neighbors(nid).map_err(|e| e.to_string())?;
            for entry in neighbors {
                let record = graph
                    .get_edge(entry.edge)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge {} not found", entry.edge))?;
                let props: Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                if let Some(obj) = props.as_object() {
                    for k in obj.keys() {
                        edge_keys.insert(k.clone());
                    }
                }
            }
        }

        let mut edges_file =
            File::create(&edges_path).map_err(|e| format!("failed to create edges file: {}", e))?;

        let mut header_cols = vec!["_from".to_string(), "_to".to_string(), "_type".to_string()];
        header_cols.extend(edge_keys.iter().cloned());
        writeln!(edges_file, "{}", header_cols.join(",")).map_err(|e| e.to_string())?;

        for &nid in &all_nodes {
            let neighbors = graph.out_neighbors(nid).map_err(|e| e.to_string())?;
            for entry in neighbors {
                let record = graph
                    .get_edge(entry.edge)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge {} not found", entry.edge))?;

                let type_name = graph
                    .type_name(entry.edge_type)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| "RELATED_TO".to_string());

                let props: Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;
                let props_obj = props.as_object();

                let mut row = vec![
                    nid.to_string(),
                    entry.node.to_string(),
                    escape_csv_string(&type_name),
                ];
                for k in &edge_keys {
                    let val = props_obj.and_then(|obj| obj.get(k)).unwrap_or(&Value::Null);
                    row.push(format_csv_cell(val));
                }
                writeln!(edges_file, "{}", row.join(",")).map_err(|e| e.to_string())?;
            }
        }
    } else {
        let mut edges_file =
            File::create(&edges_path).map_err(|e| format!("failed to create edges file: {}", e))?;

        for &nid in &all_nodes {
            let neighbors = graph.out_neighbors(nid).map_err(|e| e.to_string())?;
            for entry in neighbors {
                let record = graph
                    .get_edge(entry.edge)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("edge {} not found", entry.edge))?;

                let type_name = graph
                    .type_name(entry.edge_type)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| "RELATED_TO".to_string());

                let props: Value =
                    rmp_serde::from_slice(&record.props).map_err(|e| e.to_string())?;

                let mut obj = serde_json::Map::new();
                obj.insert("_from".to_string(), Value::Number(nid.into()));
                obj.insert("_to".to_string(), Value::Number(entry.node.into()));
                obj.insert("_type".to_string(), Value::String(type_name));
                if let Some(props_obj) = props.as_object() {
                    for (k, v) in props_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                let line = serde_json::to_string(&obj).map_err(|e| e.to_string())?;
                writeln!(edges_file, "{}", line).map_err(|e| e.to_string())?;
            }
        }
    }

    // Write schema.cypher
    let schema_path = dir.join("schema.cypher");
    let mut schema_file =
        File::create(&schema_path).map_err(|e| format!("failed to create schema file: {}", e))?;

    let node_idx = graph
        .list_node_indexes_and_constraints()
        .map_err(|e| e.to_string())?;
    for (label, prop, flags) in node_idx {
        match flags {
            0x00 => writeln!(
                schema_file,
                "CREATE INDEX FOR (n:{}) ON (n.{});",
                label, prop
            )
            .map_err(|e| e.to_string())?,
            0x01 => writeln!(
                schema_file,
                "CREATE CONSTRAINT ON (n:{}) ASSERT n.{} IS UNIQUE;",
                label, prop
            )
            .map_err(|e| e.to_string())?,
            0x02 => writeln!(
                schema_file,
                "CREATE CONSTRAINT ON (n:{}) ASSERT EXISTS(n.{});",
                label, prop
            )
            .map_err(|e| e.to_string())?,
            _ => {}
        }
    }

    let edge_idx = graph
        .list_edge_indexes_and_constraints()
        .map_err(|e| e.to_string())?;
    for (etype, prop, flags) in edge_idx {
        match flags {
            0x00 => writeln!(
                schema_file,
                "CREATE INDEX FOR ()-[r:{}]-() ON (r.{});",
                etype, prop
            )
            .map_err(|e| e.to_string())?,
            0x01 => writeln!(
                schema_file,
                "CREATE CONSTRAINT ON ()-[r:{}]-() ASSERT r.{} IS UNIQUE;",
                etype, prop
            )
            .map_err(|e| e.to_string())?,
            0x02 => writeln!(
                schema_file,
                "CREATE CONSTRAINT ON ()-[r:{}]-() ASSERT EXISTS(r.{});",
                etype, prop
            )
            .map_err(|e| e.to_string())?,
            _ => {}
        }
    }

    // Write index.cypher (for text indexes)
    let index_path = dir.join("index.cypher");
    let mut index_file =
        File::create(&index_path).map_err(|e| format!("failed to create index file: {}", e))?;

    use issundb_text::TextIndexExt;
    let text_idx = graph.list_text_indexes().map_err(|e| e.to_string())?;
    for (label, prop, _lang) in text_idx {
        writeln!(
            index_file,
            "CREATE INDEX FOR (n:{}) ON (n.{});",
            label, prop
        )
        .map_err(|e| e.to_string())?;
    }

    // Write copy.cypher
    let copy_path = dir.join("copy.cypher");
    let mut copy_file =
        File::create(&copy_path).map_err(|e| format!("failed to create copy file: {}", e))?;

    if format == "csv" {
        writeln!(
            copy_file,
            "COPY nodes FROM 'nodes.csv' WITH {{format: 'csv', header: true, delimiter: ','}};"
        )
        .map_err(|e| e.to_string())?;
        writeln!(
            copy_file,
            "COPY edges FROM 'edges.csv' WITH {{format: 'csv', header: true, delimiter: ','}};"
        )
        .map_err(|e| e.to_string())?;
    } else if format == "parquet" {
        writeln!(
            copy_file,
            "COPY nodes FROM 'nodes.parquet' WITH {{format: 'parquet'}};"
        )
        .map_err(|e| e.to_string())?;
        writeln!(
            copy_file,
            "COPY edges FROM 'edges.parquet' WITH {{format: 'parquet'}};"
        )
        .map_err(|e| e.to_string())?;
    } else {
        writeln!(
            copy_file,
            "COPY nodes FROM 'nodes.jsonl' WITH {{format: 'jsonl'}};"
        )
        .map_err(|e| e.to_string())?;
        writeln!(
            copy_file,
            "COPY edges FROM 'edges.jsonl' WITH {{format: 'jsonl'}};"
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(QueryResult {
        columns: vec!["exported".to_string()],
        records: vec![Record {
            values: vec![Value::Bool(true)],
        }],
    })
}

pub(super) fn execute_import_db(
    graph: &Graph,
    stmt: &ImportDatabaseStatement,
    params: &HashMap<String, Value>,
) -> Result<QueryResult, String> {
    let dir = Path::new(&stmt.filepath);
    if !dir.is_dir() {
        return Err(format!(
            "import path '{}' is not a directory",
            stmt.filepath
        ));
    }

    // 1. Read and execute schema.cypher
    let schema_path = dir.join("schema.cypher");
    if schema_path.is_file() {
        let content = std::fs::read_to_string(&schema_path)
            .map_err(|e| format!("failed to read schema.cypher: {}", e))?;
        for raw_stmt in content.split(';') {
            let trimmed = raw_stmt.trim();
            if !trimmed.is_empty() {
                super::execute(graph, trimmed, params)
                    .map_err(|e| format!("schema error on '{}': {}", trimmed, e))?;
            }
        }
    }

    // 2. Read and execute copy.cypher with shared id mapping
    let copy_path = dir.join("copy.cypher");
    let mut id_map = HashMap::new();
    if copy_path.is_file() {
        let file =
            File::open(&copy_path).map_err(|e| format!("failed to open copy.cypher: {}", e))?;
        let reader = BufReader::new(file);

        for line_res in reader.lines() {
            let line =
                line_res.map_err(|e| format!("failed to read line from copy.cypher: {}", e))?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("--") {
                continue;
            }
            let cypher_stmt = trimmed.strip_suffix(';').unwrap_or(trimmed);

            let parsed = crate::parser::parse(cypher_stmt)
                .map_err(|e| format!("parse error on copy line '{}': {}", cypher_stmt, e))?;

            if let crate::ast::Statement::Copy(ref copy_stmt) = parsed {
                let resolved_filepath = if Path::new(&copy_stmt.filepath).is_absolute() {
                    copy_stmt.filepath.clone()
                } else {
                    dir.join(&copy_stmt.filepath).to_string_lossy().to_string()
                };

                let resolved_copy_stmt = CopyStatement {
                    target: copy_stmt.target.clone(),
                    filepath: resolved_filepath,
                    options: copy_stmt.options.clone(),
                };

                execute_copy_internal(graph, &resolved_copy_stmt, params, &mut id_map)?;
            } else {
                return Err(format!(
                    "unexpected statement in copy.cypher: {}",
                    cypher_stmt
                ));
            }
        }
    }

    // 3. Read and execute index.cypher
    let index_path = dir.join("index.cypher");
    if index_path.is_file() {
        let content = std::fs::read_to_string(&index_path)
            .map_err(|e| format!("failed to read index.cypher: {}", e))?;
        for raw_stmt in content.split(';') {
            let trimmed = raw_stmt.trim();
            if !trimmed.is_empty() {
                super::execute(graph, trimmed, params)
                    .map_err(|e| format!("index error on '{}': {}", trimmed, e))?;
            }
        }
    }

    // 4. Rebuild CSR snapshot once at the end of the entire import process.
    graph
        .rebuild_csr()
        .map_err(|e| format!("failed to rebuild CSR after import: {}", e))?;

    Ok(QueryResult {
        columns: vec!["imported".to_string()],
        records: vec![Record {
            values: vec![Value::Bool(true)],
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

fn format_csv_cell(val: &Value) -> String {
    match val {
        Value::Null => "".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => escape_csv_string(s),
        Value::Array(_) | Value::Object(_) => escape_csv_string(&val.to_string()),
    }
}

fn escape_csv_string(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        s.to_string()
    }
}

fn custom_err(msg: &str) -> issundb_core::Error {
    issundb_core::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

fn determine_column_types(
    entries: &[serde_json::Map<String, Value>],
    keys: &BTreeSet<String>,
) -> HashMap<String, arrow_schema::DataType> {
    let mut types = HashMap::new();
    for k in keys {
        let mut has_bool = false;
        let mut has_int = false;
        let mut has_float = false;
        let mut has_string = false;
        let mut has_complex = false;

        for obj in entries {
            if let Some(val) = obj.get(k) {
                match val {
                    Value::Null => {}
                    Value::Bool(_) => has_bool = true,
                    Value::Number(n) => {
                        if n.is_i64() || n.is_u64() {
                            has_int = true;
                        } else {
                            has_float = true;
                        }
                    }
                    Value::String(_) => has_string = true,
                    Value::Array(_) | Value::Object(_) => has_complex = true,
                }
            }
        }

        let dtype = if has_complex || has_string {
            arrow_schema::DataType::Utf8
        } else if has_float {
            arrow_schema::DataType::Float64
        } else if has_int {
            arrow_schema::DataType::Int64
        } else if has_bool {
            arrow_schema::DataType::Boolean
        } else {
            arrow_schema::DataType::Utf8
        };
        types.insert(k.clone(), dtype);
    }
    types
}

fn build_record_batch(
    entries: &[serde_json::Map<String, Value>],
    prop_keys: &BTreeSet<String>,
    col_types: &HashMap<String, arrow_schema::DataType>,
    is_relationship: bool,
) -> Result<arrow_array::RecordBatch, String> {
    use arrow_array::builder::{
        BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, StringBuilder,
    };
    use arrow_schema::{Field, Schema};
    use std::sync::Arc;

    let mut fields = Vec::new();
    let mut arrays: Vec<arrow_array::ArrayRef> = Vec::new();

    if is_relationship {
        fields.push(Field::new("_from", arrow_schema::DataType::Int64, false));
        fields.push(Field::new("_to", arrow_schema::DataType::Int64, false));
        fields.push(Field::new("_type", arrow_schema::DataType::Utf8, false));

        let mut from_builder = Int64Builder::with_capacity(entries.len());
        let mut to_builder = Int64Builder::with_capacity(entries.len());
        let mut type_builder = StringBuilder::new();

        for obj in entries {
            let from_val = obj
                .get("_from")
                .or_else(|| obj.get("from"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let to_val = obj
                .get("_to")
                .or_else(|| obj.get("to"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let type_val = obj
                .get("_type")
                .or_else(|| obj.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            from_builder.append_value(from_val as i64);
            to_builder.append_value(to_val as i64);
            type_builder.append_value(type_val);
        }

        arrays.push(Arc::new(from_builder.finish()));
        arrays.push(Arc::new(to_builder.finish()));
        arrays.push(Arc::new(type_builder.finish()));
    } else {
        fields.push(Field::new("_id", arrow_schema::DataType::Int64, false));
        fields.push(Field::new(
            "_labels",
            arrow_schema::DataType::List(Arc::new(Field::new(
                "item",
                arrow_schema::DataType::Utf8,
                true,
            ))),
            true,
        ));

        let mut id_builder = Int64Builder::with_capacity(entries.len());
        let mut labels_builder = ListBuilder::new(StringBuilder::new());

        for obj in entries {
            let id_val = obj
                .get("_id")
                .or_else(|| obj.get("id"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            id_builder.append_value(id_val as i64);

            let labels_val = obj.get("_labels").or_else(|| obj.get("labels"));
            if let Some(val) = labels_val {
                if let Some(arr) = val.as_array() {
                    let values_builder = labels_builder.values();
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            values_builder.append_value(s);
                        } else {
                            values_builder.append_value("");
                        }
                    }
                    labels_builder.append(true);
                } else if let Some(s) = val.as_str() {
                    let values_builder = labels_builder.values();
                    for item in s.split([':', ';']).filter(|s| !s.is_empty()) {
                        values_builder.append_value(item);
                    }
                    labels_builder.append(true);
                } else {
                    labels_builder.append(false);
                }
            } else {
                labels_builder.append(false);
            }
        }

        arrays.push(Arc::new(id_builder.finish()));
        arrays.push(Arc::new(labels_builder.finish()));
    }

    for k in prop_keys {
        let dtype = col_types
            .get(k)
            .ok_or_else(|| format!("missing type for column {}", k))?
            .clone();
        fields.push(Field::new(k, dtype.clone(), true));

        match dtype {
            arrow_schema::DataType::Boolean => {
                let mut builder = BooleanBuilder::with_capacity(entries.len());
                for obj in entries {
                    if let Some(val) = obj.get(k) {
                        if val.is_null() {
                            builder.append_null();
                        } else {
                            builder.append_value(val.as_bool().unwrap_or(false));
                        }
                    } else {
                        builder.append_null();
                    }
                }
                arrays.push(Arc::new(builder.finish()));
            }
            arrow_schema::DataType::Int64 => {
                let mut builder = Int64Builder::with_capacity(entries.len());
                for obj in entries {
                    if let Some(val) = obj.get(k) {
                        if val.is_null() {
                            builder.append_null();
                        } else {
                            builder.append_value(val.as_i64().unwrap_or(0));
                        }
                    } else {
                        builder.append_null();
                    }
                }
                arrays.push(Arc::new(builder.finish()));
            }
            arrow_schema::DataType::Float64 => {
                let mut builder = Float64Builder::with_capacity(entries.len());
                for obj in entries {
                    if let Some(val) = obj.get(k) {
                        if val.is_null() {
                            builder.append_null();
                        } else {
                            builder.append_value(val.as_f64().unwrap_or(0.0));
                        }
                    } else {
                        builder.append_null();
                    }
                }
                arrays.push(Arc::new(builder.finish()));
            }
            arrow_schema::DataType::Utf8 => {
                let mut builder = StringBuilder::new();
                for obj in entries {
                    if let Some(val) = obj.get(k) {
                        if val.is_null() {
                            builder.append_null();
                        } else if let Some(s) = val.as_str() {
                            builder.append_value(s);
                        } else {
                            builder.append_value(val.to_string());
                        }
                    } else {
                        builder.append_null();
                    }
                }
                arrays.push(Arc::new(builder.finish()));
            }
            _ => {
                return Err(format!("unsupported data type for column {}", k));
            }
        }
    }

    let schema = Arc::new(Schema::new(fields));
    arrow_array::RecordBatch::try_new(schema, arrays)
        .map_err(|e| format!("failed to build record batch: {}", e))
}

fn write_parquet_file(path: &Path, batch: &arrow_array::RecordBatch) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|e| format!("failed to create file {}: {}", path.display(), e))?;
    let mut writer = parquet::arrow::arrow_writer::ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| format!("failed to create arrow writer: {}", e))?;
    writer
        .write(batch)
        .map_err(|e| format!("failed to write batch: {}", e))?;
    writer
        .close()
        .map_err(|e| format!("failed to close writer: {}", e))?;
    Ok(())
}

fn read_parquet_entries(path: &Path) -> Result<Vec<serde_json::Map<String, Value>>, String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file =
        File::open(path).map_err(|e| format!("failed to open file {}: {}", path.display(), e))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("failed to create reader builder: {}", e))?;
    let reader = builder
        .build()
        .map_err(|e| format!("failed to build reader: {}", e))?;

    let mut entries = Vec::new();
    for batch_res in reader {
        let batch = batch_res.map_err(|e| format!("failed to read record batch: {}", e))?;
        let schema = batch.schema();
        let num_rows = batch.num_rows();
        let num_cols = batch.num_columns();

        for row in 0..num_rows {
            let mut obj = serde_json::Map::new();
            for col in 0..num_cols {
                let col_name = schema.field(col).name();
                let array = batch.column(col);
                let val = arrow_to_json_value(array, row)?;
                obj.insert(col_name.clone(), val);
            }

            if let Some(props_val) = obj.get("props") {
                if let Some(props_obj) = props_val.as_object() {
                    let props_obj = props_obj.clone();
                    obj.remove("props");
                    for (k, v) in props_obj {
                        obj.insert(k, v);
                    }
                }
            }

            entries.push(obj);
        }
    }
    Ok(entries)
}

fn arrow_to_json_value(array: &arrow_array::ArrayRef, row: usize) -> Result<Value, String> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }

    use arrow_array::cast::AsArray;
    use arrow_schema::DataType;

    match array.data_type() {
        DataType::Boolean => {
            let arr = array.as_boolean();
            Ok(Value::Bool(arr.value(row)))
        }
        DataType::Int8 => {
            let arr = array.as_primitive::<arrow_array::types::Int8Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::Int16 => {
            let arr = array.as_primitive::<arrow_array::types::Int16Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::Int32 => {
            let arr = array.as_primitive::<arrow_array::types::Int32Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::Int64 => {
            let arr = array.as_primitive::<arrow_array::types::Int64Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::UInt8 => {
            let arr = array.as_primitive::<arrow_array::types::UInt8Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::UInt16 => {
            let arr = array.as_primitive::<arrow_array::types::UInt16Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::UInt32 => {
            let arr = array.as_primitive::<arrow_array::types::UInt32Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::UInt64 => {
            let arr = array.as_primitive::<arrow_array::types::UInt64Type>();
            Ok(Value::Number(arr.value(row).into()))
        }
        DataType::Float32 => {
            let arr = array.as_primitive::<arrow_array::types::Float32Type>();
            if let Some(num) = serde_json::Number::from_f64(arr.value(row) as f64) {
                Ok(Value::Number(num))
            } else {
                Ok(Value::Null)
            }
        }
        DataType::Float64 => {
            let arr = array.as_primitive::<arrow_array::types::Float64Type>();
            if let Some(num) = serde_json::Number::from_f64(arr.value(row)) {
                Ok(Value::Number(num))
            } else {
                Ok(Value::Null)
            }
        }
        DataType::Utf8 => {
            let arr = array.as_string::<i32>();
            let val_str = arr.value(row);
            if (val_str.starts_with('[') && val_str.ends_with(']'))
                || (val_str.starts_with('{') && val_str.ends_with('}'))
            {
                Ok(serde_json::from_str(val_str)
                    .unwrap_or_else(|_| Value::String(val_str.to_owned())))
            } else {
                Ok(Value::String(val_str.to_owned()))
            }
        }
        DataType::LargeUtf8 => {
            let arr = array.as_string::<i64>();
            let val_str = arr.value(row);
            if (val_str.starts_with('[') && val_str.ends_with(']'))
                || (val_str.starts_with('{') && val_str.ends_with('}'))
            {
                Ok(serde_json::from_str(val_str)
                    .unwrap_or_else(|_| Value::String(val_str.to_owned())))
            } else {
                Ok(Value::String(val_str.to_owned()))
            }
        }
        DataType::List(_) => {
            let list_arr = array.as_list::<i32>();
            let value_arr = list_arr.value(row);
            let mut list_vals = Vec::new();
            for i in 0..value_arr.len() {
                list_vals.push(arrow_to_json_value(&value_arr, i)?);
            }
            Ok(Value::Array(list_vals))
        }
        DataType::LargeList(_) => {
            let list_arr = array.as_list::<i64>();
            let value_arr = list_arr.value(row);
            let mut list_vals = Vec::new();
            for i in 0..value_arr.len() {
                list_vals.push(arrow_to_json_value(&value_arr, i)?);
            }
            Ok(Value::Array(list_vals))
        }
        _ => Ok(Value::Null),
    }
}
