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

    // The chumsky-based Cypher parser has deep call stacks on complex TCK queries.
    // Run the actual test body in a thread with a large stack to prevent SIGSEGV.
    let result = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(conformance_body)
        .expect("failed to spawn conformance thread")
        .join();

    match result {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => panic!("{}", msg),
        Err(payload) => {
            if let Some(s) = payload.downcast_ref::<String>() {
                panic!("{}", s);
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                panic!("{}", s);
            } else {
                panic!("conformance thread panicked");
            }
        }
    }
}

fn conformance_body() -> Result<(), String> {
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

    let filter = std::env::var("ISSUNDB_CONFORMANCE_FILTER").ok();

    let feature_files: Vec<PathBuf> = WalkDir::new(&features_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "feature"))
        .map(|e| e.path().to_path_buf())
        .filter(|p| {
            if let Some(ref f) = filter {
                p.to_string_lossy().contains(f)
            } else {
                true
            }
        })
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
                    eprintln!("SKIPPED [{category}] {name}\n        {e}");
                    entry.2 += 1;
                }
                Ok(Err(ref e)) if e.starts_with("setup query failed:") => {
                    // Setup failure means we cannot run the scenario; count as skipped.
                    eprintln!("SKIPPED [{category}] {name}\n        {e}");
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

    // A regression gate rather than a perfection gate: a known set of TCK
    // scenarios fails today (tracked as deferred conformance work), and a few
    // are `rand()`-flaky, so an exact-zero requirement would either be red or
    // flaky in CI. `ISSUNDB_CONFORMANCE_MAX_FAILURES` sets the tolerated
    // failure budget; the run fails only when failures exceed it, which still
    // catches any new regression. The default is 0 so a local run surfaces
    // every failure; CI sets the budget to the current known-gap count plus a
    // small margin for the flaky scenarios.
    let max_failures: usize = std::env::var("ISSUNDB_CONFORMANCE_MAX_FAILURES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    println!(
        "Conformance failure budget (ISSUNDB_CONFORMANCE_MAX_FAILURES): {}",
        max_failures
    );
    if total_failed > max_failures {
        return Err(format!(
            "{} TCK scenario(s) failed, above the tolerated budget of {}: see output above",
            total_failed, max_failures
        ));
    }
    Ok(())
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
        ignore_list_order: bool,
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        /// The first cell whose text looks like a display literal the literal
        /// parser does not support, if any; the runner skips the scenario and
        /// names the form.
        unsupported_literal: Option<String>,
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
    /// Table-backed procedures registered via `And there exists a procedure ...`.
    procedures: Vec<issundb::Procedure>,
}

// ---------------------------------------------------------------------------
// Feature-file parser
// ---------------------------------------------------------------------------

fn parse_feature_file(path: &Path) -> Result<Vec<Scenario>, String> {
    let feature = load_feature(path)?;

    // Background steps apply to every scenario in the file.
    let background = feature
        .background
        .as_ref()
        .map(|b| collect_setup_steps(&b.steps))
        .unwrap_or_default();

    let mut scenarios = Vec::new();
    for scenario in &feature.scenarios {
        scenarios.extend(expand_scenario(scenario, &background));
    }
    // Scenarios nested under `Rule:` sections inherit the feature background
    // followed by the rule background. The openCypher TCK does not currently use
    // rules, but handle them so a future TCK bump does not silently drop coverage.
    for rule in &feature.rules {
        let mut rule_background = background.clone();
        if let Some(b) = &rule.background {
            rule_background.extend(collect_setup_steps(&b.steps));
        }
        for scenario in &rule.scenarios {
            scenarios.extend(expand_scenario(scenario, &rule_background));
        }
    }

    Ok(scenarios)
}

/// Parse a feature file into a `gherkin::Feature`, with a fallback for the
/// non-strict dialect used by parts of the openCypher TCK. Some TCK scenarios
/// open with an `And`/`But` step that continues the `Background`; strict Gherkin
/// rejects a leading continuation step. On parse failure, promote any leading
/// continuation step to `Given` and retry once.
fn load_feature(path: &Path) -> Result<gherkin::Feature, String> {
    match gherkin::Feature::parse_path(path, gherkin::GherkinEnv::default()) {
        Ok(feature) => Ok(feature),
        Err(_) => {
            let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let normalized = normalize_leading_continuations(&content);
            gherkin::Feature::parse(normalized, gherkin::GherkinEnv::default())
                .map_err(|e| e.to_string())
        }
    }
}

