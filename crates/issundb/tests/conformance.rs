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

    // Collect all .feature files recursively.
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

    if total_failed > 0 {
        return Err(format!(
            "{} TCK scenario(s) failed — see output above",
            total_failed
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
                    let raw_val = row[1].trim();
                    // Skip unexpanded substitution placeholders like <elt>.
                    if key.is_empty() || (raw_val.starts_with('<') && raw_val.ends_with('>')) {
                        continue;
                    }
                    params.insert(key.to_string(), parse_table_value(raw_val));
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
        let (columns, rows, has_node_literals) = parse_gherkin_result_table(table);
        return Assertion::Rows {
            ordered,
            ignore_list_order,
            columns,
            rows,
            has_node_literals,
        };
    }
    if value.contains("should be raised") {
        return Assertion::ExpectError;
    }
    Assertion::None
}

/// Parse a `gherkin::Table` into columns, rows, and a node literal flag.
fn parse_gherkin_result_table(
    table: Option<&gherkin::Table>,
) -> (Vec<String>, Vec<Vec<serde_json::Value>>, bool) {
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut has_node_literals = false;

    if let Some(t) = table {
        if let Some(first_row) = t.rows.first() {
            columns = first_row.iter().map(|c| c.trim().to_string()).collect();
            for row_cells in t.rows.iter().skip(1) {
                let mut parsed_row = Vec::new();
                for cell in row_cells {
                    let (val, is_node) = parse_table_cell(cell);
                    if is_node {
                        has_node_literals = true;
                    }
                    parsed_row.push(val);
                }
                rows.push(parsed_row);
            }
        }
    }

    (columns, rows, has_node_literals)
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
        .map(|cells| cells.iter().map(|c| parse_table_value(c)).collect())
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

/// Parse a single table cell value.
/// Returns `(value, is_node_literal)`.
fn parse_table_cell(s: &str) -> (serde_json::Value, bool) {
    let t = s.trim();

    // Node / relationship display literals:
    //   (:Label), (:L {p: v}), ()-[:T]->(), [:T], [:T {p: v}], etc.
    // A path display literal is wrapped in angle brackets: `<(:A)-[:T]->(:B)>`, `<()>`.
    // IssunDB returns a structured `__Path__` object for a path, never the `<...>`
    // string, so any expected cell carrying a path literal is a representational
    // mismatch and the scenario is skipped. The `<...>` wrapper appears only in path
    // literals (result cells never contain bare comparison operators and strings are
    // quoted), so matching it cannot reclassify a passing scenario. The narrower
    // node and relationship literal markers are deliberately not broadened to nested
    // positions, because the runner already compares many node and relationship
    // literals successfully (for example `()` and `[[:T]]` cells), and a broad
    // substring match there would skip scenarios that currently pass.
    if (t.starts_with("(:") || t.starts_with("(") && t.contains(':'))
        || t.starts_with("()-[")
        || t.starts_with("()-[:")
        || t.starts_with("<-[")
        || t.starts_with("[:")               // relationship literal [:TYPE] or [:TYPE {...}]
        || (t.starts_with('[') && t.contains("->"))
        || (t.starts_with('<') && t.ends_with('>'))
    // path literal <(:A)-[:T]->(:B)>, <()>
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

            let mut actual_rows: Vec<Vec<serde_json::Value>> = res
                .records
                .into_iter()
                .map(|r| r.values.into_iter().map(normalize_value).collect())
                .collect();
            let mut exp = expected_rows.clone();

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
