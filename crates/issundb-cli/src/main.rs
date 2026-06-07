use std::{collections::HashMap, fs, io::Write, path::PathBuf};

use clap::{CommandFactory, Parser};
use colored::Colorize;
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
// Clap CLI definitions
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "issundb", version, about = "IssunDB command-line interface")]
struct Cli {
    /// Path to the database directory
    db_path: Option<PathBuf>,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true, disable_help_subcommand = true)]
enum ReplCommand {
    /// Open or reopen a database at the given path
    #[command(name = ":open")]
    Open {
        /// Path to the database
        path: PathBuf,
    },

    /// Execute a script file line by line
    #[command(name = ":run")]
    Run {
        /// Path to the script file
        file: String,
    },

    /// Save the output of the next query to a file
    #[command(name = ":save")]
    Save {
        /// Path to the output file
        file: PathBuf,
    },

    /// List all current query parameters
    #[command(name = ":params")]
    Params,

    /// Set a query parameter ($name)
    #[command(name = ":set")]
    Set {
        /// Parameter name
        name: String,
        /// Parameter value (JSON or string)
        value: String,
    },

    /// Remove a query parameter
    #[command(name = ":unset")]
    Unset {
        /// Parameter name
        name: String,
    },

    /// Show the optimized physical plan for a Cypher query
    #[command(name = ":explain")]
    Explain {
        /// The Cypher query
        cypher: String,
    },

    /// Write a hot backup snapshot of the database
    #[command(name = ":backup")]
    Backup {
        /// Path to backup destination
        file: PathBuf,
    },

    /// Write a compacted backup snapshot of the database
    #[command(name = ":backup-compact")]
    BackupCompact {
        /// Path to backup destination
        file: PathBuf,
    },

    /// Import nodes from a JSONL file (one JSON object per line)
    #[command(name = ":import-jsonl")]
    ImportJsonl {
        /// Path to the JSONL file
        file: String,
    },

    /// Import nodes from a CSV file (first column as label, rest as properties)
    #[command(name = ":import-csv")]
    ImportCsv {
        /// Path to the CSV file
        file: String,
    },

    /// Run a Cypher query
    #[command(name = "query", alias = "cypher")]
    Query {
        /// The Cypher query
        cypher: String,
    },

