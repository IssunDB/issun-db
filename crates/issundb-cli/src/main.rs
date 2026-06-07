use std::{collections::HashMap, fs, io::Write, path::PathBuf};

use issundb::{
    DegreeDirection, EdgeId, Graph, GraphQueryExt, Hit, NodeId, RetrieveOptions, TextGraphExt,
    TextIndexExt, TextSearchOptions, VectorGraphExt, retrieve_with,
};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

// ---------------------------------------------------------------------------
// History file location
// ---------------------------------------------------------------------------

fn history_path() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".issundb_history"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// CLI state
// ---------------------------------------------------------------------------

struct State {
    graph: Option<Graph>,
    params: HashMap<String, serde_json::Value>,
    /// Path to capture the next query output into, then cleared.
    save_path: Option<PathBuf>,
}

impl State {
    fn new(graph: Option<Graph>) -> Self {
        Self {
            graph,
            params: HashMap::new(),
            save_path: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let db_path = std::env::args().nth(1).map(PathBuf::from);
    let graph = db_path.as_ref().and_then(|p| match Graph::open(p, 1) {
        Ok(g) => {
            eprintln!("opened: {}", p.display());
            Some(g)
        }
        Err(e) => {
            eprintln!("error opening {}: {e}", p.display());
            None
        }
    });

    let mut state = State::new(graph);

    let mut rl = match DefaultEditor::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("readline init failed: {e}");
            return;
        }
    };

    // Load persistent history.
    if let Some(ref hp) = history_path() {
        let _ = rl.load_history(hp);
    }

    println!("IssunDB REPL: type `help` for commands, `quit` to exit.");

    loop {
        let prompt = if state.graph.is_some() {
            "issundb> "
        } else {
            "issundb (no db)> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                if !handle(&mut state, &line) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline: {e}");
                break;
            }
        }
    }

    // Persist history.
    if let Some(ref hp) = history_path() {
        let _ = rl.save_history(hp);
    }
}

// ---------------------------------------------------------------------------
// Top-level command dispatch
// ---------------------------------------------------------------------------

