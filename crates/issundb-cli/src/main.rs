use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use clap::{CommandFactory, Parser};
use colored::Colorize;
use issundb::{
    DegreeDirection, EdgeId, FusionStrategy, Graph, GraphQueryExt, Hit, HybridRetrieveOptions,
    Language, NodeId, PropValue, TextGraphExt, TextIndexExt, TextSearchOptions, VectorGraphExt,
    VectorIndexOptions, VectorMetric, VectorQuantization, VectorSearchOptions, retrieve_hybrid,
};
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};
use std::cell::Cell;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Error tracking
// ---------------------------------------------------------------------------

thread_local! {
    /// Set whenever a command reports an error, so a `:run` script (and the
    /// `--script` batch launch) can fail fast on the first failing command.
    /// Informational and success messages do not touch it.
    static COMMAND_ERRORED: Cell<bool> = const { Cell::new(false) };
}

/// Record that the current command failed.
fn note_command_error() {
    COMMAND_ERRORED.with(|e| e.set(true));
}

/// Read and reset the command-error flag.
fn take_command_error() -> bool {
    COMMAND_ERRORED.with(|e| e.replace(false))
}

/// Like `eprintln!`, but also flags the current command as failed. Use it for
/// error output; keep plain `eprintln!` for informational and success messages.
macro_rules! cli_eprintln {
    ($($arg:tt)*) => {{
        note_command_error();
        eprintln!($($arg)*);
    }};
}

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
    /// Directory of the open database, tracked so `stats` can report on-disk
    /// size. Set on `:open`, cleared on `:close`.
    db_path: Option<PathBuf>,
    params: HashMap<String, serde_json::Value>,
    /// Path to capture the next query output into, then cleared.
    save_path: Option<PathBuf>,
    /// Map size from the launch flag; the `:open` default when the positional
    /// map size is omitted.
    map_size_gb: usize,
    /// True while a `:run` script is executing, so the `:!` shell escape can
    /// refuse to run from inside a script file.
    in_script: bool,
}

impl State {
    fn new(graph: Option<Graph>, db_path: Option<PathBuf>, map_size_gb: usize) -> Self {
        Self {
            graph,
            db_path,
            params: HashMap::new(),
            save_path: None,
            map_size_gb,
            in_script: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Clap CLI definitions
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "issundb-cli",
    version,
    about = "IssunDB command-line interface"
)]
struct Cli {
    /// Path to the database directory. Defaults to the ISSUNDB_DB_PATH
    /// environment variable when set (the container image sets it to /data).
    #[arg(env = "ISSUNDB_DB_PATH")]
    db_path: Option<PathBuf>,

    /// LMDB memory map size in gigabytes (defaults to 1).
    #[arg(long, default_value_t = 1)]
    map_size_gb: usize,

    /// Execute a script file then exit, instead of starting the interactive
    /// prompt (e.g., `--script ./setup.txt`). Lines may mix meta, data, and
    /// Cypher, exactly like `:run`; the `:!` shell escape is rejected. Execution
    /// stops at the first failing command, and the process exits non-zero.
    #[arg(long, short = 'f')]
    script: Option<String>,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true, disable_help_subcommand = true)]
enum ReplCommand {
    /// Open or reopen a database at the given path (e.g., `:open ./issundb-data 1`)
    #[command(name = ":open")]
    Open {
        /// Path to the database
        path: PathBuf,
        /// Optional map size in GB (defaults to the launch `--map-size-gb` value)
        map_size_gb: Option<usize>,
    },

    /// Close the open database without exiting the CLI (e.g., `:close`)
    #[command(name = ":close")]
    Close,

    /// Execute a script file line by line (e.g., `:run ./script.txt`). Each line
    /// runs through the same dispatcher as the prompt, so a script may mix meta
    /// commands, data commands, and Cypher; blank lines and `//` or `--` comments
    /// are skipped. Execution stops at the first failing command. The `:!` shell
    /// escape is rejected inside a script.
    #[command(name = ":run")]
    Run {
        /// Path to the script file
        file: String,
    },