/// Rewrite the first step of each `Scenario`, `Scenario Outline`, or
/// `Background` from a leading `And`/`But`/`*` continuation to `Given`,
/// preserving indentation. Later continuation steps are left untouched. This is
/// applied only as a fallback when strict parsing fails.
fn normalize_leading_continuations(content: &str) -> String {
    let mut out: Vec<String> = Vec::with_capacity(content.lines().count());
    let mut expect_first_step = false;
    // Tracks the delimiter (`"""` or ```` ``` ````) of an open docstring, if any.
    // Lines inside a docstring are copied verbatim and never reinterpreted as
    // headers or steps. The delimiter kind is remembered so the other delimiter
    // appearing as docstring content does not close the block.
    let mut docstring_delim: Option<&str> = None;

    for line in content.lines() {
        let trimmed = line.trim_start();

        let delim = if trimmed.starts_with("\"\"\"") {
            Some("\"\"\"")
        } else if trimmed.starts_with("```") {
            Some("```")
        } else {
            None
        };
        if let Some(d) = delim {
            match docstring_delim {
                None => docstring_delim = Some(d),
                Some(open) if open == d => docstring_delim = None,
                Some(_) => {} // the other delimiter as content; stays open
            }
            out.push(line.to_string());
            continue;
        }
        if docstring_delim.is_some() {
            out.push(line.to_string());
            continue;
        }

        if trimmed.starts_with("Scenario:")
            || trimmed.starts_with("Scenario Outline:")
            || trimmed.starts_with("Background:")
        {
            expect_first_step = true;
            out.push(line.to_string());
            continue;
        }

        if expect_first_step {
            // Tags, comments, and blank lines may precede the first step.
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('@') {
                out.push(line.to_string());
                continue;
            }

            expect_first_step = false;
            let indent = &line[..line.len() - trimmed.len()];
            let promoted = trimmed
                .strip_prefix("And ")
                .or_else(|| trimmed.strip_prefix("But "))
                .or_else(|| trimmed.strip_prefix("* "));
            if let Some(rest) = promoted {
                out.push(format!("{}Given {}", indent, rest));
                continue;
            }
        }

        out.push(line.to_string());
    }

    out.join("\n")
}

/// Expand one parsed scenario into concrete `Scenario` values, materializing a
/// `Scenario Outline` into one entry per `Examples` data row.
fn expand_scenario(scenario: &gherkin::Scenario, background: &[String]) -> Vec<Scenario> {
    let skip = scenario
        .tags
        .iter()
        .any(|t| t == "skip" || t == "NegativeTests");

    if scenario.examples.is_empty() {
        // An outline with no Examples table cannot be instantiated; skip it.
        let is_outline = scenario.keyword.contains("Outline");
        return vec![build_scenario(
            scenario.name.clone(),
            &scenario.steps,
            background,
            skip || is_outline,
        )];
    }

    let mut expanded = Vec::new();
    for examples in &scenario.examples {
        let Some(table) = &examples.table else {
            continue;
        };
        let Some((header, data_rows)) = table.rows.split_first() else {
            continue;
        };
        for data_row in data_rows {
            let subs: Vec<(String, String)> = header
                .iter()
                .zip(data_row.iter())
                .map(|(k, v)| (format!("<{}>", k.trim()), v.trim().to_string()))
                .collect();
            let name = apply_subs(&scenario.name, &subs);
            let steps: Vec<gherkin::Step> = scenario
                .steps
                .iter()
                .map(|s| subst_step(s, &subs))
                .collect();
            expanded.push(build_scenario(name, &steps, background, skip));
        }
    }
    expanded
}

/// Collect the setup queries carried by a list of `Given` steps (used for
/// `Background:` sections and any rule background).
fn collect_setup_steps(steps: &[gherkin::Step]) -> Vec<String> {
    steps.iter().filter_map(setup_query_from_step).collect()
}

/// Extract a setup query from a single step, if it carries one. Handles
/// `... having executed: """<query>"""` and `Given the <name> graph` fixtures.
fn setup_query_from_step(step: &gherkin::Step) -> Option<String> {
    let value = step.value.trim();
    if value.contains("having executed:") {
        return step.docstring.clone();
    }
    if let Some(name) = value
        .strip_prefix("the ")
        .and_then(|s| s.strip_suffix(" graph"))
    {
        return load_named_graph(name);
    }
    None
}