fn handle(state: &mut State, line: &str) -> bool {
    let (cmd, rest) = split_cmd(line);

    match cmd {
        "quit" | "exit" => return false,
        "help" => print_help(),

        // --- database control -----------------------------------------------
        ":open" => {
            if rest.is_empty() {
                eprintln!("usage: :open <path>");
            } else {
                let p = PathBuf::from(rest);
                match Graph::open(&p, 1) {
                    Ok(g) => {
                        eprintln!("opened: {}", p.display());
                        state.graph = Some(g);
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }

        // --- script execution -----------------------------------------------
        ":run" => {
            if rest.is_empty() {
                eprintln!("usage: :run <file>");
            } else {
                run_script(state, rest);
            }
        }

        // --- batch import ---------------------------------------------------
        ":import-jsonl" => {
            if rest.is_empty() {
                eprintln!("usage: :import-jsonl <file>");
            } else {
                cmd_import_jsonl(state, rest);
            }
        }
        ":import-csv" => {
            if rest.is_empty() {
                eprintln!("usage: :import-csv <file>");
            } else {
                cmd_import_csv(state, rest);
            }
        }

        ":explain" => {
            let g = match state.graph.as_ref() {
                Some(g) => g,
                None => {
                    eprintln!("no database open");
                    return true;
                }
            };
            if rest.is_empty() {
                eprintln!("usage: :explain <cypher>");
            } else {
                match g.explain(rest) {
                    Ok(plan) => print!("{}", plan),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }

        // --- backup / restore ------------------------------------------------
        ":backup" => {
            let g = match state.graph.as_ref() {
                Some(g) => g,
                None => {
                    eprintln!("no database open");
                    return true;
                }
            };
            if rest.is_empty() {
                eprintln!("usage: :backup <file>");
            } else {
                let path = std::path::Path::new(rest);
                match g.backup(path) {
                    Ok(_) => eprintln!("backup written to {}", path.display()),
                    Err(e) => eprintln!("backup failed: {e}"),
                }
            }
        }
        ":backup-compact" => {
            let g = match state.graph.as_ref() {
                Some(g) => g,
                None => {
                    eprintln!("no database open");
                    return true;
                }
            };
            if rest.is_empty() {
                eprintln!("usage: :backup-compact <file>");
            } else {
                let path = std::path::Path::new(rest);
                match g.backup_compact(path) {
                    Ok(_) => eprintln!("compact backup written to {}", path.display()),
                    Err(e) => eprintln!("backup failed: {e}"),
                }
            }
        }

        // --- output capture --------------------------------------------------
        ":save" => {
            if rest.is_empty() {
                eprintln!("usage: :save <file>  (output of next query goes to file)");
            } else {
                state.save_path = Some(PathBuf::from(rest));
                eprintln!("next query output will be saved to: {rest}");
            }
        }

        // --- query parameters ------------------------------------------------
        ":params" => {
            if state.params.is_empty() {
                println!("(no parameters set)");
            } else {
                for (k, v) in &state.params {
                    println!("  ${k} = {v}");
                }
            }
        }
        ":set" => {
            let mut s = rest;
            match next_token(&mut s) {
                None => eprintln!("usage: :set <name> <json_value>"),
                Some(name) => {
                    let val_str = s.trim();
                    match serde_json::from_str::<serde_json::Value>(val_str) {
                        Ok(v) => {
                            state.params.insert(name.to_owned(), v);
                        }
                        Err(_) => {
                            // Treat bare words / numbers as JSON strings.
                            state.params.insert(
                                name.to_owned(),
                                serde_json::Value::String(val_str.to_owned()),
                            );
                        }
                    }
                }
            }
        }
        ":unset" => {
            if rest.is_empty() {
                eprintln!("usage: :unset <name>");
            } else {
                state.params.remove(rest);
            }
        }

        // --- everything else needs an open database -------------------------
        _ => {
            if state.graph.is_none() {
                eprintln!("no database open; use :open <path>");
                return true;
            }
            // Cypher shorthand: lines starting with known Cypher keywords.
            let upper = cmd.to_uppercase();
            let is_cypher_kw = matches!(
                upper.as_str(),
                "MATCH"
                    | "CREATE"
                    | "MERGE"
                    | "WITH"
                    | "RETURN"
                    | "DELETE"
                    | "DETACH"
                    | "SET"
                    | "UNWIND"
                    | "CALL"
                    | "OPTIONAL"
                    | "WHERE"
                    | "FOREACH"
            );
            if is_cypher_kw {
                // Reassemble full line and run as Cypher.
                run_cypher(state, line);
            } else {
                dispatch(state, cmd, rest);
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Script execution
// ---------------------------------------------------------------------------

fn run_script(state: &mut State, path: &str) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return;
        }
    };

    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with("--") {
            continue;
        }
        println!("[{}:{}] {line}", path, lineno + 1);
        if !handle(state, line) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Cypher execution
// ---------------------------------------------------------------------------

fn run_cypher(state: &mut State, cypher: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            eprintln!("no database open");
            return;
        }
    };

    let result = if state.params.is_empty() {
        g.query(cypher)
    } else {
        g.query_with_params(cypher, &state.params)
    };

    match result {
        Err(e) => eprintln!("error: {e}"),
        Ok(qr) => {
            let output = format_query_result(&qr);
            if let Some(ref save) = state.save_path.take() {
                match fs::File::create(save) {
                    Ok(mut f) => {
                        if let Err(e) = f.write_all(output.as_bytes()) {
                            eprintln!("write error: {e}");
                        } else {
                            eprintln!("saved to {}", save.display());
                        }
                    }
                    Err(e) => eprintln!("cannot create {}: {e}", save.display()),
                }
            } else {
                print!("{output}");
            }
        }
    }
}

fn format_query_result(qr: &issundb::QueryResult) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if qr.columns.is_empty() {
        let _ = writeln!(out, "(no columns returned)");
        return out;
    }

    // Header.
    let _ = writeln!(out, "{}", qr.columns.join("\t"));
    let _ = writeln!(out, "{}", "-".repeat(qr.columns.join("\t").len().max(40)));

    for rec in &qr.records {
        let row: Vec<String> = rec.values.iter().map(|v| v.to_string()).collect();
        let _ = writeln!(out, "{}", row.join("\t"));
    }

    let _ = writeln!(
        out,
        "({} row{})",
        qr.records.len(),
        if qr.records.len() == 1 { "" } else { "s" }
    );
    out
}

// ---------------------------------------------------------------------------
// Low-level command dispatch
// ---------------------------------------------------------------------------

fn dispatch(state: &mut State, cmd: &str, rest: &str) {
    let Some(g) = &state.graph else {
        return;
    };

    match cmd {
        // --- Cypher ---------------------------------------------------------
        "query" | "cypher" => {
            if rest.is_empty() {
                eprintln!("usage: query <cypher>");
            } else {
                run_cypher(state, rest);
            }
        }

        // --- node CRUD -------------------------------------------------------
        "add-node" => {
            let mut s = rest;
            match next_token(&mut s) {
                None => eprintln!("usage: add-node <label> [json]"),
                Some(label) => match parse_props(s.trim()) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(props) => match g.add_node(label, &props) {
                        Ok(id) => println!("{id}"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                },
            }
        }
        "get-node" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: get-node <id>"),
            Ok(id) => match g.get_node(NodeId::from(id)) {
                Ok(Some(r)) => {
                    let label = g
                        .node_labels(NodeId::from(id))
                        .unwrap_or_default()
                        .join(":");
                    println!("label={label} props={}", decode_props(&r.props));
                }
                Ok(None) => eprintln!("not found"),
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "update-node" => {
            let mut s = rest;
            let id = next_token(&mut s).and_then(|t| t.parse::<u64>().ok());
            match id {
                Some(id) => match parse_props(s.trim()) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(props) => match g.update_node(NodeId::from(id), &props) {
                        Ok(()) => println!("ok"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                },
                None => eprintln!("usage: update-node <id> [json]"),
            }
        }
        "delete-node" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: delete-node <id>"),
            Ok(id) => match g.delete_node(NodeId::from(id)) {
                Ok(()) => println!("ok"),
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "delete-edge" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: delete-edge <id>"),
            Ok(id) => match g.delete_edge(EdgeId::from(id)) {
                Ok(()) => println!("ok"),
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "add-edge" => {
            let parts: Vec<&str> = rest.splitn(4, ' ').collect();
            if parts.len() < 3 {
                eprintln!("usage: add-edge <src> <dst> <type> [json]");
                return;
            }
            let src = parts[0].trim().parse::<u64>();
            let dst = parts[1].trim().parse::<u64>();
            let etype = parts[2].trim();
            let props_str = if parts.len() > 3 { parts[3].trim() } else { "" };
            match (src, dst) {
                (Ok(s), Ok(d)) => match parse_props(props_str) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(props) => {
                        match g.add_edge(NodeId::from(s), NodeId::from(d), etype, &props) {
                            Ok(id) => println!("{id}"),
                            Err(e) => eprintln!("error: {e}"),
                        }
                    }
                },
                _ => eprintln!("usage: add-edge <src> <dst> <type> [json]"),
            }
        }
        "get-edge" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: get-edge <id>"),
            Ok(id) => match g.get_edge(EdgeId::from(id)) {
                Ok(Some(r)) => {
                    let etype = g
                        .type_name(r.edge_type)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| r.edge_type.to_string());
                    println!(
                        "src={} dst={} type={etype} props={}",
                        r.src,
                        r.dst,
                        decode_props(&r.props)
                    );
                }
                Ok(None) => eprintln!("not found"),
                Err(e) => eprintln!("error: {e}"),
            },
        },

        // --- adjacency -------------------------------------------------------
        "out" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: out <id>"),
            Ok(id) => match g.out_neighbors(NodeId::from(id)) {
                Ok(v) => {
                    for ne in v {
                        let etype = g
                            .type_name(ne.edge_type)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| ne.edge_type.to_string());
                        println!("  node={} edge={} type={etype}", ne.node, ne.edge);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "in" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: in <id>"),
            Ok(id) => match g.in_neighbors(NodeId::from(id)) {
                Ok(v) => {
                    for ne in v {
                        let etype = g
                            .type_name(ne.edge_type)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| ne.edge_type.to_string());
                        println!("  node={} edge={} type={etype}", ne.node, ne.edge);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
        },

        // --- label / type indexes -------------------------------------------
        "label" => {
            if rest.is_empty() {
                eprintln!("usage: label <label>");
            } else {
                match g.nodes_by_label(rest) {
                    Ok(ids) => {
                        println!("{} node(s)", ids.len());
                        for id in &ids {
                            println!("  {id}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        "etype" => {
            if rest.is_empty() {
                eprintln!("usage: etype <type>");
            } else {
                match g.edges_by_type(rest) {
                    Ok(ids) => {
                        println!("{} edge(s)", ids.len());
                        for id in &ids {
                            println!("  {id}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }

        // --- statistics ------------------------------------------------------
        "stats" => {
            let nodes = g.all_nodes().map(|v| v.len()).unwrap_or(0);
            println!("nodes : {nodes}");
            // Edge count: sum across all nodes' out-neighbors is not ideal;
            // use node count as a proxy until a dedicated API exists.
            println!("(use `etype <type>` or `label <label>` for detailed counts)");
        }

        // --- graph algorithms ------------------------------------------------
        "bfs" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: bfs <id> <hops>");
                return;
            }
            match (tokens[0].parse::<u64>(), tokens[1].parse::<u8>()) {
                (Ok(n), Ok(h)) => match g.bfs(NodeId::from(n), h) {
                    Ok(v) => {
                        println!("{} node(s)", v.len());
                        for x in &v {
                            println!("  {x}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                },
                _ => eprintln!("usage: bfs <id> <hops>"),
            }
        }
        "dfs" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: dfs <id> <hops>");
                return;
            }
            match (tokens[0].parse::<u64>(), tokens[1].parse::<u8>()) {
                (Ok(n), Ok(h)) => match g.dfs(NodeId::from(n), h) {
                    Ok(v) => {
                        println!("{} node(s)", v.len());
                        for x in &v {
                            println!("  {x}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                },
                _ => eprintln!("usage: dfs <id> <hops>"),
            }
        }
        "path" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: path <src> <dst>");
                return;
            }
            match (tokens[0].parse::<u64>(), tokens[1].parse::<u64>()) {
                (Ok(s), Ok(d)) => match g.shortest_path(NodeId::from(s), NodeId::from(d)) {
                    Ok(Some(p)) => println!(
                        "{}",
                        p.iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ),
                    Ok(None) => println!("no path"),
                    Err(e) => eprintln!("error: {e}"),
                },
                _ => eprintln!("usage: path <src> <dst>"),
            }
        }
        "pagerank" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            let iters: u32 = tokens.first().and_then(|s| s.parse().ok()).unwrap_or(20);
            let damping: f32 = tokens.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.85);
            match g.page_rank(iters, damping) {
                Ok(scores) => {
                    let mut sorted: Vec<_> = scores.iter().collect();
                    sorted.sort_unstable_by(|a, b| b.1.total_cmp(a.1));
                    for (n, s) in sorted.iter().take(20) {
                        println!("  node={n} score={s:.6}");
                    }
                    if sorted.len() > 20 {
                        println!("  ... ({} total)", sorted.len());
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "components" => match g.connected_components() {
            Ok(map) => {
                let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                println!("{} node(s) in {n_comps} component(s)", map.len());
            }
            Err(e) => eprintln!("error: {e}"),
        },
        "scc" => match g.strongly_connected_components() {
            Ok(map) => {
                let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                println!(
                    "{} node(s) in {n_comps} strongly connected component(s)",
                    map.len()
                );
            }
            Err(e) => eprintln!("error: {e}"),
        },
        "detect-cycle" => match g.detect_cycle() {
            Ok(true) => println!("cycle detected"),
            Ok(false) => println!("no cycle"),
            Err(e) => eprintln!("error: {e}"),
        },
        "betweenness" => match g.betweenness_centrality() {
            Ok(scores) => {
                let mut sorted: Vec<_> = scores.iter().collect();
                sorted.sort_unstable_by(|a, b| {
                    b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal)
                });
                for (n, s) in sorted.iter().take(20) {
                    println!("  node={n} betweenness={s:.6}");
                }
                if sorted.len() > 20 {
                    println!("  ... ({} total)", sorted.len());
                }
            }
            Err(e) => eprintln!("error: {e}"),
        },
        "harmonic" => match g.harmonic_centrality() {
            Ok(scores) => {
                let mut sorted: Vec<_> = scores.iter().collect();
                sorted.sort_unstable_by(|a, b| {
                    b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal)
                });
                for (n, s) in sorted.iter().take(20) {
                    println!("  node={n} harmonic={s:.6}");
                }
                if sorted.len() > 20 {
                    println!("  ... ({} total)", sorted.len());
                }
            }
            Err(e) => eprintln!("error: {e}"),
        },
        "degree" => {
            let direction = match rest.trim() {
                "in" => DegreeDirection::In,
                "out" => DegreeDirection::Out,
                _ => DegreeDirection::Both,
            };
            match g.degree_centrality(direction) {
                Ok(scores) => {
                    let mut sorted: Vec<_> = scores.iter().collect();
                    sorted.sort_unstable_by(|a, b| b.1.cmp(a.1));
                    for (n, d) in sorted.iter().take(20) {
                        println!("  node={n} degree={d}");
                    }
                    if sorted.len() > 20 {
                        println!("  ... ({} total)", sorted.len());
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "community" => {
            let max_iters: usize = rest.trim().parse().unwrap_or(10);
            match g.label_propagation(max_iters) {
                Ok(map) => {
                    let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                    println!("{} node(s) in {n_comps} community/communities", map.len());
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "spanning-forest" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            let (weight_prop, maximum) = match tokens.as_slice() {
                [prop] => (*prop, false),
                [prop, "max"] => (*prop, true),
                _ => {
                    eprintln!("usage: spanning-forest <weight_property> [max]");
                    return;
                }
            };
            match g.spanning_forest(weight_prop, maximum) {
                Ok(edges) => {
                    println!("{} edge(s) in spanning forest", edges.len());
                    for e in &edges {
                        println!("  edge={e}");
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "max-flow" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 3 {
                eprintln!("usage: max-flow <src> <dst> <capacity_property>");
                return;
            }
            match (tokens[0].parse::<u64>(), tokens[1].parse::<u64>()) {
                (Ok(s), Ok(d)) => match g.maximum_flow(NodeId::from(s), NodeId::from(d), tokens[2])
                {
                    Ok(flow) => println!("max flow = {flow:.6}"),
                    Err(e) => eprintln!("error: {e}"),
                },
                _ => eprintln!("usage: max-flow <src> <dst> <capacity_property>"),
            }
        }
        "wpath" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: wpath <src> <dst>");
                return;
            }
            match (tokens[0].parse::<u64>(), tokens[1].parse::<u64>()) {
                (Ok(s), Ok(d)) => {
                    match g.shortest_path_dijkstra(NodeId::from(s), NodeId::from(d)) {
                        Ok(Some(wp)) => println!(
                            "cost={:.6} path={}",
                            wp.total_weight,
                            wp.nodes
                                .iter()
                                .map(|n| n.to_string())
                                .collect::<Vec<_>>()
                                .join(" -> ")
                        ),
                        Ok(None) => println!("no path"),
                        Err(e) => eprintln!("error: {e}"),
                    }
                }
                _ => eprintln!("usage: wpath <src> <dst> <weight_property>"),
            }
        }
        "rebuild-csr" => match g.rebuild_csr() {
            Ok(()) => println!("ok"),
            Err(e) => eprintln!("error: {e}"),
        },

        // --- vector search ---------------------------------------------------
        "upsert-vec" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: upsert-vec <id> <f32>...");
                return;
            }
            let id = match tokens[0].parse::<u64>() {
                Ok(n) => NodeId::from(n),
                Err(_) => {
                    eprintln!("usage: upsert-vec <id> <f32>...");
                    return;
                }
            };
            let vec: Result<Vec<f32>, _> = tokens[1..].iter().map(|s| s.parse::<f32>()).collect();
            match vec {
                Err(_) => eprintln!("invalid float in vector"),
                Ok(v) => match g.upsert_vector(id, &v) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                },
            }
        }
        "vsearch" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: vsearch <k> <f32>...");
                return;
            }
            let k = match tokens[0].parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    eprintln!("usage: vsearch <k> <f32>...");
                    return;
                }
            };
            let vec: Result<Vec<f32>, _> = tokens[1..].iter().map(|s| s.parse::<f32>()).collect();
            match vec {
                Err(_) => eprintln!("invalid float in query"),
                Ok(v) => match g.vector_search(&v, k) {
                    Ok(hits) => print_hits(&hits),
                    Err(e) => eprintln!("error: {e}"),
                },
            }
        }
        "retrieve" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 3 {
                eprintln!("usage: retrieve <k> <hops> <f32>...");
                return;
            }
            let k = tokens[0].parse::<usize>();
            let hops = tokens[1].parse::<u8>();
            let vec: Result<Vec<f32>, _> = tokens[2..].iter().map(|s| s.parse::<f32>()).collect();
            match (k, hops, vec) {
                (Ok(k), Ok(h), Ok(v)) => {
                    let opts = RetrieveOptions {
                        k,
                        hops: h,
                        ..Default::default()
                    };
                    match retrieve_with(g, &v, &opts) {
                        Ok(sub) => {
                            println!(
                                "{} node(s), {} edge(s), {} seed(s)",
                                sub.nodes.len(),
                                sub.edges.len(),
                                sub.scores.len()
                            );
                            let mut seeds: Vec<_> = sub.scores.iter().collect();
                            seeds.sort_unstable_by(|a, b| a.1.total_cmp(b.1));
                            for (n, d) in seeds {
                                println!("  seed node={n} dist={d:.6}");
                            }
                        }
                        Err(e) => eprintln!("error: {e}"),
                    }
                }
                _ => eprintln!("usage: retrieve <k> <hops> <f32>..."),
            }
        }

        // --- full-text search ------------------------------------------------
        "text-index" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 3 && !(tokens.len() == 1 && tokens[0] == "list") {
                eprintln!("usage: text-index create|drop|list <label> <property>");
                return;
            }
            match tokens.first().copied().unwrap_or("") {
                "create" => match g.create_text_index(tokens[1], tokens[2]) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                },
                "drop" => match g.drop_text_index(tokens[1], tokens[2]) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                },
                "list" => match g.list_text_indexes() {
                    Ok(idxs) => {
                        if idxs.is_empty() {
                            println!("(no text indexes)");
                        } else {
                            for (label, prop, _lang) in &idxs {
                                println!("  {label}.{prop}");
                            }
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                },
                other => eprintln!("unknown subcommand: {other}; use create, drop, or list"),
            }
        }
        "text-search" => {
            // text-search <query> [label] [prop] [limit]
            let mut s = rest;
            let query = match next_token(&mut s) {
                Some(q) => q.to_owned(),
                None => {
                    eprintln!("usage: text-search <query> [label] [prop] [limit]");
                    return;
                }
            };
            let mut opts = TextSearchOptions::default();
            let tok2 = next_token(&mut s).map(str::to_owned);
            let tok3 = next_token(&mut s).map(str::to_owned);
            let tok4 = next_token(&mut s).map(str::to_owned);
            opts.label = tok2;
            opts.property = tok3;
            if let Some(lim) = tok4 {
                if let Ok(n) = lim.parse::<usize>() {
                    opts.limit = n;
                }
            }
            match g.text_search(&query, &opts) {
                Ok(hits) => {
                    if hits.is_empty() {
                        println!("(no results)");
                    } else {
                        for h in &hits {
                            println!("  node={} score={:.6}", h.node, h.score);
                        }
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }

        _ => eprintln!("unknown command: {cmd}; type `help` for a list"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn split_cmd(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(' ') {
        None => (s, ""),
        Some(i) => (s[..i].trim(), s[i + 1..].trim()),
    }
}

fn next_token<'a>(s: &mut &'a str) -> Option<&'a str> {
    *s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        None => {
            let tok = *s;
            *s = "";
            Some(tok)
        }
        Some(i) => {
            let tok = &s[..i];
            *s = &s[i..];
            Some(tok)
        }
    }
}

fn parse_props(s: &str) -> Result<serde_json::Value, String> {
    if s.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(s).map_err(|e| e.to_string())
}

fn decode_props(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "{}".to_owned();
    }
    rmp_serde::from_slice::<serde_json::Value>(bytes)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| format!("<{} raw bytes>", bytes.len()))
}

fn print_hits(hits: &[Hit]) {
    if hits.is_empty() {
        println!("(no results)");
        return;
    }
    for h in hits {
        println!("  node={} dist={:.6}", h.node, h.distance);
    }
}

// ---------------------------------------------------------------------------
// Batch import helpers
// ---------------------------------------------------------------------------

fn cmd_import_jsonl(state: &mut State, path: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            eprintln!("no database open");
            return;
        }
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return;
        }
    };

    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let mut errors = 0usize;

    // Parse all lines first to fail fast on malformed input.
    let mut entries: Vec<(String, serde_json::Value)> = Vec::with_capacity(total);
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let label = v
                    .get("label")
                    .and_then(|l| l.as_str())
                    .unwrap_or("Node")
                    .to_owned();
                let props = v
                    .get("props")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                entries.push((label, props));
            }
            Err(e) => {
                eprintln!("line {}: parse error: {e}", i + 1);
                errors += 1;
            }
        }
    }

    // Batch-insert in one write transaction.
    let ok = entries.len();
    match g.update(|txn| {
        for (label, props) in &entries {
            txn.add_node(label, props)?;
        }
        Ok(())
    }) {
        Ok(_) => eprintln!("imported {ok}/{total} nodes ({errors} parse errors)"),
        Err(e) => eprintln!("import failed: {e}"),
    }
}

fn cmd_import_csv(state: &mut State, path: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            eprintln!("no database open");
            return;
        }
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return;
        }
    };

    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    let header_line = match lines.next() {
        Some(h) => h,
        None => {
            eprintln!("CSV file is empty");
            return;
        }
    };

    let headers: Vec<&str> = header_line.split(',').map(|s| s.trim()).collect();
    if headers.is_empty() {
        eprintln!("CSV has no columns");
        return;
    }

    // First column contains the node label; remaining columns become properties.
    let prop_headers = &headers[1..];
    let mut entries: Vec<(String, serde_json::Value)> = Vec::new();

    for line in lines {
        let cols: Vec<&str> = line.split(',').collect();
        let label = cols.first().map(|s| s.trim()).unwrap_or("Node").to_owned();
        let mut props = serde_json::Map::new();
        for (j, &header) in prop_headers.iter().enumerate() {
            let val_str = cols.get(j + 1).map(|s| s.trim()).unwrap_or("");
            let val = if val_str.is_empty() {
                serde_json::Value::Null
            } else if let Ok(n) = val_str.parse::<i64>() {
                serde_json::Value::Number(n.into())
            } else if let Ok(f) = val_str.parse::<f64>() {
                serde_json::json!(f)
            } else if val_str.eq_ignore_ascii_case("true") {
                serde_json::Value::Bool(true)
            } else if val_str.eq_ignore_ascii_case("false") {
                serde_json::Value::Bool(false)
            } else {
                serde_json::Value::String(val_str.to_owned())
            };
            props.insert(header.to_owned(), val);
        }
        entries.push((label, serde_json::Value::Object(props)));
    }

    let total = entries.len();
    match g.update(|txn| {
        for (label, props) in &entries {
            txn.add_node(label, props)?;
        }
        Ok(())
    }) {
        Ok(_) => eprintln!("imported {total} nodes from {path}"),
        Err(e) => eprintln!("import failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

fn print_help() {
    println!(
        r#"
Database control
  :open <path>                         open or reopen a database

Scripting and output
  :run <file>                          execute a script file line by line
  :save <file>                         save output of the next query to a file

Batch import
  :import-jsonl <file>                 import nodes from a JSONL file (one JSON object per line: {{"label":"L","props":{{...}}}})
  :import-csv <file>                   import nodes from a CSV file (first column = label, remaining = properties)

Query planning
  :explain <cypher>                    show the optimized physical query plan

Backup and restore
  :backup <file>                       write a hot backup snapshot to <file>
  :backup-compact <file>               write a compacted backup snapshot to <file>

Query parameters
  :params                              list all current parameters
  :set <name> <json>                   set a query parameter ($name)
  :unset <name>                        remove a query parameter

Cypher queries  (also accepted as bare Cypher keywords: MATCH, CREATE, ...)
  query <cypher>                       run a Cypher statement
  cypher <cypher>                      alias for query

Node operations
  add-node <label> [json]              add a node; prints NodeId
  get-node <id>                        get a node by id
  update-node <id> [json]              overwrite a node's properties
  delete-node <id>                     delete a node and its adjacency entries

Edge operations
  add-edge <src> <dst> <type> [json]   add an edge; prints EdgeId
  get-edge <id>                        get an edge by id
  delete-edge <id>                     delete an edge

Adjacency
  out <id>                             outgoing neighbors
  in <id>                              incoming neighbors

Indexes
  label <label>                        nodes by label
  etype <type>                         edges by type
  stats                                node count summary

Graph algorithms
  bfs <id> <hops>                      breadth-first expansion
  dfs <id> <hops>                      depth-first expansion
  path <src> <dst>                     unweighted shortest path
  wpath <src> <dst> <prop>             weighted shortest path (Dijkstra)
  pagerank [iters] [damping]           PageRank (default: 20 iters, 0.85 damping)
  components                           weakly connected components
  scc                                  strongly connected components
  detect-cycle                         cycle detection
  betweenness                          betweenness centrality (top 20)
  harmonic                             harmonic centrality (top 20)
  degree [in|out]                      degree centrality (default: both directions, top 20)
  community [max_iters]                community detection via label propagation (default: 10 iters)
  spanning-forest <prop> [max]         minimum (or maximum) spanning forest by edge property
  max-flow <src> <dst> <prop>          maximum flow by edge capacity property

Full-text search
  text-index create <label> <prop>     create a full-text index on a node property
  text-index drop <label> <prop>       drop a full-text index
  text-index list                      list all active full-text indexes
  text-search <query> [label] [prop] [limit]
                                       BM25 full-text search (defaults: all indexes, limit 10)

Vector search
  upsert-vec <id> <f32>...             attach a vector embedding to a node
  vsearch <k> <f32>...                 k-nearest-neighbor search
  retrieve <k> <hops> <f32>...         hybrid retrieval: vector search + BFS

Maintenance
  rebuild-csr                          rebuild the CSR snapshot cache

  help                                 show this message
  quit / exit                          exit
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_split_cmd() {
        assert_eq!(split_cmd("  quit  "), ("quit", ""));
        assert_eq!(split_cmd(" :open   /tmp/db  "), (":open", "/tmp/db"));
        assert_eq!(
            split_cmd("cypher match (n) return n"),
            ("cypher", "match (n) return n")
        );
    }

    #[test]
    fn test_next_token() {
        let mut s = "  a  b  c ";
        assert_eq!(next_token(&mut s), Some("a"));
        assert_eq!(next_token(&mut s), Some("b"));
        assert_eq!(next_token(&mut s), Some("c"));
        assert_eq!(next_token(&mut s), None);
    }

    #[test]
    fn test_parse_props() {
        assert_eq!(parse_props("").unwrap(), serde_json::json!({}));
        assert_eq!(
            parse_props(r#"{"name": "Alice"}"#).unwrap(),
            serde_json::json!({"name": "Alice"})
        );
        assert!(parse_props("invalid-json").is_err());
    }

    #[test]
    fn test_repl_commands_handle() {
        let temp = TempDir::new().unwrap();
        let mut state = State::new(None);

        // 1. Open database via REPL command
        let open_cmd = format!(":open {}", temp.path().display());
        assert!(handle(&mut state, &open_cmd));
        assert!(state.graph.is_some());

        // 2. Add node via REPL command
        assert!(handle(&mut state, "add Person {\"name\": \"Alice\"}"));

        // 3. Query node
        assert!(handle(&mut state, "MATCH (n:Person) RETURN n.name"));

        // 4. Algorithm command
        assert!(handle(&mut state, "pagerank"));

        // 5. Quit command should return false
        assert!(!handle(&mut state, "quit"));
    }
}
