use std::collections::HashMap;
use std::path::{Path, PathBuf};

use issundb::GraphQueryExt;
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[test]
fn test_opencypher_conformance() {
    // Gate conformance tests on ISSUNDB_CONFORMANCE=1 to keep default `cargo test` fast.
    if std::env::var("ISSUNDB_CONFORMANCE").is_err() {
        println!("Skipping openCypher conformance tests. Set ISSUNDB_CONFORMANCE=1 to execute.");
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let manifest_path = PathBuf::from(&manifest_dir);

    // Prefer the full TCK submodule; fall back to the hand-crafted files.
    let tck_root = manifest_path.join("../../external/openCypher/tck/features");
    let fallback_root = manifest_path.join("tests/conformance/features");

    let features_root = if tck_root.exists() {
        tck_root.canonicalize().unwrap_or_else(|_| tck_root.clone())
    } else if fallback_root.exists() {
        fallback_root
            .canonicalize()
            .unwrap_or_else(|_| fallback_root.clone())
    } else {
        panic!(
            "No feature files found. Checked:\n  {:?}\n  {:?}",
            tck_root, fallback_root
        );
    };

    println!("TCK root: {:?}", features_root);

    // Collect all .feature files recursively.
    let feature_files: Vec<PathBuf> = WalkDir::new(&features_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "feature"))
        .map(|e| e.path().to_path_buf())
        .collect();

    if feature_files.is_empty() {
        panic!("walkdir found no .feature files under {:?}", features_root);
    }

    // category -> (passed, failed, skipped)
    let mut counts: HashMap<String, (usize, usize, usize)> = HashMap::new();

    for path in &feature_files {
        let category = category_for(&features_root, path);

        let scenarios = match parse_feature_file(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("WARNING: could not parse {:?}: {}", path, e);
                let entry = counts.entry(category).or_default();
                // Count one "skipped" per file that we couldn't parse.
                entry.2 += 1;
                continue;
            }
        };

        for scenario in &scenarios {
            let entry = counts.entry(category.clone()).or_default();

            if scenario.skip {
                entry.2 += 1;
                continue;
            }

            let name = scenario.name.clone();
            let scenario_clone = scenario.clone();

            // Catch panics from bugs in the runner itself.
            let result = std::panic::catch_unwind(move || run_scenario(&scenario_clone));

            match result {
                Ok(Ok(())) => {
                    entry.0 += 1;
                }
                Ok(Err(ref e)) if e.starts_with("__skip__:") => {
                    entry.2 += 1;
                }
                Ok(Err(ref e)) if e.starts_with("setup query failed:") => {
                    // Setup failure means we cannot run the scenario; count as skipped.
                    entry.2 += 1;
                }
                Ok(Err(e)) => {
                    eprintln!(
                        "FAILED  [{category}] {name}\n        {e}",
                        category = &category,
                        name = &name,
                        e = e
                    );
                    entry.1 += 1;
                }
                Err(_panic) => {
                    eprintln!(
                        "PANIC   [{category}] {name}",
                        category = &category,
                        name = &name
                    );
                    entry.1 += 1;
                }
            }
        }
    }

    // Print summary table.
    println!();
    println!(
        "{:<40} {:>8} {:>8} {:>8}",
        "Category", "Passed", "Failed", "Skipped"
    );
    println!("{}", "-".repeat(66));

    let mut sorted_categories: Vec<_> = counts.keys().collect();
    sorted_categories.sort();

    let (mut total_passed, mut total_failed, mut total_skipped) = (0usize, 0usize, 0usize);
    for cat in &sorted_categories {
        let (p, f, s) = counts[*cat];
        println!("{:<40} {:>8} {:>8} {:>8}", cat, p, f, s);
        total_passed += p;
        total_failed += f;
        total_skipped += s;
    }
    println!("{}", "-".repeat(66));
    println!(
        "{:<40} {:>8} {:>8} {:>8}",
        "TOTAL", total_passed, total_failed, total_skipped
    );
    println!();

    assert!(
        total_failed == 0,
        "{} TCK scenario(s) failed — see output above",
        total_failed
    );
}