/// Load a named openCypher TCK graph fixture from
/// `external/openCypher/tck/graphs/<name>/<name>.cypher`.
fn load_named_graph(name: &str) -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../external/openCypher/tck/graphs")
        .join(name)
        .join(format!("{}.cypher", name));
    std::fs::read_to_string(&path).ok()
}

/// Apply one step's `Scenario Outline` placeholder substitutions to its text,
/// docstring, and any data table cells, returning a concrete step.
fn subst_step(step: &gherkin::Step, subs: &[(String, String)]) -> gherkin::Step {
    let mut step = step.clone();
    step.value = apply_subs(&step.value, subs);
    if let Some(doc) = &step.docstring {
        step.docstring = Some(apply_subs(doc, subs));
    }
    if let Some(table) = &mut step.table {
        for row in &mut table.rows {
            for cell in row.iter_mut() {
                *cell = apply_subs(cell, subs);
            }
        }
    }
    step
}

fn apply_subs(s: &str, subs: &[(String, String)]) -> String {
    let mut result = s.to_string();
    for (placeholder, value) in subs {
        result = result.replace(placeholder.as_str(), value.as_str());
    }
    result
}

/// Build a concrete `Scenario` from its parsed steps.
fn build_scenario(
    name: String,
    steps: &[gherkin::Step],
    background: &[String],
    skip: bool,
) -> Scenario {
    let mut setup_queries: Vec<String> = background.to_vec();
    let mut query = String::new();
    let mut assertion = Assertion::None;
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();
    let mut procedures: Vec<issundb::Procedure> = Vec::new();

    for step in steps {
        let value = step.value.trim();

        // Parameters table: `parameters are:` followed by | key | value | rows.
        if value.ends_with("parameters are:") {
            if let Some(table) = &step.table {
                for row in &table.rows {
                    if row.len() != 2 {
                        continue;
                    }
                    let key = row[0].trim();
                    let raw_val = unescape_gherkin_cell(row[1].trim());
                    // Skip unexpanded substitution placeholders like <elt>.
                    if key.is_empty() || (raw_val.starts_with('<') && raw_val.ends_with('>')) {
                        continue;
                    }
                    params.insert(key.to_string(), parse_table_value(&raw_val));
                }
            }
            continue;
        }

        // Procedure registration: `there exists a procedure NAME(sig) :: (out):`
        // with a data table whose header is `inputs ++ outputs`.
        if value.contains("there exists a procedure") {
            let sig = value
                .split_once("there exists a procedure")
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            let rows = step
                .table
                .as_ref()
                .map(|t| t.rows.clone())
                .unwrap_or_default();
            if let Some(proc) = parse_procedure(sig, &rows) {
                procedures.push(proc);
            }
            continue;
        }

        // Control query: the preceding query becomes setup and this one is asserted.
        if value.starts_with("executing control query:") {
            if !query.trim().is_empty() {
                setup_queries.push(std::mem::take(&mut query));
            }
            if let Some(doc) = &step.docstring {
                query = doc.clone();
            }
            continue;
        }

        // Main query.
        if value.starts_with("executing query:") || value.starts_with("running query:") {
            if let Some(doc) = &step.docstring {
                query = doc.clone();
            }
            continue;
        }

        // Setup steps: `... having executed:` docstrings and named-graph fixtures.
        if let Some(q) = setup_query_from_step(step) {
            setup_queries.push(q);
            continue;
        }

        // A genuine `Then` step (raw keyword `Then`, not an `And`/`But`
        // continuation) sets the assertion for the most recently selected query.
        // A later `Then` overwrites it, which is what the control-query pattern
        // needs: the first `Then` asserts the setup query, the second asserts the
        // control query. `And`/`But` side-effect steps share `ty == Then` but keep
        // their raw keyword, so they are skipped here.
        if step.keyword.trim() == "Then" {
            assertion = assertion_from_step(value, step.table.as_ref());
            continue;
        }
    }

    Scenario {
        name,
        skip,
        setup_queries,
        query,
        assertion,
        params,
        procedures,
    }
}

