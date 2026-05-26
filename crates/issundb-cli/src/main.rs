use std::{collections::HashMap, fs, io::Write, path::PathBuf};

use issundb::{
    EdgeId, Graph, GraphQueryExt, Hit, NodeId, RetrieveOptions, VectorGraphExt, retrieve_with,
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

    println!("IssunDB REPL  —  type `help` for commands, `quit` to exit.");

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
                        .label_name(r.label)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| r.label.to_string());
                    println!("label={label} props={}", decode_props(&r.props));
                }
                Ok(None) => eprintln!("not found"),
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "update-node" => {
            let mut s = rest;
            let id = next_token(&mut s).and_then(|t| t.parse::<u64>().ok());
            let label = next_token(&mut s);
            match (id, label) {
                (Some(id), Some(lbl)) => match parse_props(s.trim()) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(props) => match g.update_node(NodeId::from(id), lbl, &props) {
                        Ok(()) => println!("ok"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                },
                _ => eprintln!("usage: update-node <id> <label> [json]"),
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

        // --- adjacency -------------------------------------------------------
        "out" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: out <id>"),
            Ok(id) => match g.out_neighbors(NodeId::from(id)) {
                Ok(v) => {
                    for (nb, eid, tid) in v {
                        let etype = g
                            .type_name(tid)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| tid.to_string());
                        println!("  node={nb} edge={eid} type={etype}");
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
        },
        "in" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: in <id>"),
            Ok(id) => match g.in_neighbors(NodeId::from(id)) {
                Ok(v) => {
                    for (nb, eid, tid) in v {
                        let etype = g
                            .type_name(tid)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| tid.to_string());
                        println!("  node={nb} edge={eid} type={etype}");
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
  update-node <id> <label> [json]      overwrite a node's label and properties
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
  pagerank [iters] [damping]           PageRank (default: 20 iters, 0.85 damping)
  components                           weakly connected component count

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