    /// Add a node with a label and optional properties
    #[command(name = "add-node")]
    AddNode {
        /// Node label
        label: String,
        /// Node properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Get a node by its ID
    #[command(name = "get-node")]
    GetNode {
        /// Node ID
        id: u64,
    },

    /// Overwrite a node's properties
    #[command(name = "update-node")]
    UpdateNode {
        /// Node ID
        id: u64,
        /// Node properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Delete a node and its adjacency entries
    #[command(name = "delete-node")]
    DeleteNode {
        /// Node ID
        id: u64,
    },

    /// Add a directed edge with a type and optional properties
    #[command(name = "add-edge")]
    AddEdge {
        /// Source Node ID
        src: u64,
        /// Destination Node ID
        dst: u64,
        /// Edge type/label
        etype: String,
        /// Edge properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Get an edge by its ID
    #[command(name = "get-edge")]
    GetEdge {
        /// Edge ID
        id: u64,
    },

    /// Delete an edge
    #[command(name = "delete-edge")]
    DeleteEdge {
        /// Edge ID
        id: u64,
    },

    /// Get outgoing neighbors of a node
    #[command(name = "out")]
    Out {
        /// Node ID
        id: u64,
    },

    /// Get incoming neighbors of a node
    #[command(name = "in")]
    In {
        /// Node ID
        id: u64,
    },

    /// Find nodes carrying a specific label
    #[command(name = "label")]
    Label {
        /// Node label
        label: String,
    },

    /// Find edges of a specific type
    #[command(name = "etype")]
    Etype {
        /// Edge type
        etype: String,
    },

    /// Display node and edge count statistics
    #[command(name = "stats")]
    Stats,

    /// Run breadth-first expansion traversal
    #[command(name = "bfs")]
    Bfs {
        /// Start Node ID
        id: u64,
        /// Traversal depth (hops limit)
        hops: u8,
    },

    /// Run depth-first expansion traversal
    #[command(name = "dfs")]
    Dfs {
        /// Start Node ID
        id: u64,
        /// Traversal depth (hops limit)
        hops: u8,
    },

    /// Find the shortest unweighted path between two nodes
    #[command(name = "path")]
    Path {
        /// Source Node ID
        src: u64,
        /// Destination Node ID
        dst: u64,
    },

    /// Find the shortest weighted path (Dijkstra) between two nodes
    #[command(name = "wpath")]
    Wpath {
        /// Source Node ID
        src: u64,
        /// Destination Node ID
        dst: u64,
    },

    /// Compute PageRank centrality scores
    #[command(name = "pagerank")]
    Pagerank {
        /// Number of power iterations
        #[arg(default_value = "20")]
        iters: u32,
        /// Damping factor (usually 0.85)
        #[arg(default_value = "0.85")]
        damping: f32,
    },

    /// Find weakly connected components
    #[command(name = "components")]
    Components,

    /// Find strongly connected components
    #[command(name = "scc")]
    Scc,

    /// Check if the graph contains any directed cycle
    #[command(name = "detect-cycle")]
    DetectCycle,

    /// Compute betweenness centrality scores (top 20)
    #[command(name = "betweenness")]
    Betweenness,

    /// Compute harmonic centrality scores (top 20)
    #[command(name = "harmonic")]
    Harmonic,

    /// Compute degree centrality
    #[command(name = "degree")]
    Degree {
        /// Traversal direction: 'in', 'out', or 'both'
        #[arg(default_value = "both")]
        direction: String,
    },

    /// Detect communities via Label Propagation Algorithm
    #[command(name = "community")]
    Community {
        /// Maximum iteration steps
        #[arg(default_value = "10")]
        max_iters: usize,
    },

    /// Find minimum or maximum spanning forest by edge property
    #[command(name = "spanning-forest")]
    SpanningForest {
        /// Property name holding edge weights
        prop: String,
        /// Set to "max" to compute maximum instead of minimum spanning forest
        max: Option<String>,
    },

    /// Compute maximum flow by edge capacity property
    #[command(name = "max-flow")]
    MaxFlow {
        /// Source Node ID
        src: u64,
        /// Sink Node ID
        dst: u64,
        /// Property name holding edge capacity
        prop: String,
    },

    /// Rebuild the CSR snapshot cache
    #[command(name = "rebuild-csr")]
    RebuildCsr,

    /// Attach/upsert a vector embedding on a node
    #[command(name = "upsert-vec")]
    UpsertVec {
        /// Node ID
        id: u64,
        /// Float embedding values
        #[arg(num_args = 1..)]
        values: Vec<f32>,
    },

    /// Query the vector index for k-nearest neighbors
    #[command(name = "vsearch")]
    Vsearch {
        /// Number of results to return
        k: usize,
        /// Query embedding vector values
        #[arg(num_args = 1..)]
        query: Vec<f32>,
    },

    /// Run hybrid vector-graph retrieval search
    #[command(name = "retrieve")]
    Retrieve {
        /// Number of vector seed results
        k: usize,
        /// Traversal hops limit for BFS expansion
        hops: u8,
        /// Query embedding vector values
        #[arg(num_args = 1..)]
        query: Vec<f32>,
    },

    /// Perform full-text search index actions
    #[command(name = "text-index")]
    TextIndex {
        /// Action: 'create', 'drop', or 'list'
        #[arg(value_parser = ["create", "drop", "list"])]
        action: String,
        /// Node label (required for create/drop)
        label: Option<String>,
        /// Node property (required for create/drop)
        property: Option<String>,
    },

    /// Query BM25 full-text search index
    #[command(name = "text-search")]
    TextSearch {
        /// Search query terms
        query: String,
        /// Limit results to label
        label: Option<String>,
        /// Limit results to property
        prop: Option<String>,
        /// Max results to return
        limit: Option<usize>,
    },

    /// Show this help message
    #[command(name = "help")]
    Help,

    /// Exit the program
    #[command(name = "quit", alias = "exit")]
    Quit,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let graph = cli.db_path.as_ref().and_then(|p| match Graph::open(p, 1) {
        Ok(g) => {
            eprintln!("{}", format!("opened: {}", p.display()).green());
            Some(g)
        }
        Err(e) => {
            eprintln!("{}", format!("error opening {}: {e}", p.display()).red());
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
            colorize_prompt("issundb> ", |s| s.green())
        } else {
            colorize_prompt("issundb (no db)> ", |s| s.yellow())
        };

        match rl.readline(&prompt) {
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
    let line_trimmed = line.trim();
    if line_trimmed.is_empty() {
        return true;
    }

    // Cypher shorthand: check if the first token is a known Cypher keyword.
    let (cmd_token, _) = split_cmd(line_trimmed);
    let upper = cmd_token.to_uppercase();
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
        run_cypher(state, line_trimmed);
        return true;
    }

    let tokens = tokenize_line(line_trimmed);
    match ReplCommand::try_parse_from(tokens) {
        Ok(cmd) => {
            if !execute_cmd(state, cmd) {
                return false;
            }
        }
        Err(e) => {
            // Print Clap's parsed errors/help.
            let _ = e.print();
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Execution logic
// ---------------------------------------------------------------------------

fn execute_cmd(state: &mut State, cmd: ReplCommand) -> bool {
    // Check if the command requires an open database.
    let needs_db = !matches!(
        cmd,
        ReplCommand::Open { .. }
            | ReplCommand::Run { .. }
            | ReplCommand::Params
            | ReplCommand::Set { .. }
            | ReplCommand::Unset { .. }
            | ReplCommand::Save { .. }
            | ReplCommand::Help
            | ReplCommand::Quit
    );

    if needs_db && state.graph.is_none() {
        eprintln!("no database open; use :open <path>");
        return true;
    }

    match cmd {
        ReplCommand::Quit => return false,
        ReplCommand::Help => {
            let mut repl_cmd = ReplCommand::command();
            let _ = repl_cmd.print_help();
            println!();
        }
        ReplCommand::Open { path } => match Graph::open(&path, 1) {
            Ok(g) => {
                eprintln!("{}", format!("opened: {}", path.display()).green());
                state.graph = Some(g);
            }
            Err(e) => eprintln!("{}", format!("error: {e}").red()),
        },
        ReplCommand::Run { file } => {
            run_script(state, &file);
        }
        ReplCommand::Save { file } => {
            state.save_path = Some(file.clone());
            eprintln!("next query output will be saved to: {}", file.display());
        }
        ReplCommand::Params => {
            if state.params.is_empty() {
                println!("(no parameters set)");
            } else {
                for (k, v) in &state.params {
                    println!("  ${k} = {v}");
                }
            }
        }
        ReplCommand::Set { name, value } => {
            match serde_json::from_str::<serde_json::Value>(&value) {
                Ok(v) => {
                    state.params.insert(name, v);
                }
                Err(_) => {
                    // Treat bare words / numbers as JSON strings.
                    state.params.insert(name, serde_json::Value::String(value));
                }
            }
        }
        ReplCommand::Unset { name } => {
            state.params.remove(&name);
        }
        ReplCommand::Explain { cypher } => {
            if let Some(g) = &state.graph {
                match g.explain(&cypher) {
                    Ok(plan) => print!("{plan}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Backup { file } => {
            if let Some(g) = &state.graph {
                match g.backup(&file) {
                    Ok(_) => eprintln!("backup written to {}", file.display()),
                    Err(e) => eprintln!("backup failed: {e}"),
                }
            }
        }
        ReplCommand::BackupCompact { file } => {
            if let Some(g) = &state.graph {
                match g.backup_compact(&file) {
                    Ok(_) => eprintln!("compact backup written to {}", file.display()),
                    Err(e) => eprintln!("backup failed: {e}"),
                }
            }
        }
        ReplCommand::ImportJsonl { file } => {
            cmd_import_jsonl(state, &file);
        }
        ReplCommand::ImportCsv { file } => {
            cmd_import_csv(state, &file);
        }
        ReplCommand::Query { cypher } => {
            run_cypher(state, &cypher);
        }
        ReplCommand::AddNode { label, props } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => match g.add_node(&label, &parsed_props) {
                        Ok(id) => println!("{id}"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                }
            }
        }
        ReplCommand::GetNode { id } => {
            if let Some(g) = &state.graph {
                match g.get_node(NodeId::from(id)) {
                    Ok(Some(r)) => {
                        let label = g
                            .node_labels(NodeId::from(id))
                            .unwrap_or_default()
                            .join(":");
                        println!("label={label} props={}", decode_props(&r.props));
                    }
                    Ok(None) => eprintln!("not found"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::UpdateNode { id, props } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => match g.update_node(NodeId::from(id), &parsed_props) {
                        Ok(()) => println!("ok"),
                        Err(e) => eprintln!("error: {e}"),
                    },
                }
            }
        }
        ReplCommand::DeleteNode { id } => {
            if let Some(g) = &state.graph {
                match g.delete_node(NodeId::from(id)) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::AddEdge {
            src,
            dst,
            etype,
            props,
        } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => {
                        match g.add_edge(
                            NodeId::from(src),
                            NodeId::from(dst),
                            &etype,
                            &parsed_props,
                        ) {
                            Ok(id) => println!("{id}"),
                            Err(e) => eprintln!("error: {e}"),
                        }
                    }
                }
            }
        }
        ReplCommand::GetEdge { id } => {
            if let Some(g) = &state.graph {
                match g.get_edge(EdgeId::from(id)) {
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
                }
            }
        }
        ReplCommand::DeleteEdge { id } => {
            if let Some(g) = &state.graph {
                match g.delete_edge(EdgeId::from(id)) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Out { id } => {
            if let Some(g) = &state.graph {
                match g.out_neighbors(NodeId::from(id)) {
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
                }
            }
        }
        ReplCommand::In { id } => {
            if let Some(g) = &state.graph {
                match g.in_neighbors(NodeId::from(id)) {
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
                }
            }
        }
        ReplCommand::Label { label } => {
            if let Some(g) = &state.graph {
                match g.nodes_by_label(&label) {
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
        ReplCommand::Etype { etype } => {
            if let Some(g) = &state.graph {
                match g.edges_by_type(&etype) {
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
        ReplCommand::Stats => {
            if let Some(g) = &state.graph {
                let nodes = g.all_nodes().map(|v| v.len()).unwrap_or(0);
                println!("nodes : {nodes}");
                println!("(use `etype <type>` or `label <label>` for detailed counts)");
            }
        }
        ReplCommand::Bfs { id, hops } => {
            if let Some(g) = &state.graph {
                match g.bfs(NodeId::from(id), hops) {
                    Ok(v) => {
                        println!("{} node(s)", v.len());
                        for x in &v {
                            println!("  {x}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Dfs { id, hops } => {
            if let Some(g) = &state.graph {
                match g.dfs(NodeId::from(id), hops) {
                    Ok(v) => {
                        println!("{} node(s)", v.len());
                        for x in &v {
                            println!("  {x}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Path { src, dst } => {
            if let Some(g) = &state.graph {
                match g.shortest_path(NodeId::from(src), NodeId::from(dst)) {
                    Ok(Some(p)) => println!(
                        "{}",
                        p.iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ),
                    Ok(None) => println!("no path"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Wpath { src, dst } => {
            if let Some(g) = &state.graph {
                match g.shortest_path_dijkstra(NodeId::from(src), NodeId::from(dst)) {
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
        }
        ReplCommand::Pagerank { iters, damping } => {
            if let Some(g) = &state.graph {
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
        }
        ReplCommand::Components => {
            if let Some(g) = &state.graph {
                match g.connected_components() {
                    Ok(map) => {
                        let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                        println!("{} node(s) in {n_comps} component(s)", map.len());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Scc => {
            if let Some(g) = &state.graph {
                match g.strongly_connected_components() {
                    Ok(map) => {
                        let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                        println!(
                            "{} node(s) in {n_comps} strongly connected component(s)",
                            map.len()
                        );
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::DetectCycle => {
            if let Some(g) = &state.graph {
                match g.detect_cycle() {
                    Ok(true) => println!("cycle detected"),
                    Ok(false) => println!("no cycle"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Betweenness => {
            if let Some(g) = &state.graph {
                match g.betweenness_centrality() {
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
                }
            }
        }
        ReplCommand::Harmonic => {
            if let Some(g) = &state.graph {
                match g.harmonic_centrality() {
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
                }
            }
        }
        ReplCommand::Degree { direction } => {
            if let Some(g) = &state.graph {
                let dir = match direction.as_str() {
                    "in" => DegreeDirection::In,
                    "out" => DegreeDirection::Out,
                    _ => DegreeDirection::Both,
                };
                match g.degree_centrality(dir) {
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
        }
        ReplCommand::Community { max_iters } => {
            if let Some(g) = &state.graph {
                match g.label_propagation(max_iters) {
                    Ok(map) => {
                        let n_comps = map.values().collect::<std::collections::HashSet<_>>().len();
                        println!("{} node(s) in {n_comps} community/communities", map.len());
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::SpanningForest { prop, max } => {
            if let Some(g) = &state.graph {
                let maximum = max.as_deref() == Some("max");
                match g.spanning_forest(&prop, maximum) {
                    Ok(edges) => {
                        println!("{} edge(s) in spanning forest", edges.len());
                        for e in &edges {
                            println!("  edge={e}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::MaxFlow { src, dst, prop } => {
            if let Some(g) = &state.graph {
                match g.maximum_flow(NodeId::from(src), NodeId::from(dst), &prop) {
                    Ok(flow) => println!("max flow = {flow:.6}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::RebuildCsr => {
            if let Some(g) = &state.graph {
                match g.rebuild_csr() {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::UpsertVec { id, values } => {
            if let Some(g) = &state.graph {
                match g.upsert_vector(NodeId::from(id), &values) {
                    Ok(()) => println!("ok"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Vsearch { k, query } => {
            if let Some(g) = &state.graph {
                match g.vector_search(&query, k) {
                    Ok(hits) => print_hits(&hits),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Retrieve { k, hops, query } => {
            if let Some(g) = &state.graph {
                let opts = RetrieveOptions {
                    k,
                    hops,
                    ..Default::default()
                };
                match retrieve_with(g, &query, &opts) {
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
        }
        ReplCommand::TextIndex {
            action,
            label,
            property,
        } => {
            if let Some(g) = &state.graph {
                match action.as_str() {
                    "create" => {
                        let lbl = label.as_deref().unwrap_or("");
                        let prop = property.as_deref().unwrap_or("");
                        if lbl.is_empty() || prop.is_empty() {
                            eprintln!("label and property are required for create");
                        } else {
                            match g.create_text_index(lbl, prop) {
                                Ok(()) => println!("ok"),
                                Err(e) => eprintln!("error: {e}"),
                            }
                        }
                    }
                    "drop" => {
                        let lbl = label.as_deref().unwrap_or("");
                        let prop = property.as_deref().unwrap_or("");
                        if lbl.is_empty() || prop.is_empty() {
                            eprintln!("label and property are required for drop");
                        } else {
                            match g.drop_text_index(lbl, prop) {
                                Ok(()) => println!("ok"),
                                Err(e) => eprintln!("error: {e}"),
                            }
                        }
                    }
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
                    _ => {}
                }
            }
        }
        ReplCommand::TextSearch {
            query,
            label,
            prop,
            limit,
        } => {
            if let Some(g) = &state.graph {
                let opts = TextSearchOptions {
                    label,
                    property: prop,
                    limit: limit.unwrap_or(10),
                    ..Default::default()
                };
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
                    Err(e) => eprintln!("{}", format!("error: {e}").red()),
                }
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
        Err(e) => eprintln!("{}", format!("error: {e}").red()),
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
    let header = qr.columns.join("\t");
    let divider = "-".repeat(header.len().max(40));
    let _ = writeln!(out, "{}", header.cyan().bold());
    let _ = writeln!(out, "{}", divider.dimmed());

    for rec in &qr.records {
        let row: Vec<String> = rec.values.iter().map(|v| v.to_string()).collect();
        let _ = writeln!(out, "{}", row.join("\t"));
    }

    let footer = format!(
        "({} row{})",
        qr.records.len(),
        if qr.records.len() == 1 { "" } else { "s" }
    );
    let _ = writeln!(out, "{}", footer.dimmed());
    out
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

fn tokenize_line(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '\0';
    let mut brace_depth: usize = 0;

    for c in s.chars() {
        if c == '{' {
            brace_depth += 1;
            current.push(c);
        } else if c == '}' {
            brace_depth = brace_depth.saturating_sub(1);
            current.push(c);
        } else if (c == '"' || c == '\'') && brace_depth == 0 {
            if in_quotes {
                if c == quote_char {
                    in_quotes = false;
                } else {
                    current.push(c);
                }
            } else {
                in_quotes = true;
                quote_char = c;
            }
        } else if c.is_whitespace() && !in_quotes && brace_depth == 0 {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
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

fn colorize_prompt(
    text: &str,
    style: impl FnOnce(colored::ColoredString) -> colored::ColoredString,
) -> String {
    if colored::control::ShouldColorize::from_env().should_colorize() {
        style(text.into()).to_string()
    } else {
        text.to_string()
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

    if errors > 0 {
        eprintln!("import aborted: {errors} parse error(s) found");
        return;
    }

    // Batch-insert in one write transaction.
    let ok = entries.len();
    match g.update(|txn| {
        for (label, props) in &entries {
            txn.add_node(label, props)?;
        }
        Ok(())
    }) {
        Ok(_) => eprintln!("imported {ok}/{total} nodes"),
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
// Tests
// ---------------------------------------------------------------------------

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
    fn test_tokenize_line() {
        assert_eq!(
            tokenize_line("add-node Person {\"name\": \"Alice\"}"),
            vec![
                "add-node".to_owned(),
                "Person".to_owned(),
                "{\"name\": \"Alice\"}".to_owned()
            ]
        );
        assert_eq!(
            tokenize_line("text-search \"hello world\" Person"),
            vec![
                "text-search".to_owned(),
                "hello world".to_owned(),
                "Person".to_owned()
            ]
        );
        assert_eq!(
            tokenize_line("spanning-forest weight --max"),
            vec![
                "spanning-forest".to_owned(),
                "weight".to_owned(),
                "--max".to_owned()
            ]
        );
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
        assert!(handle(&mut state, "add-node Person {\"name\": \"Alice\"}"));

        // 3. Query node
        assert!(handle(&mut state, "MATCH (n:Person) RETURN n.name"));

        // 4. Algorithm command
        assert!(handle(&mut state, "pagerank"));

        // 5. Quit command should return false
        assert!(!handle(&mut state, "quit"));
    }
}