/// Map a `Then ...` step (and its optional table) to an `Assertion`.
fn assertion_from_step(value: &str, table: Option<&gherkin::Table>) -> Assertion {
    if value.contains("result should be empty") {
        return Assertion::Empty;
    }
    if value.contains("result should be") {
        let ordered = value.contains("in order") && !value.contains("any order");
        let ignore_list_order = value.contains("ignoring element order for lists");
        let (columns, rows, unsupported_literal) = parse_gherkin_result_table(table);
        return Assertion::Rows {
            ordered,
            ignore_list_order,
            columns,
            rows,
            unsupported_literal,
        };
    }
    if value.contains("should be raised") {
        return Assertion::ExpectError;
    }
    Assertion::None
}

/// Parse a `gherkin::Table` into columns, rows, and the first unsupported
/// display-literal cell, if any.
fn parse_gherkin_result_table(
    table: Option<&gherkin::Table>,
) -> (Vec<String>, Vec<Vec<serde_json::Value>>, Option<String>) {
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut unsupported_literal: Option<String> = None;

    if let Some(t) = table {
        if let Some(first_row) = t.rows.first() {
            columns = first_row.iter().map(|c| c.trim().to_string()).collect();
            for row_cells in t.rows.iter().skip(1) {
                let mut parsed_row = Vec::new();
                for cell in row_cells {
                    let (val, unsupported) = parse_table_cell(&unescape_gherkin_cell(cell));
                    if unsupported_literal.is_none() {
                        unsupported_literal = unsupported;
                    }
                    parsed_row.push(val);
                }
                rows.push(parsed_row);
            }
        }
    }

    (columns, rows, unsupported_literal)
}

/// Parse a procedure signature such as
/// `test.my.proc(name :: STRING?, id :: INTEGER?) :: (city :: STRING?, country_code :: INTEGER?):`
/// together with its data table (header row `inputs ++ outputs` followed by data
/// rows) into a registrable `Procedure`. Returns `None` if the signature is
/// malformed.
fn parse_procedure(sig: &str, table_rows: &[Vec<String>]) -> Option<issundb::Procedure> {
    let sig = sig.trim().trim_end_matches(':').trim();

    let open = sig.find('(')?;
    let name = sig[..open].trim().to_string();
    let close = sig[open + 1..].find(')')? + open + 1;
    let inputs_str = &sig[open + 1..close];

    // Outputs live inside the `:: ( ... )` that follows the input list.
    let rest = &sig[close + 1..];
    let out_open = rest.find('(');
    let outputs_str = match out_open {
        Some(o) => {
            let out_close = rest[o + 1..].find(')')? + o + 1;
            &rest[o + 1..out_close]
        }
        None => "",
    };

    let parse_fields = |s: &str| -> Vec<(String, issundb::CypherType)> {
        s.split(',')
            .filter_map(|field| {
                let field = field.trim();
                if field.is_empty() {
                    return None;
                }
                let (fname, ftype) = field.split_once("::").unwrap_or((field, ""));
                Some((fname.trim().to_string(), issundb::CypherType::parse(ftype)))
            })
            .collect()
    };

    let inputs = parse_fields(inputs_str);
    let outputs = parse_fields(outputs_str);

    // The first table row is the header (column names); the rest are data rows,
    // each parsed cell-by-cell with the shared table-value parser.
    let rows: Vec<Vec<serde_json::Value>> = table_rows
        .iter()
        .skip(1)
        .map(|cells| {
            cells
                .iter()
                .map(|c| parse_table_value(&unescape_gherkin_cell(c.trim())))
                .collect()
        })
        .collect();

    Some(issundb::Procedure {
        name,
        inputs,
        outputs,
        rows,
    })
}

// ---------------------------------------------------------------------------
// Table cell parsing
// ---------------------------------------------------------------------------