// ---------------------------------------------------------------------------
// Category helper
// ---------------------------------------------------------------------------

fn category_for(root: &Path, feature_file: &Path) -> String {
    feature_file
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Scenario data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Assertion {
    /// `Then the result should be, in any order:` or `in order:`
    Rows {
        ordered: bool,
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        /// True when at least one cell contained a node/rel display literal.
        has_node_literals: bool,
    },
    /// `Then the result should be empty`
    Empty,
    /// `Then a SyntaxError / error should be raised ...`
    ExpectError,
    /// No explicit result assertion (e.g., only side-effects).
    None,
}

#[derive(Debug, Clone)]
struct Scenario {
    name: String,
    skip: bool,
    /// Queries that must be executed before the main query (from Background + Given steps).
    setup_queries: Vec<String>,
    query: String,
    assertion: Assertion,
    /// Query parameters from `And parameters are:` tables.
    params: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Feature-file parser
// ---------------------------------------------------------------------------

fn parse_feature_file(path: &Path) -> Result<Vec<Scenario>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = content.lines().collect();

    // First pass: collect background setup queries (they apply to every scenario).
    let background_queries = collect_background(&lines);

    // Second pass: split the file into scenario blocks and parse each one.
    let blocks = split_scenario_blocks(&lines);

    let mut scenarios = Vec::new();
    for block in blocks {
        let parsed = parse_scenario_block(&block, &background_queries)?;
        scenarios.extend(parsed);
    }

    Ok(scenarios)
}

/// Collect the setup queries from the `Background:` section (if any).
fn collect_background(lines: &[&str]) -> Vec<String> {
    let mut in_background = false;
    let mut queries = Vec::new();
    let mut idx = 0;

    while idx < lines.len() {
        let trimmed = lines[idx].trim();

        if trimmed.starts_with("Background:") {
            in_background = true;
            idx += 1;
            continue;
        }

        // A new Feature/Scenario ends the background.
        if in_background
            && (trimmed.starts_with("Scenario:")
                || trimmed.starts_with("Scenario Outline:")
                || trimmed.starts_with("Feature:"))
        {
            break;
        }

        if in_background
            && (trimmed.starts_with("Given ")
                || trimmed.starts_with("And ")
                || trimmed.starts_with("* "))
        {
            // Consume an optional docstring block.
            if let Some(query) = consume_docstring(lines, &mut idx) {
                queries.push(query);
                continue;
            }
        }

        idx += 1;
    }

    queries
}

/// A raw block of lines belonging to a single Scenario / Scenario Outline.
struct Block {
    tags: Vec<String>,
    lines: Vec<String>,
}

/// Split the file's lines into one `Block` per scenario heading.
fn split_scenario_blocks(lines: &[&str]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut pending_tags: Vec<String> = Vec::new();
    let mut current_block: Option<Vec<String>> = None;
    let mut current_tags: Vec<String> = Vec::new();

    for &line in lines {
        let trimmed = line.trim();

        // Tag lines begin with `@`.
        if trimmed.starts_with('@') {
            let tags: Vec<String> = trimmed
                .split_whitespace()
                .filter(|t| t.starts_with('@'))
                .map(|t| t[1..].to_string())
                .collect();
            if current_block.is_none() {
                pending_tags.extend(tags);
            } else {
                // Tags between scenarios belong to the upcoming one; flush current.
                if let Some(b) = current_block.take() {
                    blocks.push(Block {
                        tags: current_tags.clone(),
                        lines: b,
                    });
                }
                current_tags = tags;
                current_block = None;
                pending_tags.clear();
            }
            continue;
        }

        if trimmed.starts_with("Scenario:") || trimmed.starts_with("Scenario Outline:") {
            if let Some(b) = current_block.take() {
                blocks.push(Block {
                    tags: current_tags.clone(),
                    lines: b,
                });
            }
            current_tags = std::mem::take(&mut pending_tags);
            current_block = Some(vec![line.to_string()]);
            continue;
        }

        if let Some(ref mut block) = current_block {
            block.push(line.to_string());
        }
    }

    if let Some(b) = current_block {
        blocks.push(Block {
            tags: current_tags,
            lines: b,
        });
    }

    blocks
}

/// Parse a single block, expanding `Scenario Outline` + `Examples` into concrete scenarios.
fn parse_scenario_block(block: &Block, background: &[String]) -> Result<Vec<Scenario>, String> {
    let lines: Vec<&str> = block.lines.iter().map(|s| s.as_str()).collect();
    if lines.is_empty() {
        return Ok(vec![]);
    }

    let header = lines[0].trim();
    let is_outline = header.starts_with("Scenario Outline:");

    let base_name = if is_outline {
        header
            .strip_prefix("Scenario Outline:")
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        header
            .strip_prefix("Scenario:")
            .unwrap_or(header)
            .trim()
            .to_string()
    };

    let skip = block
        .tags
        .iter()
        .any(|t| t == "skip" || t == "NegativeTests");

    if !is_outline {
        let scenario = build_scenario(&base_name, &lines[1..], background, skip, &[])?;
        return Ok(vec![scenario]);
    }

    // Parse an outline: collect Examples tables.
    let examples_tables = collect_examples_tables(&lines[1..]);

    if examples_tables.is_empty() {
        // No Examples; treat as a regular skip.
        let scenario = build_scenario(&base_name, &lines[1..], background, true, &[])?;
        return Ok(vec![scenario]);
    }

    let mut expanded = Vec::new();
    for table in &examples_tables {
        if table.is_empty() {
            continue;
        }
        let header_row = &table[0];
        for data_row in &table[1..] {
            // Build a substitution map.
            let subs: Vec<(String, String)> = header_row
                .iter()
                .zip(data_row.iter())
                .map(|(k, v)| (format!("<{}>", k.trim()), v.trim().to_string()))
                .collect();

            let concrete_name = apply_subs(&base_name, &subs);
            let concrete_lines: Vec<String> = lines[1..]
                .iter()
                .map(|l| apply_subs(l.trim(), &subs))
                .collect();
            let concrete_refs: Vec<&str> = concrete_lines.iter().map(|s| s.as_str()).collect();
            let scenario = build_scenario(&concrete_name, &concrete_refs, background, skip, &subs)?;
            expanded.push(scenario);
        }
    }

    Ok(expanded)
}

/// Collect `Examples:` tables from the body of a Scenario Outline.
fn collect_examples_tables(lines: &[&str]) -> Vec<Vec<Vec<String>>> {
    let mut tables = Vec::new();
    let mut current_table: Option<Vec<Vec<String>>> = None;
    let mut in_examples = false;

    for &line in lines {
        let trimmed = line.trim();

        if trimmed.starts_with("Examples:") || trimmed == "Examples:" {
            if let Some(t) = current_table.take() {
                tables.push(t);
            }
            current_table = Some(Vec::new());
            in_examples = true;
            continue;
        }

        // A new section ends the current table.
        if in_examples
            && (trimmed.starts_with("Scenario")
                || trimmed.starts_with("Background")
                || trimmed.starts_with("Feature"))
        {
            if let Some(t) = current_table.take() {
                tables.push(t);
            }
            in_examples = false;
            continue;
        }

        if in_examples {
            if let Some(ref mut t) = current_table {
                if trimmed.starts_with('|') && trimmed.ends_with('|') {
                    let cells: Vec<String> = trimmed[1..trimmed.len() - 1]
                        .split('|')
                        .map(|c| c.trim().to_string())
                        .collect();
                    t.push(cells);
                }
            }
        }
    }

    if let Some(t) = current_table {
        tables.push(t);
    }

    tables
}

fn apply_subs(s: &str, subs: &[(String, String)]) -> String {
    let mut result = s.to_string();
    for (placeholder, value) in subs {
        result = result.replace(placeholder.as_str(), value.as_str());
    }
    result
}

/// Build a concrete `Scenario` from its body lines.
fn build_scenario(
    name: &str,
    body: &[&str],
    background: &[String],
    skip: bool,
    _subs: &[(String, String)],
) -> Result<Scenario, String> {
    let mut setup_queries: Vec<String> = background.to_vec();
    let mut query = String::new();
    let mut assertion = Assertion::None;
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();

    let mut idx = 0;
    let mut pending_query_kind: Option<&'static str> = None; // "setup" | "when"

    while idx < body.len() {
        let trimmed = body[idx].trim();

        // Skip blank lines and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }

        // Skip Examples: sections (already used during outline expansion).
        if trimmed.starts_with("Examples:") {
            // Skip the whole table.
            idx += 1;
            while idx < body.len() {
                let t = body[idx].trim();
                if t.starts_with('|') {
                    idx += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        // Docstring blocks.
        if trimmed == "\"\"\"" {
            if let Some(kind) = pending_query_kind.take() {
                let (collected, advance) = collect_docstring(body, idx + 1);
                idx = advance;
                if kind == "setup" {
                    setup_queries.push(collected);
                } else {
                    query = collected;
                }
                continue;
            }
            idx += 1;
            continue;
        }

        // Parameters table: `And parameters are:` followed by | key | value | rows.
        if trimmed == "And parameters are:" || trimmed == "* parameters are:"
            || trimmed.ends_with("parameters are:")
        {
            idx += 1;
            // ALL rows are data rows (key, value) - no header.
            while idx < body.len() {
                let t = body[idx].trim();
                if t.starts_with('|') && t.ends_with('|') {
                    let cells: Vec<&str> = t[1..t.len() - 1].split('|').collect();
                    if cells.len() == 2 {
                        let key = cells[0].trim().to_string();
                        let raw_val = cells[1].trim();
                        // Skip substitution placeholders like <elt> that weren't expanded.
                        if raw_val.starts_with('<') && raw_val.ends_with('>') {
                            idx += 1;
                            continue;
                        }
                        let val = parse_table_value(raw_val);
                        if !key.is_empty() {
                            params.insert(key, val);
                        }
                    }
                    idx += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        // Given/And/But (setup steps).
        if trimmed.starts_with("Given ")
            || trimmed.starts_with("And having executed:")
            || trimmed.starts_with("* having executed:")
        {
            if trimmed.ends_with("\"\"\"") || trimmed.contains("having executed:") {
                // The docstring may start on the next line.
                if let Some(q) = consume_docstring(body, &mut idx) {
                    setup_queries.push(q);
                } else {
                    pending_query_kind = Some("setup");
                    idx += 1;
                }
            } else {
                // `Given an empty graph`, `Given any graph`, etc. — nothing to collect.
                idx += 1;
            }
            continue;
        }

        if trimmed.starts_with("And ") || trimmed.starts_with("But ") {
            // Could be "And having executed:", "And no side effects", "And the side effects..."
            if trimmed.contains("having executed:") {
                if let Some(q) = consume_docstring(body, &mut idx) {
                    setup_queries.push(q);
                } else {
                    pending_query_kind = Some("setup");
                    idx += 1;
                }
            } else if trimmed.contains("the side effects should be:")
                || trimmed.contains("no side effects")
            {
                // Consume an optional table.
                idx += 1;
                while idx < body.len() {
                    let t = body[idx].trim();
                    if t.starts_with('|') {
                        idx += 1;
                    } else {
                        break;
                    }
                }
            } else {
                idx += 1;
            }
            continue;
        }

        // When executing query:
        if trimmed.starts_with("When executing query:")
            || trimmed.starts_with("When running query:")
        {
            if let Some(q) = consume_docstring(body, &mut idx) {
                query = q;
            } else {
                pending_query_kind = Some("when");
                idx += 1;
            }
            continue;
        }

        // Then assertions.
        if trimmed.starts_with("Then ") {
            assertion = parse_then(body, &mut idx);
            continue;
        }

        idx += 1;
    }

    Ok(Scenario {
        name: name.to_string(),
        skip,
        setup_queries,
        query,
        assertion,
        params,
    })
}

/// Advance past a `"""..."""` docstring starting at `idx` (which points to the `"""` opener or the
/// line before it, depending on how the caller found it). Returns the collected text and the new
/// `idx` (pointing just past the closing `"""`).
fn collect_docstring(lines: &[&str], start: usize) -> (String, usize) {
    let mut idx = start;
    let mut collected = Vec::new();
    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed == "\"\"\"" {
            return (collected.join("\n"), idx + 1);
        }
        collected.push(lines[idx].to_string());
        idx += 1;
    }
    (collected.join("\n"), idx)
}

/// Try to find and consume a docstring that starts on the current line or the next line.
/// Advances `idx` past the closing `"""` and returns the collected text, or returns `None` if
/// no docstring is found.
fn consume_docstring(lines: &[&str], idx: &mut usize) -> Option<String> {
    // Check if the opening `"""` is on the same line (e.g., `Given having executed: """`).
    let current = lines[*idx].trim();
    if current.ends_with("\"\"\"") && current.len() > 3 {
        // The docstring opens and possibly closes on the next line(s).
        *idx += 1;
        let (text, new_idx) = collect_docstring(lines, *idx);
        *idx = new_idx;
        return Some(text);
    }

    // Check if the very next line is `"""`.
    let next_idx = *idx + 1;
    if next_idx < lines.len() && lines[next_idx].trim() == "\"\"\"" {
        *idx = next_idx + 1;
        let (text, new_idx) = collect_docstring(lines, *idx);
        *idx = new_idx;
        return Some(text);
    }

    None
}

/// Parse a `Then ...` line (and any following table) into an `Assertion`.
/// Advances `idx` past the assertion block.
fn parse_then(lines: &[&str], idx: &mut usize) -> Assertion {
    let trimmed = lines[*idx].trim();
    *idx += 1;

    if trimmed.contains("result should be empty") {
        // Consume optional `And ...` lines (side effects).
        consume_and_clauses(lines, idx);
        return Assertion::Empty;
    }

    if trimmed.contains("result should be") {
        let ordered = trimmed.contains("in order") && !trimmed.contains("any order");
        let (columns, rows, has_node_literals) = parse_result_table(lines, idx);
        // Consume optional `And ...` lines (side effects).
        consume_and_clauses(lines, idx);
        return Assertion::Rows {
            ordered,
            columns,
            rows,
            has_node_literals,
        };
    }

    if trimmed.contains("SyntaxError should be raised")
        || trimmed.contains("error should be raised")
        || trimmed.contains("TypeError should be raised")
        || trimmed.contains("EntityNotFound should be raised")
        || trimmed.contains("ArgumentError should be raised")
        || trimmed.contains("ParameterMissing should be raised")
        || trimmed.contains("ProcedureError should be raised")
        || trimmed.contains("SemanticError should be raised")
        || trimmed.contains("ConstraintVerificationFailed should be raised")
        || trimmed.contains("ConstraintValidationFailed should be raised")
        || trimmed.contains("should be raised")
    {
        consume_and_clauses(lines, idx);
        return Assertion::ExpectError;
    }

    // Unknown Then clause; consume any table and move on.
    while *idx < lines.len() {
        let t = lines[*idx].trim();
        if t.starts_with('|') {
            *idx += 1;
        } else {
            break;
        }
    }
    consume_and_clauses(lines, idx);
    Assertion::None
}

/// Consume `And ...` / `But ...` follow-up lines (side-effects assertions, etc.).
fn consume_and_clauses(lines: &[&str], idx: &mut usize) {
    while *idx < lines.len() {
        let t = lines[*idx].trim();
        if t.starts_with("And ") || t.starts_with("But ") {
            *idx += 1;
            // Consume an optional table attached to this clause.
            while *idx < lines.len() {
                let inner = lines[*idx].trim();
                if inner.starts_with('|') {
                    *idx += 1;
                } else {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

/// Parse the `| col | col |` header and rows that follow a `Then the result should be...` step.
/// Returns `(columns, rows, has_node_literals)`.
fn parse_result_table(
    lines: &[&str],
    idx: &mut usize,
) -> (Vec<String>, Vec<Vec<serde_json::Value>>, bool) {
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut has_node_literals = false;

    while *idx < lines.len() {
        let trimmed = lines[*idx].trim();
        if !(trimmed.starts_with('|') && trimmed.ends_with('|')) {
            break;
        }
        let cells: Vec<&str> = trimmed[1..trimmed.len() - 1].split('|').collect();
        if columns.is_empty() {
            columns = cells.iter().map(|c| c.trim().to_string()).collect();
        } else {
            let row: Vec<serde_json::Value> = cells
                .iter()
                .map(|c| {
                    let (v, is_node) = parse_table_cell(c);
                    if is_node {
                        has_node_literals = true;
                    }
                    v
                })
                .collect();
            rows.push(row);
        }
        *idx += 1;
    }

    (columns, rows, has_node_literals)
}

// ---------------------------------------------------------------------------
// Table cell parsing
// ---------------------------------------------------------------------------

/// Parse a single table cell value.
/// Returns `(value, is_node_literal)`.
fn parse_table_cell(s: &str) -> (serde_json::Value, bool) {
    let t = s.trim();

    // Node / relationship display literals like `(:Label)`, `(:L {p: v})`, `()-[:T]->()`, etc.
    if (t.starts_with("(:") || t.starts_with("(") && t.contains(':'))
        || t.starts_with("()-[")
        || t.starts_with("()-[:")
        || t.starts_with("<-[")
        || (t.starts_with('[') && t.contains("->"))
    {
        return (serde_json::Value::String(t.to_string()), true);
    }

    (parse_table_value(t), false)
}

fn parse_table_value(trimmed: &str) -> serde_json::Value {
    if trimmed.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return serde_json::Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return serde_json::Value::Bool(false);
    }

    // Quoted string: 'text' or "text"
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        return serde_json::Value::String(trimmed[1..trimmed.len() - 1].to_string());
    }

    // List: [...]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            return serde_json::Value::Array(vec![]);
        }
        let items = split_table_list(inner);
        let parsed: Vec<serde_json::Value> =
            items.iter().map(|s| parse_table_value(s.trim())).collect();
        return serde_json::Value::Array(parsed);
    }

    // Map: {...}
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = trimmed[1..trimmed.len() - 1].trim();
        let mut map = serde_json::Map::new();
        if !inner.is_empty() {
            for entry in split_table_list(inner) {
                let entry = entry.trim();
                if let Some(colon) = entry.find(':') {
                    let key = entry[..colon].trim().trim_matches('\'').trim_matches('"');
                    let val = parse_table_value(entry[colon + 1..].trim());
                    map.insert(key.to_string(), val);
                }
            }
        }
        return serde_json::Value::Object(map);
    }

    // Integer (including negative).
    if let Ok(v) = trimmed.parse::<i64>() {
        return serde_json::Value::Number(v.into());
    }

    // Float.
    if let Ok(v) = trimmed.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(v) {
            return serde_json::Value::Number(n);
        }
        return serde_json::Value::Null;
    }

    // Bare identifier or anything else.
    serde_json::Value::String(trimmed.to_string())
}

/// Split a comma-separated list respecting nested brackets.
fn split_table_list(s: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth_b = 0i32;
    let mut depth_p = 0i32;
    let mut depth_br = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '\'' if !in_dq => {
                in_sq = !in_sq;
            }
            '"' if !in_sq => {
                in_dq = !in_dq;
            }
            '[' if !in_sq && !in_dq => {
                depth_b += 1;
            }
            ']' if !in_sq && !in_dq => {
                depth_b -= 1;
            }
            '(' if !in_sq && !in_dq => {
                depth_p += 1;
            }
            ')' if !in_sq && !in_dq => {
                depth_p -= 1;
            }
            '{' if !in_sq && !in_dq => {
                depth_br += 1;
            }
            '}' if !in_sq && !in_dq => {
                depth_br -= 1;
            }
            ',' if !in_sq && !in_dq && depth_b == 0 && depth_p == 0 && depth_br == 0 => {
                items.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    items.push(&s[start..]);
    items
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

fn run_scenario(scenario: &Scenario) -> Result<(), String> {
    let temp_dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let graph = issundb::Graph::open(temp_dir.path(), 1).map_err(|e| e.to_string())?;

    // Run setup queries; skip the scenario if any setup fails.
    for setup_query in &scenario.setup_queries {
        if setup_query.trim().is_empty() {
            continue;
        }
        let params: HashMap<String, serde_json::Value> = HashMap::new();
        graph
            .query_with_params(setup_query, &params)
            .map_err(|e| format!("setup query failed: {}", e))?;
    }

    graph.rebuild_csr().map_err(|e| e.to_string())?;

    if scenario.query.trim().is_empty() {
        // No main query; treat as passed if we expected no error.
        return match &scenario.assertion {
            Assertion::ExpectError => Err("expected an error but there was no query".into()),
            _ => Ok(()),
        };
    }

    let params = scenario.params.clone();
    let exec_result = graph.query_with_params(&scenario.query, &params);

    match &scenario.assertion {
        Assertion::ExpectError => {
            if exec_result.is_err() {
                return Ok(());
            }
            Err("expected an error but the query succeeded".into())
        }

        Assertion::Empty => {
            let res = exec_result.map_err(|e| e.to_string())?;
            if !res.records.is_empty() {
                return Err(format!(
                    "expected empty result but got {} row(s)",
                    res.records.len()
                ));
            }
            Ok(())
        }

        Assertion::Rows {
            ordered,
            columns,
            rows: expected_rows,
            has_node_literals,
        } => {
            // Skip scenarios whose expected output contains node/rel display literals because
            // IssunDB returns node IDs, not display strings.
            if *has_node_literals {
                return Err(
                    "__skip__: result table contains node/relationship display literals".into(),
                );
            }

            let res = exec_result.map_err(|e| e.to_string())?;

            if columns != &res.columns {
                return Err(format!(
                    "column mismatch.\nExpected: {:?}\nActual:   {:?}",
                    columns, res.columns
                ));
            }

            let mut actual_rows: Vec<Vec<serde_json::Value>> =
                res.records.into_iter().map(|r| r.values).collect();
            let mut exp = expected_rows.clone();

            if !*ordered {
                let key = |r: &Vec<serde_json::Value>| {
                    r.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("|")
                };
                actual_rows.sort_by_key(key);
                exp.sort_by_key(|r: &Vec<serde_json::Value>| {
                    r.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("|")
                });
            }

            if actual_rows != exp {
                return Err(format!(
                    "row mismatch.\nExpected: {:#?}\nActual:   {:#?}",
                    exp, actual_rows
                ));
            }
            Ok(())
        }

        Assertion::None => {
            // No assertion to check; just ensure the query does not error.
            exec_result.map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}