    /// Run a shell command (e.g., `:! ls -l ./data`). Disabled inside `:run`
    /// scripts.
    #[command(name = ":!", alias = ":shell")]
    Shell {
        /// The shell command and its arguments
        #[arg(num_args = 1.., allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Save the output of the next query to a file (e.g., `:save ./output.txt`)
    #[command(name = ":save")]
    Save {
        /// Path to the output file
        file: PathBuf,
    },

    /// List all current query parameters (e.g., `:params`)
    #[command(name = ":params")]
    Params,

    /// Set a query parameter (e.g., `:set limit 10` or `:set person {"name": "Alice"}`)
    #[command(name = ":set")]
    Set {
        /// Parameter name
        name: String,
        /// Parameter value (JSON or string)
        value: String,
    },

    /// Remove a query parameter (e.g., `:unset limit`)
    #[command(name = ":unset")]
    Unset {
        /// Parameter name
        name: String,
    },

    /// Write a hot backup snapshot of the database (e.g., `:backup ./backup.db`)
    #[command(name = ":backup")]
    Backup {
        /// Path to backup destination
        file: PathBuf,
    },

    /// Write a compacted backup snapshot of the database (e.g., `:backup-compact ./compact.db`)
    #[command(name = ":backup-compact")]
    BackupCompact {
        /// Path to backup destination
        file: PathBuf,
    },

    /// Restore a snapshot into a new database directory (e.g., `:restore ./snap.db ./restored`)
    #[command(name = ":restore")]
    Restore {
        /// Path to the snapshot file to restore from
        snapshot: PathBuf,
        /// Destination directory for the restored database
        dst: PathBuf,
    },

    /// Bulk-import nodes from a CSV or Parquet file whose columns become node
    /// properties (e.g., `:import-nodes ./people.csv Person`). A `.parquet` file
    /// extension selects the Parquet reader; any other extension is read as CSV
    /// where the first row is the header. Each remaining row, or each Parquet
    /// row, is one node of the given label. Rows are inserted in batched
    /// transactions.
    #[command(name = ":import-nodes")]
    ImportNodes {
        /// Path to the CSV or Parquet file (one node per row)
        file: String,
        /// Label applied to every imported node
        label: String,
    },

    /// Bulk-import edges from a two-column CSV or Parquet file of
    /// source/destination domain keys (e.g., `:import-edges ./knows.csv Person
    /// Person KNOWS`). A `.parquet` file extension selects the Parquet reader,
    /// whose first two columns are the source and destination keys; any other
    /// extension is read as CSV. Each key is resolved to a node by its
    /// auto-indexed `Id` property and edges are inserted in batched transactions.
    #[command(name = ":import-edges")]
    ImportEdges {
        /// Path to the two-column CSV or Parquet file (src_key, dst_key)
        file: String,
        /// Label of the source nodes (matched on their `Id` property)
        src_label: String,
        /// Label of the destination nodes (matched on their `Id` property)
        dst_label: String,
        /// Relationship type for the created edges
        etype: String,
    },

    /// Rebuild the CSR snapshot cache (e.g., `rebuild-csr`)
    #[command(name = "rebuild-csr")]
    RebuildCsr,

    /// Show the optimized physical plan for a Cypher query (e.g., `:explain MATCH (n) RETURN n`)
    #[command(name = ":explain")]
    Explain {
        /// The Cypher query
        #[arg(num_args = 1..)]
        cypher: Vec<String>,
    },

    /// Run a Cypher query (e.g., `query MATCH (n) RETURN n`)
    #[command(name = "query", alias = "cypher")]
    Query {
        /// The Cypher query
        #[arg(num_args = 1..)]
        cypher: Vec<String>,
    },

    /// Add a node with one or more colon-separated labels and optional properties
    /// (e.g., `add-node Person {"name": "Alice"}` or `add-node Person:Admin {...}`)
    #[command(name = "add-node")]
    AddNode {
        /// Node label, or colon-separated labels for a multi-label node (e.g., `Person:Admin`)
        label: String,
        /// Node properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Get a node by its ID (e.g., `get-node 1`)
    #[command(name = "get-node")]
    GetNode {
        /// Node ID
        id: u64,
    },

    /// Overwrite a node's properties (e.g., `update-node 1 {"name": "Bob"}`)
    #[command(name = "update-node")]
    UpdateNode {
        /// Node ID
        id: u64,
        /// Node properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Delete a node and its adjacency entries (e.g., `delete-node 1`)
    #[command(name = "delete-node")]
    DeleteNode {
        /// Node ID
        id: u64,
    },

    /// Add a label to a node (e.g., `add-label 1 Admin`)
    #[command(name = "add-label")]
    AddLabel {
        /// Node ID
        id: u64,
        /// Label to add
        label: String,
    },

    /// Remove a label from a node (e.g., `remove-label 1 Admin`)
    #[command(name = "remove-label")]
    RemoveLabel {
        /// Node ID
        id: u64,
        /// Label to remove
        label: String,
    },

    /// Add a directed edge with a type and optional properties (e.g., `add-edge 1 2 KNOWS {"since": 2020}`)
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

    /// Get an edge by its ID (e.g., `get-edge 5`)
    #[command(name = "get-edge")]
    GetEdge {
        /// Edge ID
        id: u64,
    },

    /// Overwrite an edge's properties (e.g., `update-edge 5 {"weight": 2.5}`)
    #[command(name = "update-edge")]
    UpdateEdge {
        /// Edge ID
        id: u64,
        /// Edge properties JSON
        #[arg(default_value = "{}")]
        props: String,
    },

    /// Delete an edge (e.g., `delete-edge 5`)
    #[command(name = "delete-edge")]
    DeleteEdge {
        /// Edge ID
        id: u64,
    },

    /// Get outgoing neighbors of a node (e.g., `out 1`)
    #[command(name = "out")]
    Out {
        /// Node ID
        id: u64,
    },

    /// Get incoming neighbors of a node (e.g., `in 1`)
    #[command(name = "in")]
    In {
        /// Node ID
        id: u64,
    },

    /// Find nodes carrying a specific label (e.g., `label Person`)
    #[command(name = "label")]
    Label {
        /// Node label
        label: String,
    },

    /// Find edges of a specific type (e.g., `etype KNOWS`)
    #[command(name = "etype")]
    Etype {
        /// Edge type
        etype: String,
    },

    /// Display database and graph statistics: on-disk size, node and edge
    /// counts, per-label and per-type breakdowns, indexes, constraints, text
    /// indexes, and vector count (e.g., `stats`)
    #[command(name = "stats")]
    Stats,

    /// Run breadth-first expansion traversal (e.g., `bfs 1 2`)
    #[command(name = "bfs")]
    Bfs {
        /// Start Node ID
        id: u64,
        /// Traversal depth (hops limit)
        hops: u8,
    },

    /// Run depth-first expansion traversal (e.g., `dfs 1 2`)
    #[command(name = "dfs")]
    Dfs {
        /// Start Node ID
        id: u64,
        /// Traversal depth (hops limit)
        hops: u8,
    },

    /// Find the shortest unweighted path between two nodes (e.g., `path 1 2`)
    #[command(name = "path")]
    Path {
        /// Source Node ID
        src: u64,
        /// Destination Node ID
        dst: u64,
    },

    /// Find the shortest weighted path between two nodes (e.g., `wpath 1 2`)
    #[command(name = "wpath")]
    Wpath {
        /// Source Node ID
        src: u64,
        /// Destination Node ID
        dst: u64,
    },

    /// Compute PageRank centrality scores (e.g., `pagerank 20 0.85`)
    #[command(name = "pagerank")]
    Pagerank {
        /// Number of power iterations
        #[arg(default_value = "20")]
        iters: u32,
        /// Damping factor (usually 0.85)
        #[arg(default_value = "0.85")]
        damping: f32,
    },

    /// Find weakly connected components (e.g., `components`)
    #[command(name = "components")]
    Components,

    /// Compute degree centrality (e.g., `degree out` or `degree both`)
    #[command(name = "degree")]
    Degree {
        /// Traversal direction: 'in', 'out', or 'both'
        #[arg(default_value = "both")]
        direction: String,
    },

    /// Attach/upsert a vector embedding on a node (e.g., `upsert-vec 1 0.1 0.2 0.3`)
    #[command(name = "upsert-vec")]
    UpsertVec {
        /// Node ID
        id: u64,
        /// Float embedding values
        #[arg(num_args = 1.., allow_negative_numbers = true)]
        values: Vec<f32>,
    },

    /// Remove the vector embedding from a node (e.g., `remove-vec 1`)
    #[command(name = "remove-vec")]
    RemoveVec {
        /// Node ID
        id: u64,
    },

    /// Query the vector index for k-nearest neighbors, optionally filtered by
    /// label and property values (e.g., `vsearch 5 0.1 0.2 0.3 --label Person`)
    #[command(name = "vsearch")]
    Vsearch {
        /// Number of results to return
        k: usize,
        /// Query embedding vector values
        #[arg(num_args = 1.., allow_negative_numbers = true)]
        query: Vec<f32>,
        /// Restrict results to nodes carrying this label
        #[arg(long)]
        label: Option<String>,
        /// JSON object of property key-value filters (e.g., `--props {"name":"Alice"}`)
        #[arg(long)]
        props: Option<String>,
        /// Optional rescore factor for quantized indexes
        #[arg(long = "rescore-factor")]
        rescore_factor: Option<usize>,
    },

    /// Run hybrid retrieval over vector seeds, optional text seeds, and graph
    /// expansion (e.g., `retrieve 5 2 0.1 0.2 0.3 --text alice`)
    #[command(name = "retrieve")]
    Retrieve {
        /// Number of seed results from each enabled source
        k: usize,
        /// Traversal hops limit for BFS expansion
        hops: u8,
        /// Query embedding vector values
        #[arg(num_args = 1.., allow_negative_numbers = true)]
        query: Vec<f32>,
        /// Full-text query string to add text seeds (vector-only when omitted)
        #[arg(long)]
        text: Option<String>,
        /// Restrict text seeds to this label
        #[arg(long = "text-label")]
        text_label: Option<String>,
        /// Restrict text seeds to this property
        #[arg(long = "text-prop")]
        text_property: Option<String>,
        /// Score fusion strategy: 'rrf' or 'weighted_sum'
        #[arg(long, default_value = "rrf", value_parser = ["rrf", "weighted_sum"])]
        fusion: String,
    },

    /// Configure or reindex vector metric and quantization (e.g., `configure-vec cosine float32`)
    #[command(name = "configure-vec")]
    ConfigureVec {
        /// Metric: 'cosine', 'l2', or 'dot'
        #[arg(value_parser = ["cosine", "l2", "dot"])]
        metric: String,
        /// Quantization: 'float32', 'float16', or 'int8'
        #[arg(value_parser = ["float32", "float16", "int8"], default_value = "float32")]
        quantization: String,
        /// Rebuild the index if vector embeddings already exist
        #[arg(short, long)]
        reindex: bool,
    },

    /// Perform full-text index actions (e.g., `text-index create Person name --lang english` or `text-index list`)
    #[command(name = "text-index")]
    TextIndex {
        /// Action: 'create', 'drop', 'list', or 'has'
        #[arg(value_parser = ["create", "drop", "list", "has"])]
        action: String,
        /// Node label (required for create/drop)
        label: Option<String>,
        /// Node property (required for create/drop)
        property: Option<String>,
        /// Index language for stemming
        #[arg(short, long, default_value = "english", value_parser = ["english", "spanish", "french", "german", "italian", "portuguese"])]
        lang: String,
    },

    /// Query BM25 full-text search index (e.g., `text-search "alice" Person name 5`)
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

    /// Show the CLI version (e.g., `:version`)
    #[command(name = ":version")]
    Version,

    /// Set the thread count for GraphBLAS matrix computations (e.g., `:threads 4`)
    #[command(name = ":threads")]
    Threads {
        /// Number of threads (0 to restore default behavior)
        count: i32,
    },

    /// Show this help message (e.g., `help`)
    #[command(name = "help")]
    Help,

    /// Exit the program (e.g., `quit` or `exit`)
    #[command(name = "quit", alias = "exit")]
    Quit,
}

// ---------------------------------------------------------------------------
// Help Text Grouping
// ---------------------------------------------------------------------------

const HELP_TEXT: &str = r#"
Database Control
  :open <path> [map_size_gb]           Open or reopen a database at the given path (e.g., :open ./issundb-data 2)
  :close                               Close the open database without exiting the CLI
  :threads <count>                     Set the thread count for GraphBLAS computations (e.g., :threads 4)

Scripting and Parameters
  :run <file>                          Execute a script file line by line, stopping at the first failing command (e.g., :run ./setup.txt)
  :! / :shell <command>                Run a shell command (e.g., :! ls -l ./data); note that it's disabled inside :run scripts
  :save <file>                         Save the output of the next query to a file (e.g., :save ./output.txt)
  :params                              List all current query parameters
  :set <name> <value>                  Set a query parameter (e.g., :set limit 10 or :set person {"name": "Alice"})
  :unset <name>                        Remove a query parameter (e.g., :unset limit)

Backup and Import
  :backup <file>                       Write a hot backup snapshot of the database (e.g., :backup ./backup.db)
  :backup-compact <file>               Write a compacted backup snapshot of the database (e.g., :backup-compact ./compact.db)
  :restore <snapshot> <dst>            Restore a snapshot into a new database directory (e.g., :restore ./snap.db ./restored)
  :import-nodes <file> <label>         Bulk-import nodes from a CSV or Parquet file whose columns become properties (e.g., :import-nodes ./people.parquet Person)
  :import-edges <file> <src> <dst> <type>  Bulk-import edges from a 2-column CSV or Parquet file of domain keys (e.g., :import-edges ./knows.parquet Person Person KNOWS)
  rebuild-csr                          Rebuild the CSR snapshot cache

Query and Mutations
  :explain <cypher>                    Show the optimized physical plan for a Cypher query (e.g., :explain MATCH (n) RETURN n)
  query / cypher <cypher>              Run a Cypher query (e.g., query MATCH (n) RETURN n)
  add-node <label[:label...]> [props]  Add a node with one or more colon-separated labels (e.g., add-node Person:Admin {"name": "Alice"})
  get-node <id>                        Get a node by its ID (e.g., get-node 1)
  update-node <id> [props]             Overwrite a node's properties (e.g., update-node 1 {"name": "Bob"})
  delete-node <id>                     Delete a node and its adjacency entries (e.g., delete-node 1)
  add-label <id> <label>               Add a label to a node (e.g., add-label 1 Admin)
  remove-label <id> <label>            Remove a label from a node (e.g., remove-label 1 Admin)
  add-edge <src> <dst> <type> [props]  Add a directed edge with a type and optional properties (e.g., add-edge 1 2 KNOWS {"since": 2020})
  get-edge <id>                        Get an edge by its ID (e.g., get-edge 5)
  update-edge <id> [props]             Overwrite an edge's properties (e.g., update-edge 5 {"weight": 2.5})
  delete-edge <id>                     Delete an edge (e.g., delete-edge 5)
  out <id>                             Get outgoing neighbors of a node (e.g., out 1)
  in <id>                              Get incoming neighbors of a node (e.g., in 1)
  label <label>                        Find nodes carrying a specific label (e.g., label Person)
  etype <type>                         Find edges of a specific type (e.g., etype KNOWS)
  stats                                Show database and graph statistics like sizes, counts, indexes, etc.

Graph Algorithms
  bfs <id> <hops>                      Run breadth-first expansion traversal (e.g., bfs 1 2)
  dfs <id> <hops>                      Run depth-first expansion traversal (e.g., dfs 1 2)
  path <src> <dst>                     Find the shortest unweighted path between two nodes (e.g., path 1 2)
  wpath <src> <dst>                    Find the shortest weighted path between two nodes (e.g., wpath 1 2)
  pagerank [iters] [damping]           Compute PageRank centrality scores (e.g., pagerank 20 0.85)
  components                           Find weakly connected components
  degree [in|out|both]                 Compute degree centrality (e.g., degree out or degree both)

Vector and Text Search
  configure-vec <metric> [quantization] Configure or reindex vector metric and quantization (e.g., configure-vec cosine float32)
  upsert-vec <id> <values...>          Attach/upsert a vector embedding on a node (e.g., upsert-vec 1 0.1 0.2 0.3)
  vsearch <k> <query...> [--label L]   Query the vector index for k-nearest neighbors (e.g., vsearch 5 0.1 0.2 0.3 --label Person)
  retrieve <k> <hops> <query...>       Run hybrid retrieval; add --text <q> for text seeds (e.g., retrieve 5 2 0.1 0.2 0.3 --text alice)
  text-index <act> [label] [property]  Perform full-text index actions (e.g., text-index create Person name --lang german)
  text-search <q> [l] [p] [limit]      Query BM25 full-text search index (e.g., text-search "alice" Person name 5)

System
  :version                             Show the IssunDB version
  help                                 Show this help message
  quit / exit                          Exit the CLI
"#;

fn print_help() {
    for line in HELP_TEXT.lines() {
        if line.trim().is_empty() {
            println!();
            continue;
        }
        let leading_spaces = line.len() - line.trim_start().len();
        if leading_spaces == 0 {
            println!("{}", line.bold().blue());
        } else {
            let syntax_limit = 39;
            if line.len() < syntax_limit {
                let trimmed = line.trim();
                let (cmd, args) = if let Some(idx) = trimmed.find(['<', '[']) {
                    let (c, a) = trimmed.split_at(idx);
                    let cmd_trimmed = c.trim_end();
                    let spaces_count = c.len() - cmd_trimmed.len();
                    (cmd_trimmed, format!("{}{}", " ".repeat(spaces_count), a))
                } else {
                    (trimmed, "".to_owned())
                };
                let colored_cmd = if cmd.starts_with(':') {
                    cmd.cyan()
                } else {
                    cmd.green()
                };
                println!("{}{}{}", " ".repeat(leading_spaces), colored_cmd, args);
            } else {
                let (syntax, desc) = line.split_at(syntax_limit);
                let trimmed_syntax = syntax.trim();
                let (cmd, args) = if let Some(idx) = trimmed_syntax.find(['<', '[']) {
                    let (c, a) = trimmed_syntax.split_at(idx);
                    let cmd_trimmed = c.trim_end();
                    let spaces_count = c.len() - cmd_trimmed.len();
                    (cmd_trimmed, format!("{}{}", " ".repeat(spaces_count), a))
                } else {
                    (trimmed_syntax, "".to_owned())
                };
                let colored_cmd = if cmd.starts_with(':') {
                    cmd.cyan()
                } else {
                    cmd.green()
                };

                let uncolored_len = cmd.len() + args.len();
                let target_len = syntax.len() - leading_spaces;
                let padding = target_len.saturating_sub(uncolored_len);

                print!(
                    "{}{}{}{}",
                    " ".repeat(leading_spaces),
                    colored_cmd,
                    args,
                    " ".repeat(padding)
                );
                println!("{}", desc.dimmed());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let opened = cli
        .db_path
        .as_ref()
        .and_then(|p| match Graph::open(p, cli.map_size_gb) {
            Ok(g) => {
                eprintln!("{}", format!("opened: {}", p.display()).green());
                Some((g, p.clone()))
            }
            Err(e) => {
                eprintln!("{}", format!("error opening {}: {e}", p.display()).red());
                None
            }
        });
    let (graph, db_path) = match opened {
        Some((g, p)) => (Some(g), Some(p)),
        None => (None, None),
    };

    let mut state = State::new(graph, db_path, cli.map_size_gb);

    // Batch mode: run the script then exit without starting the prompt. The
    // script may `:open` its own database, so a launch path is not required.
    // Exit non-zero if any command in the script failed.
    if let Some(script) = cli.script.as_deref() {
        if !run_script(&mut state, script) {
            std::process::exit(1);
        }
        return;
    }

    let mut rl: Editor<ReplHelper, FileHistory> = match Editor::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("readline init failed: {e}");
            return;
        }
    };
    rl.set_helper(Some(ReplHelper::new()));

    // Load persistent history.
    if let Some(ref hp) = history_path() {
        let _ = rl.load_history(hp);
    }

    println!(
        "IssunDB CLI (v{}): type `help` for command list, `quit` to exit.",
        env!("CARGO_PKG_VERSION")
    );

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
// Tab completion
// ---------------------------------------------------------------------------

/// Cypher clause keywords the REPL recognizes as a leading token, routing the
/// whole line to the query path without a `query`/`cypher` prefix. Shared by the
/// dispatcher and the completer so both stay in sync.
const CYPHER_KEYWORDS: &[&str] = &[
    "MATCH", "CREATE", "MERGE", "WITH", "RETURN", "DELETE", "DETACH", "SET", "UNWIND", "CALL",
    "OPTIONAL", "WHERE", "FOREACH", "EXPORT", "IMPORT",
];

/// REPL commands that take filesystem-path arguments, paired with the 1-based
/// argument positions (counting from the first word after the command) that are
/// paths. The completer delegates only those positions to `FilenameCompleter`;
/// the remaining arguments (labels, relationship types) get no completion so the
/// filesystem is not offered where a name is expected. For example,
/// `:import-nodes <file> <label>` lists only position 1 as a path.
const FILE_ARG_COMMANDS: &[(&str, &[usize])] = &[
    (":run", &[1]),
    (":save", &[1]),
    (":backup", &[1]),
    (":backup-compact", &[1]),
    (":restore", &[1, 2]),
    (":import-nodes", &[1]),
    (":import-edges", &[1]),
];

/// rustyline helper providing tab completion for the REPL: command names and
/// Cypher keywords in the first token, filesystem paths in the arguments of the
/// path-taking commands. Hinting, highlighting, and validation are left at their
/// trait defaults.
struct ReplHelper {
    filename: FilenameCompleter,
    /// All completable first-token words, sorted and deduplicated: every Clap
    /// command name (`:open`, `add-node`, `help`, `quit`, ...) plus the Cypher
    /// leading keywords. Sourced from the Clap command tree so it cannot drift.
    commands: Vec<String>,
}

impl ReplHelper {
    fn new() -> Self {
        let mut commands: Vec<String> = ReplCommand::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        commands.extend(CYPHER_KEYWORDS.iter().map(|k| (*k).to_string()));
        commands.sort();
        commands.dedup();
        Self {
            filename: FilenameCompleter::new(),
            commands,
        }
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        // Start byte of the word under the cursor. Whitespace is ASCII, so the
        // byte index after it is a valid char boundary.
        let word_start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        let word = &head[word_start..];
        let is_first_token = head[..word_start].trim().is_empty();

        if is_first_token {
            let needle = word.to_ascii_uppercase();
            let candidates = self
                .commands
                .iter()
                .filter(|name| name.to_ascii_uppercase().starts_with(&needle))
                .map(|name| Pair {
                    display: name.clone(),
                    replacement: name.clone(),
                })
                .collect();
            return Ok((word_start, candidates));
        }

        // Argument position: complete filesystem paths only for the path-typed
        // argument positions of the path commands. The 1-based index of the word
        // under the cursor is the count of tokens preceding it (the command is
        // index 0, so the first argument is index 1).
        let cmd = head.split_whitespace().next().unwrap_or("");
        let arg_index = head[..word_start].split_whitespace().count();
        if let Some((_, paths)) = FILE_ARG_COMMANDS.iter().find(|(name, _)| *name == cmd) {
            if paths.contains(&arg_index) {
                return self.filename.complete(line, pos, ctx);
            }
        }

        Ok((pos, Vec::new()))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Highlighter for ReplHelper {}

impl Validator for ReplHelper {}

impl Helper for ReplHelper {}

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
    if CYPHER_KEYWORDS.contains(&upper.as_str()) {
        run_cypher(state, line_trimmed);
        return true;
    }

    // The `query`, `cypher`, and `:explain` commands carry a raw Cypher body.
    // Route it verbatim instead of through tokenize_line, which strips the
    // quotes that delimit Cypher string literals (so `query RETURN 'x'` keeps
    // its literal rather than degrading to the bare variable `x`).
    match classify_raw_cypher(line_trimmed) {
        Some(RawCypher::Query(body)) => {
            if body.is_empty() {
                cli_eprintln!("usage: query <cypher>");
            } else {
                run_cypher(state, body);
            }
            return true;
        }
        Some(RawCypher::Explain(body)) => {
            if body.is_empty() {
                cli_eprintln!("usage: :explain <cypher>");
            } else {
                run_explain(state, body);
            }
            return true;
        }
        None => {}
    }

    let tokens = tokenize_line(line_trimmed);
    if tokens.len() == 1 && (tokens[0] == "help" || tokens[0] == "-h" || tokens[0] == "--help") {
        print_help();
        return true;
    }

    match ReplCommand::try_parse_from(tokens) {
        Ok(cmd) => {
            if !execute_cmd(state, cmd) {
                return false;
            }
        }
        Err(e) => {
            // Print Clap's parsed errors/help. A help or version display is not
            // a failure, but a genuine parse error (unknown command or bad
            // arguments) is, so a script can fail fast on it.
            use clap::error::ErrorKind;
            if !matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                note_command_error();
            }
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
            | ReplCommand::Close
            | ReplCommand::Run { .. }
            | ReplCommand::Shell { .. }
            | ReplCommand::Params
            | ReplCommand::Set { .. }
            | ReplCommand::Unset { .. }
            | ReplCommand::Save { .. }
            | ReplCommand::Restore { .. }
            | ReplCommand::Help
            | ReplCommand::Quit
            | ReplCommand::Version
    );

    if needs_db && state.graph.is_none() {
        cli_eprintln!("no database open; use :open <path>");
        return true;
    }

    match cmd {
        ReplCommand::Quit => return false,
        ReplCommand::Help => {
            print_help();
        }
        ReplCommand::Version => {
            println!("IssunDB v{}", env!("CARGO_PKG_VERSION"));
        }
        ReplCommand::Threads { count } => {
            if let Some(ref g) = state.graph {
                match g.set_thread_count(count) {
                    Ok(_) => {
                        println!("{}", format!("thread count set to: {}", count).green());
                    }
                    Err(e) => {
                        cli_eprintln!("{}", format!("error: {}", e).red());
                    }
                }
            }
        }
        ReplCommand::Close => {
            // Dropping the Graph closes the LMDB environment. The prompt falls
            // back to the "(no db)" state until the next :open.
            if state.graph.take().is_some() {
                state.db_path = None;
                eprintln!("{}", "closed".green());
            } else {
                eprintln!("no database open");
            }
        }
        ReplCommand::Open { path, map_size_gb } => {
            match Graph::open(&path, map_size_gb.unwrap_or(state.map_size_gb)) {
                Ok(g) => {
                    eprintln!("{}", format!("opened: {}", path.display()).green());
                    state.graph = Some(g);
                    state.db_path = Some(path);
                }
                Err(e) => cli_eprintln!("{}", format!("error: {e}").red()),
            }
        }
        ReplCommand::Run { file } => {
            // Propagate a nested script failure so an enclosing `:run` (or the
            // `--script` launch) also fails fast.
            if !run_script(state, &file) {
                note_command_error();
            }
        }
        ReplCommand::Shell { args } => {
            run_shell(state, &args);
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
                let cypher_str = cypher.join(" ");
                match g.explain(&cypher_str) {
                    Ok(plan) => print!("{plan}"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Backup { file } => {
            if let Some(g) = &state.graph {
                match g.backup(&file) {
                    Ok(_) => eprintln!("backup written to {}", file.display()),
                    Err(e) => cli_eprintln!("backup failed: {e}"),
                }
            }
        }
        ReplCommand::BackupCompact { file } => {
            if let Some(g) = &state.graph {
                match g.backup_compact(&file) {
                    Ok(_) => eprintln!("compact backup written to {}", file.display()),
                    Err(e) => cli_eprintln!("backup failed: {e}"),
                }
            }
        }
        ReplCommand::Restore { snapshot, dst } => {
            // Restore materializes a fresh database directory; it does not touch
            // the currently open graph. Use `:open <dst>` afterward to switch to it.
            match Graph::restore(&snapshot, &dst) {
                Ok(()) => eprintln!(
                    "restored {} into {} (use `:open {}` to switch)",
                    snapshot.display(),
                    dst.display(),
                    dst.display()
                ),
                Err(e) => cli_eprintln!("restore failed: {e}"),
            }
        }
        ReplCommand::ImportNodes { file, label } => {
            cmd_import_nodes(state, &file, &label);
        }
        ReplCommand::ImportEdges {
            file,
            src_label,
            dst_label,
            etype,
        } => {
            cmd_import_edges(state, &file, &src_label, &dst_label, &etype);
        }
        ReplCommand::Query { cypher } => {
            let cypher_str = cypher.join(" ");
            run_cypher(state, &cypher_str);
        }
        ReplCommand::AddNode { label, props } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => cli_eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => {
                        // A colon-separated label string creates a multi-label node,
                        // matching the Cypher `(n:A:B)` convention.
                        let labels: Vec<&str> =
                            label.split(':').filter(|s| !s.is_empty()).collect();
                        let result = match labels.as_slice() {
                            [single] => g.add_node(single, &parsed_props),
                            _ => g.add_node_multi(&labels, &parsed_props),
                        };
                        match result {
                            Ok(id) => println!("{id}"),
                            Err(e) => cli_eprintln!("error: {e}"),
                        }
                    }
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
                    Ok(None) => eprintln!("node {id} not found"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::UpdateNode { id, props } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => cli_eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => match g.update_node(NodeId::from(id), &parsed_props) {
                        Ok(()) => println!("ok"),
                        Err(e) => cli_eprintln!("error: {e}"),
                    },
                }
            }
        }
        ReplCommand::DeleteNode { id } => {
            if let Some(g) = &state.graph {
                let node_id = NodeId::from(id);
                match g.get_node(node_id) {
                    Ok(Some(_)) => match g.delete_node(node_id) {
                        Ok(()) => println!("ok"),
                        Err(e) => cli_eprintln!("error: {e}"),
                    },
                    Ok(None) => eprintln!("node {id} not found"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::AddLabel { id, label } => {
            if let Some(g) = &state.graph {
                match g.add_label(NodeId::from(id), &label) {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::RemoveLabel { id, label } => {
            if let Some(g) = &state.graph {
                match g.remove_label(NodeId::from(id), &label) {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => {
                        match g.add_edge(
                            NodeId::from(src),
                            NodeId::from(dst),
                            &etype,
                            &parsed_props,
                        ) {
                            Ok(id) => println!("{id}"),
                            Err(e) => cli_eprintln!("error: {e}"),
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
                    Ok(None) => eprintln!("edge {id} not found"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::UpdateEdge { id, props } => {
            if let Some(g) = &state.graph {
                match parse_props(&props) {
                    Err(e) => cli_eprintln!("invalid props: {e}"),
                    Ok(parsed_props) => match g.update_edge(EdgeId::from(id), &parsed_props) {
                        Ok(()) => println!("ok"),
                        Err(e) => cli_eprintln!("error: {e}"),
                    },
                }
            }
        }
        ReplCommand::DeleteEdge { id } => {
            if let Some(g) = &state.graph {
                let edge_id = EdgeId::from(id);
                match g.get_edge(edge_id) {
                    Ok(Some(_)) => match g.delete_edge(edge_id) {
                        Ok(()) => println!("ok"),
                        Err(e) => cli_eprintln!("error: {e}"),
                    },
                    Ok(None) => eprintln!("edge {id} not found"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Out { id } => {
            if let Some(g) = &state.graph {
                match g.out_neighbors(NodeId::from(id)) {
                    Ok(v) => {
                        if v.is_empty() {
                            println!("no outgoing edges for node {id}");
                        }
                        for ne in v {
                            let etype = g
                                .type_name(ne.edge_type)
                                .ok()
                                .flatten()
                                .unwrap_or_else(|| ne.edge_type.to_string());
                            println!("  node={} edge={} type={etype}", ne.node, ne.edge);
                        }
                    }
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::In { id } => {
            if let Some(g) = &state.graph {
                match g.in_neighbors(NodeId::from(id)) {
                    Ok(v) => {
                        if v.is_empty() {
                            println!("no incoming edges for node {id}");
                        }
                        for ne in v {
                            let etype = g
                                .type_name(ne.edge_type)
                                .ok()
                                .flatten()
                                .unwrap_or_else(|| ne.edge_type.to_string());
                            println!("  node={} edge={} type={etype}", ne.node, ne.edge);
                        }
                    }
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Label { label } => {
            if let Some(g) = &state.graph {
                match g.nodes_by_label(&label) {
                    Ok(ids) => {
                        if ids.is_empty() {
                            println!("no nodes found for label \"{label}\"");
                        } else {
                            println!("{} node(s)", ids.len());
                            for id in &ids {
                                println!("  {id}");
                            }
                        }
                    }
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Etype { etype } => {
            if let Some(g) = &state.graph {
                match g.edges_by_type(&etype) {
                    Ok(ids) => {
                        if ids.is_empty() {
                            println!("no edges found for type \"{etype}\"");
                        } else {
                            println!("{} edge(s)", ids.len());
                            for id in &ids {
                                println!("  {id}");
                            }
                        }
                    }
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Stats => {
            if let Some(g) = &state.graph {
                match gather_stats(g, state.db_path.as_deref(), state.map_size_gb) {
                    Ok(stats) => print_stats(&stats),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }

        ReplCommand::RebuildCsr => {
            if let Some(g) = &state.graph {
                match g.rebuild_csr() {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::UpsertVec { id, values } => {
            if let Some(g) = &state.graph {
                match g.upsert_vector(NodeId::from(id), &values) {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::RemoveVec { id } => {
            if let Some(g) = &state.graph {
                match g.remove_vector(NodeId::from(id)) {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Vsearch {
            k,
            query,
            label,
            props,
            rescore_factor,
        } => {
            if let Some(g) = &state.graph {
                let properties = match props.as_deref().map(parse_prop_filters) {
                    None => None,
                    Some(Ok(map)) => Some(map),
                    Some(Err(e)) => {
                        cli_eprintln!("invalid props: {e}");
                        return true;
                    }
                };
                let opts = VectorSearchOptions {
                    k,
                    label,
                    properties,
                    rescore_factor,
                };
                match g.vector_search_with(&query, &opts) {
                    Ok(hits) => print_hits(&hits),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::Retrieve {
            k,
            hops,
            query,
            text,
            text_label,
            text_property,
            fusion,
        } => {
            if let Some(g) = &state.graph {
                let fusion_strategy = match fusion.as_str() {
                    "weighted_sum" => FusionStrategy::WeightedSum {
                        vector_weight: 0.5,
                        text_weight: 0.5,
                    },
                    _ => FusionStrategy::Rrf { k: 60 },
                };
                let text_query = text.clone().unwrap_or_default();
                let opts = HybridRetrieveOptions {
                    vector_k: k,
                    // Text seeds are only drawn when a text query is supplied.
                    text_k: if text.is_some() { k } else { 0 },
                    text_label,
                    text_property,
                    hops,
                    max_distance: f32::MAX,
                    max_nodes: None,
                    vector_label: None,
                    fusion: fusion_strategy,
                };
                match retrieve_hybrid(g, &query, &text_query, &opts) {
                    Ok(sub) => {
                        println!(
                            "{} node(s), {} edge(s), {} seed(s)",
                            sub.nodes.len(),
                            sub.edges.len(),
                            sub.scores.len()
                        );
                        let mut seeds: Vec<_> = sub.scores.iter().collect();
                        seeds.sort_unstable_by(|a, b| b.1.total_cmp(a.1));
                        for (n, score) in seeds {
                            println!("  seed node={n} score={score:.6}");
                        }
                    }
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::ConfigureVec {
            metric,
            quantization,
            reindex,
        } => {
            if let Some(g) = &state.graph {
                let opts = VectorIndexOptions {
                    metric: VectorMetric::from_str(&metric).unwrap_or_default(),
                    quantization: VectorQuantization::from_str(&quantization).unwrap_or_default(),
                };
                let res = if reindex {
                    g.reindex_vector_index(opts)
                } else {
                    g.configure_vector_index(opts)
                };
                match res {
                    Ok(()) => println!("ok"),
                    Err(e) => cli_eprintln!("error: {e}"),
                }
            }
        }
        ReplCommand::TextIndex {
            action,
            label,
            property,
            lang,
        } => {
            if let Some(g) = &state.graph {
                match action.as_str() {
                    "create" => {
                        let lbl = label.as_deref().unwrap_or("");
                        let prop = property.as_deref().unwrap_or("");
                        if lbl.is_empty() || prop.is_empty() {
                            cli_eprintln!("label and property are required for create");
                        } else {
                            let language = Language::from_str(&lang).unwrap_or_default();
                            match g.create_text_index_with_language(lbl, prop, language) {
                                Ok(()) => println!("ok"),
                                Err(e) => cli_eprintln!("error: {e}"),
                            }
                        }
                    }
                    "drop" => {
                        let lbl = label.as_deref().unwrap_or("");
                        let prop = property.as_deref().unwrap_or("");
                        if lbl.is_empty() || prop.is_empty() {
                            cli_eprintln!("label and property are required for drop");
                        } else {
                            match g.drop_text_index(lbl, prop) {
                                Ok(()) => println!("ok"),
                                Err(e) => cli_eprintln!("error: {e}"),
                            }
                        }
                    }
                    "has" => {
                        let lbl = label.as_deref().unwrap_or("");
                        let prop = property.as_deref().unwrap_or("");
                        if lbl.is_empty() || prop.is_empty() {
                            cli_eprintln!("label and property are required for has");
                        } else {
                            match g.has_text_index(lbl, prop) {
                                Ok(exists) => println!("{}", exists),
                                Err(e) => cli_eprintln!("error: {e}"),
                            }
                        }
                    }
                    "list" => match g.list_text_indexes() {
                        Ok(idxs) => {
                            if idxs.is_empty() {
                                println!("(no text indexes)");
                            } else {
                                for (label, prop, lang) in &idxs {
                                    println!("  {label}.{prop} (language: {lang:?})");
                                }
                            }
                        }
                        Err(e) => cli_eprintln!("error: {e}"),
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
                    Err(e) => cli_eprintln!("{}", format!("error: {e}").red()),
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Script execution
// ---------------------------------------------------------------------------

/// Execute a script file line by line, stopping at the first failing command.
/// Returns `true` if the whole script ran without an error, `false` otherwise.
fn run_script(state: &mut State, path: &str) -> bool {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            cli_eprintln!("cannot read {path}: {e}");
            return false;
        }
    };

    // Mark the script scope so a `:!` shell escape inside the file is rejected.
    // Save and restore the flag so a nested `:run` does not clear it early.
    let was_in_script = state.in_script;
    state.in_script = true;
    let mut ok = true;
    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with("--") {
            continue;
        }
        println!("[{}:{}] {line}", path, lineno + 1);
        // Clear the per-command flag, run the line, then check whether it
        // reported an error. The first failing command stops the script.
        let _ = take_command_error();
        let cont = handle(state, line);
        if take_command_error() {
            cli_eprintln!(
                "script aborted at {}:{} after a failing command",
                path,
                lineno + 1
            );
            ok = false;
            break;
        }
        if !cont {
            break;
        }
    }
    state.in_script = was_in_script;
    ok
}

// ---------------------------------------------------------------------------
// Shell escape
// ---------------------------------------------------------------------------

fn run_shell(state: &State, args: &[String]) {
    // A checked-in script must not silently shell out on someone else's machine,
    // so the escape is interactive-only.
    if state.in_script {
        cli_eprintln!(
            "{}",
            "the :! shell escape is disabled inside :run scripts".red()
        );
        return;
    }
    if args.is_empty() {
        cli_eprintln!("usage: :! <command> [args...]");
        return;
    }
    // Run through the user's shell so pipes, globs, and quoting behave as typed.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let command = args.join(" ");
    match std::process::Command::new(&shell)
        .arg("-c")
        .arg(&command)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => match status.code() {
            Some(code) => cli_eprintln!(
                "{}",
                format!("shell command exited with status {code}").red()
            ),
            None => cli_eprintln!("{}", "shell command terminated by signal".red()),
        },
        Err(e) => cli_eprintln!("{}", format!("failed to run shell command: {e}").red()),
    }
}

// ---------------------------------------------------------------------------
// Cypher execution
// ---------------------------------------------------------------------------

fn run_explain(state: &mut State, cypher: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            cli_eprintln!("no database open");
            return;
        }
    };
    match g.explain(cypher) {
        Ok(plan) => print!("{plan}"),
        Err(e) => cli_eprintln!("error: {e}"),
    }
}

fn run_cypher(state: &mut State, cypher: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            cli_eprintln!("no database open");
            return;
        }
    };

    let save_path = state.save_path.take();
    let result = if state.params.is_empty() {
        g.query(cypher)
    } else {
        g.query_with_params(cypher, &state.params)
    };

    match result {
        Err(e) => cli_eprintln!("{}", format!("error: {e}").red()),
        Ok(qr) => {
            let output = format_query_result(&qr);
            if let Some(ref save) = save_path {
                match fs::File::create(save) {
                    Ok(mut f) => {
                        if let Err(e) = f.write_all(output.as_bytes()) {
                            cli_eprintln!("write error: {e}");
                        } else {
                            eprintln!("saved to {}", save.display());
                        }
                    }
                    Err(e) => cli_eprintln!("cannot create {}: {e}", save.display()),
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
// Database statistics
// ---------------------------------------------------------------------------

/// A snapshot of database and graph metadata gathered for the `stats` command.
struct GraphStats {
    /// Exact live node count (a full node scan).
    node_count: usize,
    /// Exact live edge count, summed over every relationship type.
    edge_count: u64,
    /// `(label, live node count)` for every label in the registry, including
    /// labels with zero live nodes.
    labels: Vec<(String, u64)>,
    /// `(relationship type, live edge count)` for every type in the registry.
    rel_types: Vec<(String, u64)>,
    /// `(label, property, flags)` node indexes and constraints.
    node_indexes: Vec<(String, String, u8)>,
    /// `(type, property, flags)` edge indexes and constraints.
    edge_indexes: Vec<(String, String, u8)>,
    /// `(label, property, language)` full-text indexes.
    text_indexes: Vec<(String, String, String)>,
    /// Number of persisted vector embeddings.
    vector_count: usize,
    /// Total size of the LMDB files on disk, when the path is known.
    on_disk_bytes: Option<u64>,
    /// Configured LMDB map size in gigabytes.
    map_size_gb: usize,
}

/// Gathers database and graph statistics through the public facade.
///
/// The label and type registries allocate dense, monotonic `u32` ids from 0 and
/// never reuse them, so probing ids upward until the first unallocated id
/// enumerates every label and type ever created. This is the only registry
/// enumeration the facade exposes.
fn gather_stats(
    g: &Graph,
    db_path: Option<&Path>,
    map_size_gb: usize,
) -> Result<GraphStats, issundb::Error> {
    let node_count = g.all_nodes()?.len();

    let mut labels = Vec::new();
    let mut label_id: u32 = 0;
    while let Some(name) = g.label_name(label_id)? {
        let count = g.node_count_by_label(&name)?;
        labels.push((name, count));
        label_id += 1;
    }

    let mut rel_types = Vec::new();
    let mut edge_count: u64 = 0;
    let mut type_id: u32 = 0;
    while let Some(name) = g.type_name(type_id)? {
        let count = g.edge_count_by_type(&name)?;
        edge_count += count;
        rel_types.push((name, count));
        type_id += 1;
    }

    let node_indexes = g.list_node_indexes_and_constraints()?;
    let edge_indexes = g.list_edge_indexes_and_constraints()?;

    // The text-index listing has its own error type; treat a failure as "none"
    // rather than aborting the whole report.
    let text_indexes = g
        .list_text_indexes()
        .map(|v| {
            v.into_iter()
                .map(|(label, prop, lang)| (label, prop, format!("{lang:?}").to_lowercase()))
                .collect()
        })
        .unwrap_or_default();

    let vector_count = g.vector_bytes()?.len();
    let on_disk_bytes = db_path.and_then(dir_size);

    Ok(GraphStats {
        node_count,
        edge_count,
        labels,
        rel_types,
        node_indexes,
        edge_indexes,
        text_indexes,
        vector_count,
        on_disk_bytes,
        map_size_gb,
    })
}

/// Names the index kind for a node or edge index flag byte: `0x01` is a unique
/// constraint, `0x02` a required (existence) constraint, anything else a plain
/// property index.
fn index_kind(flags: u8) -> &'static str {
    match flags {
        0x01 => "unique",
        0x02 => "required",
        _ => "index",
    }
}

/// Sum of the file sizes directly inside an LMDB directory (`data.mdb` and
/// `lock.mdb`). Returns `None` when the directory cannot be read. Not recursive:
/// an LMDB environment keeps its files flat in the directory.
fn dir_size(dir: &Path) -> Option<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    Some(total)
}

/// Formats a byte count with a binary unit suffix (KiB, MiB, ...).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Prints the gathered statistics as aligned, sectioned tables.
fn print_stats(s: &GraphStats) {
    let kv = |key: &str, val: &str| println!("  {key:<20}{val}");

    println!("{}", "Database".cyan().bold());
    if let Some(bytes) = s.on_disk_bytes {
        kv("on-disk size", &human_bytes(bytes));
    }
    kv("map size", &format!("{} GiB", s.map_size_gb));

    println!("{}", "Graph".cyan().bold());
    kv("nodes", &s.node_count.to_string());
    kv("edges", &s.edge_count.to_string());
    kv("labels", &s.labels.len().to_string());
    kv("relationship types", &s.rel_types.len().to_string());
    kv("vectors", &s.vector_count.to_string());

    if !s.labels.is_empty() {
        println!("{}", "Labels".cyan().bold());
        print_named_counts(&s.labels);
    }
    if !s.rel_types.is_empty() {
        println!("{}", "Relationship Types".cyan().bold());
        print_named_counts(&s.rel_types);
    }

    println!("{}", "Indexes & Constraints".cyan().bold());
    if s.node_indexes.is_empty() && s.edge_indexes.is_empty() {
        println!("  (none)");
    } else {
        for (label, prop, flags) in &s.node_indexes {
            println!(
                "  node  {:<28}{}",
                format!("{label}.{prop}"),
                index_kind(*flags)
            );
        }
        for (etype, prop, flags) in &s.edge_indexes {
            println!(
                "  edge  {:<28}{}",
                format!("{etype}.{prop}"),
                index_kind(*flags)
            );
        }
    }

    if !s.text_indexes.is_empty() {
        println!("{}", "Text Indexes".cyan().bold());
        for (label, prop, lang) in &s.text_indexes {
            println!("  {:<28}{}", format!("{label}.{prop}"), lang);
        }
    }
}

/// Prints `(name, count)` rows with the name column padded to the widest name.
fn print_named_counts(rows: &[(String, u64)]) {
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(8) + 2;
    for (name, count) in rows {
        println!("  {name:<width$}{count}");
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

/// A REPL command whose argument is a raw Cypher body that must reach the
/// engine verbatim, with its string-literal quotes intact.
enum RawCypher<'a> {
    Query(&'a str),
    Explain(&'a str),
}

/// Classify a line that carries a raw Cypher body (`query`, its `cypher` alias,
/// or `:explain`) and return the verbatim body. Returns `None` for any other
/// command, which is then tokenized and parsed by clap as before.
fn classify_raw_cypher(line: &str) -> Option<RawCypher<'_>> {
    let (cmd, rest) = split_cmd(line);
    match cmd {
        "query" | "cypher" => Some(RawCypher::Query(rest)),
        ":explain" => Some(RawCypher::Explain(rest)),
        _ => None,
    }
}

fn tokenize_line(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '\0';
    let mut brace_depth: usize = 0;
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            current.push(c);
            escaped = true;
        } else if in_quotes {
            if c == quote_char {
                in_quotes = false;
                if brace_depth > 0 {
                    current.push(c);
                }
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quotes = true;
            quote_char = c;
            if brace_depth > 0 {
                current.push(c);
            }
        } else if c == '{' {
            brace_depth += 1;
            current.push(c);
        } else if c == '}' {
            brace_depth = brace_depth.saturating_sub(1);
            current.push(c);
        } else if c.is_whitespace() && brace_depth == 0 {
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

/// Parse a JSON object string into property key-value filters for vector search.
fn parse_prop_filters(s: &str) -> Result<HashMap<String, serde_json::Value>, String> {
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

fn parse_csv_line(s: &str) -> Vec<String> {
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
        } else if c == ',' && !in_quotes {
            cols.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    cols.push(current.trim().to_owned());
    cols
}

/// Infer a JSON value from a raw CSV cell. Empty cells become null, integers and
/// floats become numbers, `true`/`false` become booleans, and anything else
/// stays a string. The `Id` column relies on this so integer keys index as
/// integers and name keys index as strings.
fn csv_cell_to_value(val_str: &str) -> serde_json::Value {
    if val_str.is_empty() {
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
    }
}

/// Whether `path` should be read as Parquet, decided by a case-insensitive
/// `.parquet` file extension. Any other extension is treated as CSV.
fn is_parquet_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("parquet"))
        .unwrap_or(false)
}

/// Bulk-import nodes from a CSV or Parquet file whose columns become node
/// properties.
///
/// A `.parquet` extension selects the Parquet reader, whose columns become
/// properties typed by their Arrow schema; any other extension is read as CSV
/// where the first non-empty line is the header that supplies the property
/// names and CSV cell values are typed by [`csv_cell_to_value`]. Either way each
/// row is one node carrying `label`, and nodes are inserted in fixed-size
/// batches, each its own write transaction, mirroring `:import-edges` so memory
/// stays bounded regardless of file size. The label is a command argument, so
/// neither format carries a leading label column.
fn cmd_import_nodes(state: &mut State, path: &str, label: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            cli_eprintln!("no database open");
            return;
        }
    };

    let entries: Vec<serde_json::Value> = if is_parquet_path(path) {
        match read_parquet_entries(path) {
            Ok(maps) => maps.into_iter().map(serde_json::Value::Object).collect(),
            Err(e) => {
                cli_eprintln!("cannot read {path}: {e}");
                return;
            }
        }
    } else {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                cli_eprintln!("cannot read {path}: {e}");
                return;
            }
        };

        let mut lines = content.lines().filter(|l| !l.trim().is_empty());
        let header_line = match lines.next() {
            Some(h) => h,
            None => {
                cli_eprintln!("CSV file is empty");
                return;
            }
        };

        let headers = parse_csv_line(header_line);
        if headers.is_empty() {
            cli_eprintln!("CSV has no columns");
            return;
        }

        // Every column is a property; the label comes from the command argument.
        let mut entries: Vec<serde_json::Value> = Vec::new();
        for line in lines {
            let cols = parse_csv_line(line);
            let mut props = serde_json::Map::new();
            for (j, header) in headers.iter().enumerate() {
                let val_str = cols.get(j).map(|s| s.as_str()).unwrap_or("");
                props.insert(header.to_owned(), csv_cell_to_value(val_str));
            }
            entries.push(serde_json::Value::Object(props));
        }
        entries
    };

    const BATCH: usize = 50_000;
    let total = entries.len();
    let mut inserted: u64 = 0;

    for chunk in entries.chunks(BATCH) {
        match g.update(|txn| {
            for props in chunk {
                txn.add_node(label, props)?;
            }
            Ok(())
        }) {
            Ok(_) => inserted += chunk.len() as u64,
            Err(e) => {
                cli_eprintln!("node batch insert failed: {e}");
                return;
            }
        }
    }

    eprintln!("imported {inserted}/{total} {label} nodes from {path}");
}

/// Resolve a domain key to a node id via the always-on scalar auto-index on
/// `Id`. The key is matched as an integer when it parses as one, otherwise as a
/// string. This mirrors how `:import-nodes` stores values, so both integer-keyed
/// and string-keyed nodes resolve. A unique `Id` yields exactly one match; the
/// first is taken if several exist. Returns `None` when no node carries that
/// `(label, Id)`.
fn resolve_node_by_id(
    txn: &issundb::ReadTxn,
    label: &str,
    key: &str,
) -> Result<Option<NodeId>, issundb::Error> {
    let val = match key.parse::<i64>() {
        Ok(i) => PropValue::Int(i),
        Err(_) => PropValue::Str(key.to_owned()),
    };
    Ok(txn.nodes_by_property(label, "Id", val)?.into_iter().next())
}

/// Bulk-import edges from a two-column CSV or Parquet file of source and
/// destination domain keys.
///
/// Each data row is `src_key, dst_key`: for CSV the first two columns, for
/// Parquet the first two columns by position. Both keys are resolved to node ids
/// by their auto-indexed `Id` property; `src_label` and `dst_label` scope the
/// lookup so keys that collide across labels stay distinct. Edges of type
/// `etype` are inserted in fixed-size batches, each its own write transaction.
/// This bypasses the Cypher planner entirely (no `UNWIND`, `LabelScan`, or
/// `HashJoin`) and keeps memory bounded regardless of file size.
fn cmd_import_edges(state: &mut State, path: &str, src_label: &str, dst_label: &str, etype: &str) {
    let g = match state.graph.as_ref() {
        Some(g) => g,
        None => {
            cli_eprintln!("no database open");
            return;
        }
    };

    // Collect the two key columns up front as raw strings (resolution decides
    // int-vs-string per key). A row missing either non-empty key is malformed.
    let (pairs, malformed): (Vec<(String, String)>, u64) = if is_parquet_path(path) {
        match read_parquet_edge_pairs(path) {
            Ok(r) => r,
            Err(e) => {
                cli_eprintln!("cannot read {path}: {e}");
                return;
            }
        }
    } else {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                cli_eprintln!("cannot read {path}: {e}");
                return;
            }
        };

        // First non-empty line is the header (e.g. `src_id,dst_id`).
        let mut lines = content.lines().filter(|l| !l.trim().is_empty());
        if lines.next().is_none() {
            cli_eprintln!("CSV file is empty");
            return;
        }

        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut malformed: u64 = 0;
        for line in lines {
            let cols = parse_csv_line(line);
            let src = cols.first().map(|s| s.trim()).filter(|s| !s.is_empty());
            let dst = cols.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());
            match (src, dst) {
                (Some(s), Some(d)) => pairs.push((s.to_owned(), d.to_owned())),
                _ => malformed += 1,
            }
        }
        (pairs, malformed)
    };

    const BATCH: usize = 50_000;
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let mut inserted: u64 = 0;
    let mut unresolved: u64 = 0;

    for chunk in pairs.chunks(BATCH) {
        // One read view resolves the whole chunk, then one write txn inserts it.
        let resolved: Vec<(NodeId, NodeId)> = match g.view(|txn| {
            let mut out = Vec::with_capacity(chunk.len());
            for (src_key, dst_key) in chunk {
                if let (Some(s), Some(d)) = (
                    resolve_node_by_id(txn, src_label, src_key)?,
                    resolve_node_by_id(txn, dst_label, dst_key)?,
                ) {
                    out.push((s, d));
                }
            }
            Ok(out)
        }) {
            Ok(v) => v,
            Err(e) => {
                cli_eprintln!("edge resolution failed: {e}");
                return;
            }
        };
        unresolved += (chunk.len() - resolved.len()) as u64;

        match g.update(|txn| {
            for (s, d) in &resolved {
                txn.add_edge(*s, *d, etype, &empty)?;
            }
            Ok(())
        }) {
            Ok(_) => inserted += resolved.len() as u64,
            Err(e) => {
                cli_eprintln!("edge batch insert failed: {e}");
                return;
            }
        }
    }

    eprintln!(
        "imported {inserted} {etype} edges from {path} ({unresolved} unresolved endpoint(s), {malformed} malformed row(s))"
    );
}

/// Read every row of a Parquet file into one property map per row, each column
/// becoming a property keyed by its name. The Arrow value to JSON mapping
/// mirrors the engine's `COPY ... FROM '*.parquet'` path so Parquet imports and
/// the `COPY` statement agree on typing. A `props` column holding a nested
/// object is flattened into the row, matching `COPY` so an exported file
/// round-trips.
fn read_parquet_entries(
    path: &str,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("not a valid Parquet file: {e}"))?;
    let reader = builder
        .build()
        .map_err(|e| format!("failed to read Parquet file: {e}"))?;

    let mut entries = Vec::new();
    for batch_res in reader {
        let batch = batch_res.map_err(|e| format!("failed to read record batch: {e}"))?;
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            let mut obj = serde_json::Map::new();
            for col in 0..batch.num_columns() {
                let name = schema.field(col).name();
                let val = arrow_to_json_value(batch.column(col), row)?;
                obj.insert(name.clone(), val);
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

/// Read source and destination domain keys from the first two columns of a
/// Parquet file, mirroring the positional two-column convention of the CSV edge
/// import. Returns the resolvable `(src_key, dst_key)` string pairs plus the
/// count of rows missing either key. Values are stringified by
/// [`value_to_key_string`] so `resolve_node_by_id` can match them as integers or
/// strings.
fn read_parquet_edge_pairs(path: &str) -> Result<(Vec<(String, String)>, u64), String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("not a valid Parquet file: {e}"))?;
    let reader = builder
        .build()
        .map_err(|e| format!("failed to read Parquet file: {e}"))?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut malformed: u64 = 0;
    for batch_res in reader {
        let batch = batch_res.map_err(|e| format!("failed to read record batch: {e}"))?;
        if batch.num_columns() < 2 {
            return Err(
                "Parquet file must have at least two columns (src_key, dst_key)".to_owned(),
            );
        }
        let src_col = batch.column(0);
        let dst_col = batch.column(1);
        for row in 0..batch.num_rows() {
            let src = value_to_key_string(&arrow_to_json_value(src_col, row)?);
            let dst = value_to_key_string(&arrow_to_json_value(dst_col, row)?);
            match (src, dst) {
                (Some(s), Some(d)) => pairs.push((s, d)),
                _ => malformed += 1,
            }
        }
    }
    Ok((pairs, malformed))
}

/// Convert a JSON value into a domain-key string for edge endpoint resolution,
/// or `None` when the value cannot be a key. A number is rendered without a
/// fractional part when it is integral so an `Id` stored as an integer matches.
/// A blank or whitespace-only string is treated as absent.
fn value_to_key_string(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Convert one cell of an Arrow array to a JSON value, mirroring the engine's
/// `COPY` reader so Parquet imports type values the same way. A string that
/// looks like a JSON array or object is parsed; an unsupported Arrow type maps
/// to null.
fn arrow_to_json_value(
    array: &arrow_array::ArrayRef,
    row: usize,
) -> Result<serde_json::Value, String> {
    use arrow_array::cast::AsArray;
    use arrow_schema::DataType;
    use serde_json::Value;

    if array.is_null(row) {
        return Ok(Value::Null);
    }

    let parse_strish = |s: &str| -> Value {
        if (s.starts_with('[') && s.ends_with(']')) || (s.starts_with('{') && s.ends_with('}')) {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_owned()))
        } else {
            Value::String(s.to_owned())
        }
    };

    match array.data_type() {
        DataType::Boolean => Ok(Value::Bool(array.as_boolean().value(row))),
        DataType::Int8 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::Int8Type>()
                .value(row)
                .into(),
        )),
        DataType::Int16 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::Int16Type>()
                .value(row)
                .into(),
        )),
        DataType::Int32 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::Int32Type>()
                .value(row)
                .into(),
        )),
        DataType::Int64 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::Int64Type>()
                .value(row)
                .into(),
        )),
        DataType::UInt8 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::UInt8Type>()
                .value(row)
                .into(),
        )),
        DataType::UInt16 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::UInt16Type>()
                .value(row)
                .into(),
        )),
        DataType::UInt32 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::UInt32Type>()
                .value(row)
                .into(),
        )),
        DataType::UInt64 => Ok(Value::Number(
            array
                .as_primitive::<arrow_array::types::UInt64Type>()
                .value(row)
                .into(),
        )),
        DataType::Float32 => {
            let v = array
                .as_primitive::<arrow_array::types::Float32Type>()
                .value(row);
            Ok(serde_json::Number::from_f64(v as f64).map_or(Value::Null, Value::Number))
        }
        DataType::Float64 => {
            let v = array
                .as_primitive::<arrow_array::types::Float64Type>()
                .value(row);
            Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
        }
        DataType::Utf8 => Ok(parse_strish(array.as_string::<i32>().value(row))),
        DataType::LargeUtf8 => Ok(parse_strish(array.as_string::<i64>().value(row))),
        DataType::List(_) => {
            let value_arr = array.as_list::<i32>().value(row);
            let mut vals = Vec::with_capacity(value_arr.len());
            for i in 0..value_arr.len() {
                vals.push(arrow_to_json_value(&value_arr, i)?);
            }
            Ok(Value::Array(vals))
        }
        DataType::LargeList(_) => {
            let value_arr = array.as_list::<i64>().value(row);
            let mut vals = Vec::with_capacity(value_arr.len());
            for i in 0..value_arr.len() {
                vals.push(arrow_to_json_value(&value_arr, i)?);
            }
            Ok(Value::Array(vals))
        }
        _ => Ok(Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Run the completer over `line` with the cursor at the end and return the
    /// word-start offset plus the replacement strings.
    fn complete_at_end(helper: &ReplHelper, line: &str) -> (usize, Vec<String>) {
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&hist);
        let (start, pairs) = helper.complete(line, line.len(), &ctx).expect("completion");
        (start, pairs.into_iter().map(|p| p.replacement).collect())
    }

    #[test]
    fn completer_completes_command_names_and_cypher_keywords() {
        let helper = ReplHelper::new();

        // Colon meta-command prefix.
        let (start, cands) = complete_at_end(&helper, ":ru");
        assert_eq!(start, 0);
        assert!(cands.contains(&":run".to_string()));

        // Bare data command.
        let (_, cands) = complete_at_end(&helper, "add-n");
        assert!(cands.contains(&"add-node".to_string()));

        // Cypher keyword, matched case-insensitively but offered uppercase.
        let (_, cands) = complete_at_end(&helper, "ma");
        assert!(cands.contains(&"MATCH".to_string()));

        // help and quit are real Clap subcommands, so they complete too.
        let (_, cands) = complete_at_end(&helper, "qu");
        assert!(cands.contains(&"quit".to_string()));
    }

    #[test]
    fn completer_offset_skips_leading_whitespace() {
        let helper = ReplHelper::new();
        let (start, cands) = complete_at_end(&helper, "   :op");
        assert_eq!(start, 3);
        assert!(cands.contains(&":open".to_string()));
    }

    #[test]
    fn completer_does_not_offer_commands_in_argument_position() {
        let helper = ReplHelper::new();
        // A non-path command's argument gets no command-name candidates.
        let (_, cands) = complete_at_end(&helper, "get-node 1");
        assert!(cands.is_empty());
    }

    #[test]
    fn completer_path_argument_completes_filenames() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("nodes.csv"), "Id\n").unwrap();
        let helper = ReplHelper::new();
        // Position 1 of :import-nodes is the CSV path, so filenames complete.
        let line = format!(":import-nodes {}/no", dir.path().display());
        let (_, cands) = complete_at_end(&helper, &line);
        assert!(
            cands.iter().any(|c| c.contains("nodes.csv")),
            "expected a filename candidate, got {cands:?}"
        );
    }

    #[test]
    fn completer_label_argument_is_not_path_completed() {
        let helper = ReplHelper::new();
        // Position 2 of :import-nodes is the label, not a path, so the filesystem
        // is not offered there.
        let (_, cands) = complete_at_end(&helper, ":import-nodes people.csv Per");
        assert!(
            cands.is_empty(),
            "label argument should not complete to files"
        );

        // The relationship-type and label arguments of :import-edges (positions
        // 2 through 4) are likewise not path-completed.
        let (_, cands) = complete_at_end(&helper, ":import-edges e.csv Person Person KNO");
        assert!(cands.is_empty());
    }

    #[test]
    fn completer_keyword_list_matches_dispatcher() {
        // The completer and the Cypher shorthand in `handle` share CYPHER_KEYWORDS,
        // so every keyword the dispatcher routes is also a completion candidate.
        let helper = ReplHelper::new();
        for kw in CYPHER_KEYWORDS {
            assert!(
                helper.commands.iter().any(|c| c == kw),
                "{kw} missing from completion candidates"
            );
        }
    }

    #[test]
    fn stats_gather_reports_counts_labels_types_indexes_and_vectors() {
        use serde_json::json;
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();

        let a = g.add_node("Person", &json!({"name": "Alice"})).unwrap();
        // A multi-label node: counted under both Person and Admin per the label
        // scans, but only once in the exact total.
        let b = g
            .add_node_multi(&["Person", "Admin"], &json!({"name": "Bob"}))
            .unwrap();
        let c = g.add_node("City", &json!({"name": "Paris"})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "LIVES_IN", &json!({})).unwrap();
        g.create_node_unique_constraint("Person", "email").unwrap();
        g.upsert_vector(a, &[0.1, 0.2, 0.3]).unwrap();

        let stats = gather_stats(&g, Some(dir.path()), 1).unwrap();

        // Exact totals, independent of multi-labeling (sum of label counts is 4).
        assert_eq!(stats.node_count, 3);
        assert_eq!(stats.edge_count, 2);

        // Per-label breakdown: Bob counts under both Person and Admin.
        assert!(stats.labels.iter().any(|(n, c)| n == "Person" && *c == 2));
        assert!(stats.labels.iter().any(|(n, c)| n == "Admin" && *c == 1));
        assert!(stats.labels.iter().any(|(n, c)| n == "City" && *c == 1));

        // Per-type edge breakdown.
        assert!(stats.rel_types.iter().any(|(n, c)| n == "KNOWS" && *c == 1));
        assert!(
            stats
                .rel_types
                .iter()
                .any(|(n, c)| n == "LIVES_IN" && *c == 1)
        );

        // The unique constraint surfaces with the unique flag (0x01).
        assert!(
            stats
                .node_indexes
                .iter()
                .any(|(l, p, f)| l == "Person" && p == "email" && *f == 0x01)
        );

        assert_eq!(stats.vector_count, 1);
        assert!(stats.on_disk_bytes.unwrap_or(0) > 0);
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(index_kind(0x01), "unique");
        assert_eq!(index_kind(0x02), "required");
        assert_eq!(index_kind(0x00), "index");
    }

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
    fn classify_raw_cypher_preserves_quoted_literals() {
        // A single-quoted string literal must survive verbatim, where
        // tokenize_line would have stripped the quotes into a bare variable.
        match classify_raw_cypher("query RETURN 'hello world' AS x") {
            Some(RawCypher::Query(body)) => assert_eq!(body, "RETURN 'hello world' AS x"),
            _ => panic!("expected a Query body"),
        }
        // The `cypher` alias behaves identically, including double quotes.
        match classify_raw_cypher("cypher MATCH (n) WHERE n.p = \"a b\" RETURN n") {
            Some(RawCypher::Query(body)) => {
                assert_eq!(body, "MATCH (n) WHERE n.p = \"a b\" RETURN n")
            }
            _ => panic!("expected a Query body"),
        }
        // `:explain` carries the same raw body, quotes intact.
        match classify_raw_cypher(":explain MATCH (n) WHERE n.p CONTAINS 'z' RETURN n") {
            Some(RawCypher::Explain(body)) => {
                assert_eq!(body, "MATCH (n) WHERE n.p CONTAINS 'z' RETURN n")
            }
            _ => panic!("expected an Explain body"),
        }
        // Any other command is left for clap to tokenize and parse.
        assert!(classify_raw_cypher("add-node Person {\"name\": \"Alice\"}").is_none());
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
        assert_eq!(
            tokenize_line("add-node Person {\"name\": \"Alice {bracket}\"}"),
            vec![
                "add-node".to_owned(),
                "Person".to_owned(),
                "{\"name\": \"Alice {bracket}\"}".to_owned()
            ]
        );
        assert_eq!(
            tokenize_line("text-search \"hello \\\"world\\\"\" Person"),
            vec![
                "text-search".to_owned(),
                "hello \\\"world\\\"".to_owned(),
                "Person".to_owned()
            ]
        );
    }

    #[test]
    fn test_parse_csv_line() {
        assert_eq!(
            parse_csv_line("label,name,age"),
            vec!["label".to_owned(), "name".to_owned(), "age".to_owned()]
        );
        assert_eq!(
            parse_csv_line("Person,\"Alice, Smith\",30"),
            vec![
                "Person".to_owned(),
                "Alice, Smith".to_owned(),
                "30".to_owned()
            ]
        );
        assert_eq!(
            parse_csv_line("Person,\"Alice \"\"The Great\"\" Smith\",30"),
            vec![
                "Person".to_owned(),
                "Alice \"The Great\" Smith".to_owned(),
                "30".to_owned()
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
        let mut state = State::new(None, None, 1);

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

        // 4a. Export and Import database via Cypher queries
        let export_path = temp.path().join("cli_export");
        let export_cmd = format!(
            "EXPORT DATABASE '{}' WITH {{format: 'parquet'}}",
            export_path.display()
        );
        assert!(handle(&mut state, &export_cmd));
        assert!(export_path.exists());
        assert!(export_path.join("nodes.parquet").exists());
        assert!(export_path.join("edges.parquet").exists());

        let import_cmd = format!("IMPORT DATABASE '{}'", export_path.display());
        assert!(handle(&mut state, &import_cmd));

        // 4b. Configure vector index
        assert!(handle(&mut state, "configure-vec l2 float16"));

        // 4c. Create FTS index with custom language, then list it
        assert!(handle(
            &mut state,
            "text-index create Person name --lang german"
        ));
        assert!(handle(&mut state, "text-index list"));

        // 4d. Multi-label node creation via colon-separated labels.
        assert!(handle(
            &mut state,
            "add-node Person:Admin {\"name\": \"Bob\"}"
        ));
        // Both labels must be queryable.
        assert!(handle(&mut state, "MATCH (n:Admin) RETURN n.name"));

        // 4e. Vector upsert, then filtered nearest-neighbor search.
        assert!(handle(&mut state, "upsert-vec 1 0.1 0.2 0.3"));
        assert!(handle(&mut state, "vsearch 5 0.1 0.2 0.3 --label Person"));
        assert!(handle(
            &mut state,
            "vsearch 5 0.1 0.2 0.3 --props {\"name\":\"Alice\"}"
        ));

        // 4f. Hybrid retrieval with text seeds.
        assert!(handle(
            &mut state,
            "retrieve 5 2 0.1 0.2 0.3 --text Bob --text-label Person --text-prop name"
        ));

        // 4g. Backup then restore into a fresh directory.
        let snap = temp.path().join("snap.db");
        assert!(handle(&mut state, &format!(":backup {}", snap.display())));
        let restored = temp.path().join("restored");
        assert!(handle(
            &mut state,
            &format!(":restore {} {}", snap.display(), restored.display())
        ));
        assert!(restored.exists());

        // 4h. Close releases the open database without exiting; a second close
        // is a no-op against the now-empty state.
        assert!(state.graph.is_some());
        assert!(handle(&mut state, ":close"));
        assert!(state.graph.is_none());
        assert!(handle(&mut state, ":close"));

        // 5. Quit command should return false
        assert!(!handle(&mut state, "quit"));
    }

    #[test]
    fn test_import_edges_resolves_domain_keys() {
        let temp = TempDir::new().unwrap();
        let mut state = State::new(None, None, 1);
        assert!(handle(
            &mut state,
            &format!(":open {}", temp.path().display())
        ));

        // Nodes keyed by a domain `Id` that differs from their internal id.
        assert!(handle(&mut state, "add-node User {\"Id\": 10}"));
        assert!(handle(&mut state, "add-node User {\"Id\": 20}"));
        assert!(handle(&mut state, "add-node Kernel {\"Id\": 100}"));
        assert!(handle(&mut state, "add-node Kernel {\"Id\": 200}"));

        // CSV of domain keys: two valid rows plus one with an unresolvable dst.
        let csv = temp.path().join("edges.csv");
        std::fs::write(&csv, "from_user_id,to_kernel_id\n10,100\n20,200\n10,999\n").unwrap();

        assert!(handle(
            &mut state,
            &format!(
                ":import-edges {} User Kernel AUTHORED_KERNEL",
                csv.display()
            )
        ));

        // Exactly the two resolvable edges should exist; the 999 dst is dropped.
        let g = state.graph.as_ref().unwrap();
        let count = g
            .view(|txn| txn.edge_count_by_type("AUTHORED_KERNEL"))
            .unwrap();
        assert_eq!(count, 2);

        // String-keyed endpoints (e.g. Library nodes keyed by name) resolve too.
        assert!(handle(&mut state, "add-node Library {\"Id\": \"numpy\"}"));
        let lib_csv = temp.path().join("imports.csv");
        std::fs::write(
            &lib_csv,
            "from_kernel_version_id,to_library_id\n100,numpy\n200,pandas\n",
        )
        .unwrap();
        assert!(handle(
            &mut state,
            &format!(":import-edges {} Kernel Library IMPORTS", lib_csv.display())
        ));
        let g = state.graph.as_ref().unwrap();
        let imports = g.view(|txn| txn.edge_count_by_type("IMPORTS")).unwrap();
        // Only (Kernel 100 -> Library "numpy") resolves; "pandas" has no node.
        assert_eq!(imports, 1);
    }

    #[test]
    fn test_import_nodes_loads_csv_columns_as_properties() {
        let temp = TempDir::new().unwrap();
        let mut state = State::new(None, None, 1);
        assert!(handle(
            &mut state,
            &format!(":open {}", temp.path().display())
        ));

        // Header columns become properties; the label is the command argument,
        // so there is no leading label column.
        let csv = temp.path().join("users.csv");
        std::fs::write(&csv, "Id,name\n10,Alice\n20,Bob\n").unwrap();
        assert!(handle(
            &mut state,
            &format!(":import-nodes {} User", csv.display())
        ));

        let g = state.graph.as_ref().unwrap();
        let users = g.view(|txn| txn.nodes_by_label("User")).unwrap();
        assert_eq!(users.len(), 2);

        // Imported nodes resolve through the same auto-index the edge importer
        // uses, with the integer `Id` typed as an integer.
        let by_id = g
            .view(|txn| txn.nodes_by_property("User", "Id", PropValue::Int(10)))
            .unwrap();
        assert_eq!(by_id.len(), 1);

        // String-keyed nodes (e.g. Library) type their `Id` as a string.
        let lib_csv = temp.path().join("libs.csv");
        std::fs::write(&lib_csv, "Id\nnumpy\npandas\n").unwrap();
        assert!(handle(
            &mut state,
            &format!(":import-nodes {} Library", lib_csv.display())
        ));
        let g = state.graph.as_ref().unwrap();
        let by_name = g
            .view(|txn| txn.nodes_by_property("Library", "Id", PropValue::Str("numpy".to_owned())))
            .unwrap();
        assert_eq!(by_name.len(), 1);
    }

    /// Write `batch` to `path` as a single-row-group Parquet file for the import
    /// tests.
    fn write_parquet(path: &std::path::Path, batch: &arrow_array::RecordBatch) {
        use parquet::arrow::arrow_writer::ArrowWriter;
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        writer.write(batch).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn test_import_nodes_loads_parquet_columns_as_properties() {
        use arrow_array::{Int64Array, RecordBatch, StringArray};
        use arrow_schema::{DataType, Field, Schema};
        use std::sync::Arc;

        let temp = TempDir::new().unwrap();
        let mut state = State::new(None, None, 1);
        assert!(handle(
            &mut state,
            &format!(":open {}", temp.path().display())
        ));

        // A Parquet file with a typed integer `Id` column and a string column.
        let schema = Arc::new(Schema::new(vec![
            Field::new("Id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![10i64, 20])),
                Arc::new(StringArray::from(vec!["Alice", "Bob"])),
            ],
        )
        .unwrap();
        let pq = temp.path().join("users.parquet");
        write_parquet(&pq, &batch);

        assert!(handle(
            &mut state,
            &format!(":import-nodes {} User", pq.display())
        ));

        let g = state.graph.as_ref().unwrap();
        let users = g.view(|txn| txn.nodes_by_label("User")).unwrap();
        assert_eq!(users.len(), 2);

        // The Parquet integer column types the `Id` as an integer, so the same
        // auto-index lookup the edge importer relies on resolves it.
        let by_id = g
            .view(|txn| txn.nodes_by_property("User", "Id", PropValue::Int(10)))
            .unwrap();
        assert_eq!(by_id.len(), 1);

        // The string column is stored verbatim as a string property.
        let g = state.graph.as_ref().unwrap();
        let by_name = g
            .view(|txn| txn.nodes_by_property("User", "name", PropValue::Str("Bob".to_owned())))
            .unwrap();
        assert_eq!(by_name.len(), 1);
    }

    #[test]
    fn test_import_edges_resolves_parquet_domain_keys() {
        use arrow_array::{Int64Array, RecordBatch};
        use arrow_schema::{DataType, Field, Schema};
        use std::sync::Arc;

        let temp = TempDir::new().unwrap();
        let mut state = State::new(None, None, 1);
        assert!(handle(
            &mut state,
            &format!(":open {}", temp.path().display())
        ));

        assert!(handle(&mut state, "add-node User {\"Id\": 10}"));
        assert!(handle(&mut state, "add-node User {\"Id\": 20}"));
        assert!(handle(&mut state, "add-node Kernel {\"Id\": 100}"));
        assert!(handle(&mut state, "add-node Kernel {\"Id\": 200}"));

        // The first two columns are the source and destination keys by position;
        // the third row's dst (999) has no node and is dropped.
        let schema = Arc::new(Schema::new(vec![
            Field::new("from_user_id", DataType::Int64, false),
            Field::new("to_kernel_id", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![10i64, 20, 10])),
                Arc::new(Int64Array::from(vec![100i64, 200, 999])),
            ],
        )
        .unwrap();
        let pq = temp.path().join("edges.parquet");
        write_parquet(&pq, &batch);

        assert!(handle(
            &mut state,
            &format!(":import-edges {} User Kernel AUTHORED_KERNEL", pq.display())
        ));

        let g = state.graph.as_ref().unwrap();
        let count = g
            .view(|txn| txn.edge_count_by_type("AUTHORED_KERNEL"))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn shell_escape_runs_interactively() {
        let temp = TempDir::new().unwrap();
        let marker = temp.path().join("ran");
        let mut state = State::new(None, None, 1);

        // At the interactive prompt the escape runs the command.
        assert!(handle(
            &mut state,
            &format!(":! touch {}", marker.display())
        ));
        assert!(marker.exists());
    }

    #[test]
    fn script_run_succeeds_when_all_commands_ok() {
        let temp = TempDir::new().unwrap();
        let script = temp.path().join("ok.txt");
        std::fs::write(
            &script,
            format!(
                ":open {}\nadd-node Person {{\"name\": \"Alice\"}}\nMATCH (n) RETURN count(n)\n",
                temp.path().join("db").display()
            ),
        )
        .unwrap();

        let mut state = State::new(None, None, 1);
        assert!(run_script(&mut state, script.to_str().unwrap()));
    }

    #[test]
    fn script_run_stops_at_first_failing_command() {
        let temp = TempDir::new().unwrap();
        let script = temp.path().join("bad.txt");
        // A bad command sits between two good ones; the trailing one must not run.
        std::fs::write(
            &script,
            format!(
                ":open {}\nadd-node Person {{\"name\": \"Alice\"}}\nbogus-command 1 2\nadd-node Person {{\"name\": \"Carol\"}}\n",
                temp.path().join("db").display()
            ),
        )
        .unwrap();

        let mut state = State::new(None, None, 1);
        assert!(!run_script(&mut state, script.to_str().unwrap()));

        // The command after the failure never executed, so only Alice exists.
        let g = state.graph.as_ref().unwrap();
        let people = g.view(|txn| txn.nodes_by_label("Person")).unwrap();
        assert_eq!(people.len(), 1);
    }

    #[test]
    fn script_run_fails_on_missing_file() {
        let mut state = State::new(None, None, 1);
        assert!(!run_script(&mut state, "/no/such/script.txt"));
    }

    #[test]
    fn shell_escape_is_rejected_inside_a_script() {
        let temp = TempDir::new().unwrap();
        let marker = temp.path().join("ran");
        let script = temp.path().join("setup.txt");
        std::fs::write(&script, format!(":! touch {}\n", marker.display())).unwrap();

        let mut state = State::new(None, None, 1);
        assert!(handle(&mut state, &format!(":run {}", script.display())));

        // The script line is parsed and dispatched, but the escape refuses to run
        // from inside a script, so the command never executes.
        assert!(!marker.exists());
        // The flag is cleared once the script finishes.
        assert!(!state.in_script);
    }
}