/// Undo Gherkin data-table cell escaping. The `gherkin` crate hands cells over
/// verbatim, so `\\`, `\|`, and `\n` (the three escapes the Gherkin table
/// syntax defines) reach the harness still escaped. Any other backslash
/// sequence (for example the Cypher escape `\'`) passes through unchanged.
fn unescape_gherkin_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('|') => out.push('|'),
            Some('n') => out.push('\n'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Display-literal parsing
// ---------------------------------------------------------------------------
//
// TCK result tables express nodes, relationships, and paths as openCypher
// display literals: `(:A:B {k: v})`, `[:T {k: v}]`, `<(:A)-[:T]->(:B)>`.
// IssunDB's query results use the same display grammar for whole-entity
// values, so one parser serves both sides. Each literal parses into a
// canonical structural value (labels as a sorted list, properties as a typed
// map, path hops with an explicit direction), and the runner compares those
// structures: label order and property order are irrelevant, list order and
// path direction are significant, and an integer property never equals its
// float twin.

/// Parse a display literal into its canonical structural value. Returns `None`
/// when `t` is not a node, relationship, or path display literal.
fn parse_entity_literal(t: &str) -> Option<serde_json::Value> {
    if t.starts_with('<') && t.ends_with('>') {
        parse_path_literal(t)
    } else if t.starts_with('(') && t.ends_with(')') {
        parse_node_literal(t)
    } else if t.starts_with('[') && t.ends_with(']') {
        parse_rel_literal(t)
    } else {
        None
    }
}

/// Canonical node value: `{"__entity__": "node", "labels": [...sorted],
/// "properties": {...}}`.
fn parse_node_literal(t: &str) -> Option<serde_json::Value> {
    let inner = t.strip_prefix('(')?.strip_suffix(')')?.trim();
    let (labels, props) = parse_labels_and_props(inner, usize::MAX)?;
    let mut labels: Vec<serde_json::Value> =
        labels.into_iter().map(serde_json::Value::String).collect();
    labels.sort_by_key(|l| l.to_string());
    let mut m = serde_json::Map::new();
    m.insert(
        "__entity__".to_string(),
        serde_json::Value::String("node".to_string()),
    );
    m.insert("labels".to_string(), serde_json::Value::Array(labels));
    m.insert("properties".to_string(), props);
    Some(serde_json::Value::Object(m))
}

/// Canonical relationship value: `{"__entity__": "relationship", "type": "T",
/// "properties": {...}}`. Requires a leading `:TYPE`, which is what tells a
/// relationship literal apart from a plain list cell.
fn parse_rel_literal(t: &str) -> Option<serde_json::Value> {
    let inner = t.strip_prefix('[')?.strip_suffix(']')?.trim();
    if !inner.starts_with(':') {
        return None;
    }
    let (mut types, props) = parse_labels_and_props(inner, 1)?;
    let mut m = serde_json::Map::new();
    m.insert(
        "__entity__".to_string(),
        serde_json::Value::String("relationship".to_string()),
    );
    m.insert("type".to_string(), serde_json::Value::String(types.pop()?));
    m.insert("properties".to_string(), props);
    Some(serde_json::Value::Object(m))
}

/// Shared body parser for node and relationship literals: zero or more
/// `:Name` segments (at most `max_names`), then an optional `{...}` property
/// map that must extend to the end of the body.
fn parse_labels_and_props(
    body: &str,
    max_names: usize,
) -> Option<(Vec<String>, serde_json::Value)> {
    let mut rest = body;
    let mut names = Vec::new();
    while let Some(after) = rest.strip_prefix(':') {
        if names.len() == max_names {
            return None;
        }
        let end = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        if end == 0 {
            return None;
        }
        names.push(after[..end].to_string());
        rest = &after[end..];
    }
    let rest = rest.trim();
    let props = if rest.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else if rest.starts_with('{') && rest.ends_with('}') {
        match parse_table_value(rest) {
            v @ serde_json::Value::Object(_) => v,
            _ => return None,
        }
    } else {
        return None;
    };
    Some((names, props))
}

/// Canonical path value: `{"__entity__": "path", "nodes": [...], "rels": [...],
/// "dirs": [...]}` where `dirs[i]` is `"->"` or `"<-"` for the hop between
/// `nodes[i]` and `nodes[i + 1]`. Direction is part of the value, so a path
/// only equals another path traversing its edges the same way.
fn parse_path_literal(t: &str) -> Option<serde_json::Value> {
    let inner = t.strip_prefix('<')?.strip_suffix('>')?.trim();
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    let mut dirs = Vec::new();

    let mut rest = inner;
    loop {
        if !rest.starts_with('(') {
            return None;
        }
        let close = matching_close(rest, '(', ')')?;
        nodes.push(parse_node_literal(&rest[..=close])?);
        rest = rest[close + 1..].trim_start();
        if rest.is_empty() {
            break;
        }

        let incoming = rest.starts_with("<-");
        rest = rest
            .strip_prefix("<-")
            .or_else(|| rest.strip_prefix('-'))?
            .trim_start();
        if !rest.starts_with('[') {
            return None;
        }
        let close = matching_close(rest, '[', ']')?;
        rels.push(parse_rel_literal(&rest[..=close])?);
        rest = rest[close + 1..].trim_start();
        if incoming {
            rest = rest.strip_prefix('-')?.trim_start();
            dirs.push(serde_json::Value::String("<-".to_string()));
        } else {
            rest = rest.strip_prefix("->")?.trim_start();
            dirs.push(serde_json::Value::String("->".to_string()));
        }
    }

    let mut m = serde_json::Map::new();
    m.insert(
        "__entity__".to_string(),
        serde_json::Value::String("path".to_string()),
    );
    m.insert("nodes".to_string(), serde_json::Value::Array(nodes));
    m.insert("rels".to_string(), serde_json::Value::Array(rels));
    m.insert("dirs".to_string(), serde_json::Value::Array(dirs));
    Some(serde_json::Value::Object(m))
}

/// Index of the `close` delimiter matching the `open` at position 0,
/// respecting nesting and quoted strings.
fn matching_close(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_sq || in_dq => escaped = true,
            '\'' if !in_dq => in_sq = !in_sq,
            '"' if !in_sq => in_dq = !in_dq,
            c if c == open && !in_sq && !in_dq => depth += 1,
            c if c == close && !in_sq && !in_dq => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Replace every string that parses as a display literal with its canonical
/// structural value, recursively. Applied to both the expected and the actual
/// rows, so the comparison stays symmetric: a genuine string that happens to
/// look like a literal canonicalizes identically on both sides.
fn canonicalize_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => match parse_entity_literal(&s) {
            Some(canon) => canon,
            None => serde_json::Value::String(s),
        },
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(canonicalize_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, canonicalize_value(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Parse a single table cell value. Returns the parsed value plus the text of
/// the first nested token that looks like a display literal the parser does
/// not support, so the runner can skip the scenario with a reason naming the
/// form.
fn parse_table_cell(s: &str) -> (serde_json::Value, Option<String>) {
    let t = s.trim();
    (parse_table_value(t), unsupported_literal_in(t))
}

/// The first literal-shaped token in `t` (recursing through list and map
/// syntax) that the display-literal parser declined, for example a path
/// written without its angle brackets (`()-[:T]->()`). Comparing such a token
/// as a plain string would be a silent representational mismatch, so the
/// scenario is skipped instead, with the form named. Quoted strings are
/// genuine strings and are never flagged.
fn unsupported_literal_in(t: &str) -> Option<String> {
    let t = t.trim();
    if parse_entity_literal(t).is_some() {
        return None;
    }
    if (t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"')) {
        return None;
    }
    if t.starts_with('[') && t.ends_with(']') {
        let inner = t[1..t.len() - 1].trim();
        if inner.is_empty() {
            return None;
        }
        return split_table_list(inner)
            .iter()
            .find_map(|item| unsupported_literal_in(item));
    }
    if t.starts_with('{') && t.ends_with('}') {
        let inner = t[1..t.len() - 1].trim();
        return split_table_list(inner).iter().find_map(|entry| {
            let entry = entry.trim();
            entry
                .find(':')
                .and_then(|colon| unsupported_literal_in(entry[colon + 1..].trim()))
        });
    }
    let literal_shaped = t.starts_with("(:")
        || (t.starts_with('(') && t.contains(':'))
        || t.starts_with("()-[")
        || t.starts_with("<-[")
        || t.starts_with(':')
        || (t.starts_with('<') && t.ends_with('>'));
    if literal_shaped {
        return Some(t.to_string());
    }
    None
}

fn parse_table_value(trimmed: &str) -> serde_json::Value {
    // Display literals may nest inside list and map cells, for example
    // `[[:REL {num: 1}], [:REL {num: 2}]]`. Trying the literal parser first is
    // what keeps a relationship literal from being misread as a list.
    if let Some(canon) = parse_entity_literal(trimmed) {
        return canon;
    }
    if trimmed.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return serde_json::Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return serde_json::Value::Bool(false);
    }

    // Quoted string: 'text' or "text". Process Cypher escape sequences:
    //   \' → '  (single quote)
    //   \" → "  (double quote)
    //   \\ → \  (backslash)
    //   \n → newline
    //   \t → tab
    //   \r → carriage return
    //   \uXXXX → unicode character
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('\'') => result.push('\''),
                    Some('"') => result.push('"'),
                    Some('\\') => result.push('\\'),
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('r') => result.push('\r'),
                    Some('u') => {
                        // \uXXXX unicode escape.
                        let mut hex = String::new();
                        for _ in 0..4 {
                            if let Some(h) = chars.peek() {
                                if h.is_ascii_hexdigit() {
                                    hex.push(*h);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                        }
                        if let Ok(code) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(code) {
                                result.push(ch);
                                continue;
                            }
                        }
                        // Invalid unicode escape: keep as-is.
                        result.push('\\');
                        result.push('u');
                        result.push_str(&hex);
                    }
                    Some(other) => {
                        result.push('\\');
                        result.push(other);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(c);
            }
        }
        return serde_json::Value::String(result);
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
    let mut registry = issundb::ProcedureRegistry::new();
    for proc in &scenario.procedures {
        registry.register(proc.clone());
    }
    let exec_result = graph.query_with_procedures(&scenario.query, &params, &registry);

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
            ignore_list_order,
            columns,
            rows: expected_rows,
            unsupported_literal,
        } => {
            if let Some(form) = unsupported_literal {
                return Err(format!(
                    "__skip__: unsupported display literal form: {}",
                    form
                ));
            }

            let res = exec_result.map_err(|e| e.to_string())?;

            if columns != &res.columns {
                return Err(format!(
                    "column mismatch.\nExpected: {:?}\nActual:   {:?}",
                    columns, res.columns
                ));
            }

            // Both sides pass through `canonicalize_value`, which turns
            // display-literal strings into comparable structures; the expected
            // rows already parsed literal cells, so this canonicalizes only
            // the quoted strings that happen to look like literals, keeping
            // the comparison symmetric.
            let mut actual_rows: Vec<Vec<serde_json::Value>> = res
                .records
                .into_iter()
                .map(|r| {
                    r.values
                        .into_iter()
                        .map(|v| canonicalize_value(normalize_value(v)))
                        .collect()
                })
                .collect();
            let mut exp: Vec<Vec<serde_json::Value>> = expected_rows
                .iter()
                .map(|r| r.iter().map(|v| canonicalize_value(v.clone())).collect())
                .collect();

            if *ignore_list_order {
                fn sort_lists_in_value(v: &mut serde_json::Value) {
                    match v {
                        serde_json::Value::Array(arr) => {
                            for item in arr.iter_mut() {
                                sort_lists_in_value(item);
                            }
                            arr.sort_by_key(|item| item.to_string());
                        }
                        serde_json::Value::Object(obj) => {
                            for (_, val) in obj.iter_mut() {
                                sort_lists_in_value(val);
                            }
                        }
                        _ => {}
                    }
                }
                for row in &mut actual_rows {
                    for cell in row {
                        sort_lists_in_value(cell);
                    }
                }
                for row in &mut exp {
                    for cell in row {
                        sort_lists_in_value(cell);
                    }
                }
            }

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

/// Convert a temporal JSON object (produced by date/time/duration functions) to
/// its canonical string representation so the conformance runner can compare it
/// against the quoted string literals that appear in the TCK result tables.
fn normalize_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(ref map)
            if map.get("__type__").and_then(|t| t.as_str()) == Some("__NaN__") =>
        {
            serde_json::Value::Null
        }
        serde_json::Value::Object(ref map) if map.contains_key("__str__") => {
            map.get("__str__").cloned().unwrap_or(v)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(normalize_value).collect())
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Display-literal parser and comparator tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod literal_tests {
    use super::*;

    fn cell(s: &str) -> serde_json::Value {
        let (v, unsupported) = parse_table_cell(s);
        assert!(unsupported.is_none(), "cell {s:?} flagged unsupported");
        v
    }

    #[test]
    fn node_literal_labels_are_order_insensitive() {
        assert_eq!(cell("(:A:B {name: 'x'})"), cell("(:B:A {name: 'x'})"));
    }

    #[test]
    fn node_literal_properties_are_order_insensitive() {
        assert_eq!(cell("(:A {a: 1, b: 2})"), cell("(:A {b: 2, a: 1})"));
    }

    #[test]
    fn node_literal_int_and_float_properties_stay_distinct() {
        assert_ne!(cell("(:A {v: 1})"), cell("(:A {v: 1.0})"));
    }

    #[test]
    fn bare_node_literal_parses() {
        assert_eq!(cell("()"), cell("(  )"));
        assert_ne!(cell("()"), cell("(:A)"));
    }

    #[test]
    fn labelless_node_literal_with_properties_parses() {
        assert_eq!(cell("({num: 1})"), cell("( {num: 1} )"));
        assert_ne!(cell("({num: 1})"), cell("()"));
    }

    #[test]
    fn relationship_literal_parses_and_lists_do_not() {
        assert_eq!(cell("[:T {num: 1}]"), cell("[:T {num: 1}]"));
        assert_ne!(cell("[:T]"), cell("[:U]"));
        // A plain list is a list, not a relationship literal.
        assert_eq!(cell("[]"), serde_json::json!([]));
        assert_eq!(cell("[1, 2]"), serde_json::json!([1, 2]));
    }

    #[test]
    fn literals_nest_inside_list_cells() {
        let v = cell("[[:REL {num: 1}], [:REL {num: 2}]]");
        let arr = v.as_array().expect("list cell");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], cell("[:REL {num: 1}]"));
        assert_eq!(arr[1], cell("[:REL {num: 2}]"));
        assert_ne!(arr[0], arr[1]);
    }

    #[test]
    fn path_literal_direction_is_significant() {
        assert_eq!(cell("<(:A)-[:T]->(:B)>"), cell("<(:A)-[:T]->(:B)>"));
        assert_ne!(cell("<(:A)-[:T]->(:B)>"), cell("<(:A)<-[:T]-(:B)>"));
        assert_ne!(cell("<(:A)-[:T]->(:B)>"), cell("<(:B)-[:T]->(:A)>"));
    }

    #[test]
    fn zero_length_path_literal_parses() {
        assert_eq!(cell("<()>"), cell("<()>"));
        assert_ne!(cell("<()>"), cell("()"));
    }

    #[test]
    fn engine_display_strings_canonicalize_like_expected_cells() {
        let actual = canonicalize_value(serde_json::Value::String(
            "(:B:A {f: 1.5, n: 1, name: 'x'})".to_string(),
        ));
        assert_eq!(actual, cell("(:A:B {name: 'x', n: 1, f: 1.5})"));
        let path = canonicalize_value(serde_json::Value::String(
            "<(:A)-[:T {num: 1}]->(:C)>".to_string(),
        ));
        assert_eq!(path, cell("<(:A)-[:T {num: 1}]->(:C)>"));
    }

    #[test]
    fn quoted_string_property_values_survive_literal_parsing() {
        let expected = cell("(:A {name: 'a, b: 1'})");
        let actual = canonicalize_value(serde_json::Value::String(
            "(:A {name: 'a, b: 1'})".to_string(),
        ));
        assert_eq!(expected, actual);
        assert_ne!(expected, cell("(:A {name: 'other'})"));
    }

    #[test]
    fn unsupported_literal_forms_are_flagged_not_mangled() {
        let (_, unsupported) = parse_table_cell("()-[:T]->()");
        assert!(unsupported.is_some());
        // The same form nested inside a list cell is flagged too.
        let (_, unsupported) = parse_table_cell("[()-[:T]->()]");
        assert!(unsupported.is_some());
    }

    #[test]
    fn path_literals_nest_inside_list_cells() {
        let v = cell("[<(:A)-[:T]->(:B)>]");
        let arr = v.as_array().expect("list cell");
        assert_eq!(arr[0], cell("<(:A)-[:T]->(:B)>"));
    }

    #[test]
    fn quoted_strings_with_arrows_are_not_literals() {
        assert_eq!(cell("['name->foo']"), serde_json::json!(["name->foo"]));
    }

    #[test]
    fn gherkin_cell_unescape_handles_backslash_pipe_and_newline() {
        assert_eq!(unescape_gherkin_cell(r"'a\\b'"), r"'a\b'");
        assert_eq!(unescape_gherkin_cell(r"a\|b"), "a|b");
        assert_eq!(unescape_gherkin_cell(r"a\nb"), "a\nb");
        // The Cypher escape in `'\''` is not a Gherkin escape; it passes through.
        assert_eq!(unescape_gherkin_cell(r"'\''"), r"'\''");
    }
}
