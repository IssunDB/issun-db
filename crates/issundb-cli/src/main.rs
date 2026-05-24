use std::path::PathBuf;

use issundb::{EdgeId, Graph, Hit, NodeId, RetrieveOptions, VectorGraphExt, retrieve_with};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

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

fn handle(graph: &mut Option<Graph>, line: &str) -> bool {
    let (cmd, rest) = split_cmd(line);
    match cmd {
        "quit" | "exit" => return false,
        "help" => print_help(),
        ":open" => {
            if rest.is_empty() {
                eprintln!("usage: :open <path>");
            } else {
                let p = PathBuf::from(rest);
                match Graph::open(&p, 1) {
                    Ok(g) => {
                        eprintln!("opened: {}", p.display());
                        *graph = Some(g);
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
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
        "add-edge" => {
            let mut s = rest;
            let src = next_token(&mut s).and_then(|t| t.parse::<u64>().ok());
            let dst = next_token(&mut s).and_then(|t| t.parse::<u64>().ok());
            let etype = next_token(&mut s);
            match (src, dst, etype) {
                (Some(s_id), Some(d_id), Some(t)) => match parse_props(s.trim()) {
                    Err(e) => eprintln!("invalid props: {e}"),
                    Ok(props) => {
                        match g.add_edge(NodeId::from(s_id), NodeId::from(d_id), t, &props) {
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
        _ => eprintln!("unknown command: {cmd}; type help for a list"),
    }
}

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

fn print_help() {
    println!(
        r#"Commands:
  :open <path>                        open or reopen a database
  add-node <label> [json]             add a node; prints NodeId
  get-node <id>                       get a node by id
  add-edge <src> <dst> <type> [json]  add an edge; prints EdgeId
  get-edge <id>                       get an edge by id
  out <id>                            outgoing neighbors
  in <id>                             incoming neighbors
  label <label>                       nodes by label
  etype <type>                        edges by type
  bfs <id> <hops>                     breadth-first expansion
  path <src> <dst>                    shortest path
  pagerank [iters] [damping]          PageRank (default: 20 iters, 0.85 damping)
  components                          connected components count
  rebuild-csr                         rebuild the CSR cache
  upsert-vec <id> <f32>...            attach a vector embedding to a node
  vsearch <k> <f32>...                k-nearest-neighbor search
  retrieve <k> <hops> <f32>...        hybrid retrieval: vector search plus BFS expansion
  help                                show this message
  quit / exit                         exit"#
    );
}
