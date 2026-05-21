use std::path::PathBuf;

use issundb::{Graph, Hit, NodeId, RetrieveOptions, retrieve_with};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let path = std::env::args().nth(1).map(PathBuf::from);

    let mut graph: Option<Graph> = None;

    if let Some(ref p) = path {
        match Graph::open(p, 1) {
            Ok(g) => {
                eprintln!("opened: {}", p.display());
                graph = Some(g);
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }

    let mut rl = match DefaultEditor::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("readline init failed: {e}");
            return;
        }
    };

    loop {
        let prompt = if graph.is_some() {
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
                if !handle(&mut graph, &line) {
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
}

// ---------------------------------------------------------------------------
// Command dispatch
// Returns false to signal exit.
// ---------------------------------------------------------------------------

fn handle(graph: &mut Option<Graph>, line: &str) -> bool {
    let (cmd, rest) = split_cmd(line);

    match cmd {
        "quit" | "exit" => return false,
        "help" => print_help(),
        ":open" => {
            let p = PathBuf::from(rest);
            match Graph::open(&p, 1) {
                Ok(g) => {
                    eprintln!("opened: {}", p.display());
                    *graph = Some(g);
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        _ => match graph.as_ref() {
            None => eprintln!("no database open; use :open <path>"),
            Some(g) => dispatch(g, cmd, rest),
        },
    }

    true
}

fn dispatch(g: &Graph, cmd: &str, rest: &str) {
    match cmd {
        // ---- nodes --------------------------------------------------------
        "add-node" => {
            let (label, json) = split_cmd(rest);
            let props = parse_json(json);
            match g.add_node(label, &props) {
                Ok(id) => println!("{id}"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "get-node" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: get-node <id>"),
            Ok(id) => match g.get_node(NodeId::from(id)) {
                Ok(Some(r)) => println!("label={} props={}", r.label, decode_props(&r.props)),
                Ok(None) => eprintln!("not found"),
                Err(e) => eprintln!("error: {e}"),
            },
        },

        // ---- edges --------------------------------------------------------
        "add-edge" => {
            let tokens: Vec<&str> = rest.splitn(4, ' ').collect();
            if tokens.len() < 3 {
                eprintln!("usage: add-edge <src> <dst> <type> [json]");
                return;
            }
            let (src, dst, etype) = match (tokens[0].parse::<u64>(), tokens[1].parse::<u64>()) {
                (Ok(s), Ok(d)) => (NodeId::from(s), NodeId::from(d), tokens[2]),
                _ => {
                    eprintln!("usage: add-edge <src> <dst> <type> [json]");
                    return;
                }
            };
            let json_str = tokens.get(3).copied().unwrap_or("{}");
            let props = parse_json(json_str);
            match g.add_edge(src, dst, etype, &props) {
                Ok(id) => println!("{id}"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        "get-edge" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: get-edge <id>"),
            Ok(id) => {
                use issundb::EdgeId;
                match g.get_edge(EdgeId::from(id)) {
                    Ok(Some(r)) => println!(
                        "src={} dst={} type={} props={}",
                        r.src,
                        r.dst,
                        r.edge_type,
                        decode_props(&r.props)
                    ),
                    Ok(None) => eprintln!("not found"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        },

        // ---- adjacency ----------------------------------------------------
        "out" => match rest.parse::<u64>() {
            Err(_) => eprintln!("usage: out <id>"),
            Ok(id) => match g.out_neighbors(NodeId::from(id)) {
                Ok(v) => {
                    for (nb, eid, tid) in v {
                        println!("  node={nb} edge={eid} type={tid}");
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
                        println!("  node={nb} edge={eid} type={tid}");
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            },
        },

        // ---- indexes ------------------------------------------------------
        "label" => match g.nodes_by_label(rest) {
            Ok(ids) => {
                println!("{} node(s)", ids.len());
                for id in &ids {
                    println!("  {id}");
                }
            }
            Err(e) => eprintln!("error: {e}"),
        },
        "etype" => match g.edges_by_type(rest) {
            Ok(ids) => {
                println!("{} edge(s)", ids.len());
                for id in &ids {
                    println!("  {id}");
                }
            }
            Err(e) => eprintln!("error: {e}"),
        },

        // ---- traversal ----------------------------------------------------
        "bfs" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                eprintln!("usage: bfs <id> <hops>");
                return;
            }
            let id = tokens[0].parse::<u64>();
            let hops = tokens[1].parse::<u8>();
            match (id, hops) {
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

        // ---- graph algorithms ---------------------------------------------
        "pagerank" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            let iters: u32 = tokens.first().and_then(|s| s.parse().ok()).unwrap_or(20);
            let damping: f32 = tokens.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.85);
            match g.page_rank(iters, damping) {
                Ok(scores) => {
                    let mut sorted: Vec<_> = scores.iter().collect();
                    sorted
                        .sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
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

        // ---- vector -------------------------------------------------------
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

        // ---- GraphRAG -----------------------------------------------------
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
                            seeds.sort_by(|a, b| {
                                a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)
                            });
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

        _ => eprintln!("unknown command: {cmd}; type help for a list"),
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

fn parse_json(s: &str) -> serde_json::Value {
    if s.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
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

fn print_help() {
    println!(
        r#"Commands:
  :open <path>                   open or reopen a database
  add-node <label> [json]        add a node; prints NodeId
  get-node <id>                  get a node by id
  add-edge <src> <dst> <type> [json]  add an edge; prints EdgeId
  get-edge <id>                  get an edge by id
  out <id>                       outgoing neighbors
  in <id>                        incoming neighbors
  label <label>                  nodes by label
  etype <type>                   edges by type
  bfs <id> <hops>                breadth-first expansion
  path <src> <dst>               shortest path
  pagerank [iters] [damping]     PageRank (default: 20 iters, 0.85 damping)
  components                     connected components count
  rebuild-csr                    rebuild the CSR cache
  upsert-vec <id> <f32>...       attach a vector embedding to a node
  vsearch <k> <f32>...           k-nearest-neighbor search
  retrieve <k> <hops> <f32>...   GraphRAG: vector search + BFS expansion
  help                           show this message
  quit / exit                    exit"#
    );
}
