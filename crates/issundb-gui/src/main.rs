use std::collections::HashMap;

use eframe::{App, CreationContext, NativeOptions};
use egui::{Color32, Context, ScrollArea};
use egui_graphs::{Graph, GraphView};
use petgraph::stable_graph::{NodeIndex, StableGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use issundb::{Graph as IssunGraph, GraphQueryExt, QueryResult};

// ---------------------------------------------------------------------------
// Payloads for Graph Visualization
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct NodePayload {
    pub id: u64,
    pub label: String,
    pub properties: Value,
}

#[derive(Clone, Debug)]
struct EdgePayload {
    pub id: u64,
    pub type_name: String,
    pub properties: Value,
}

// ---------------------------------------------------------------------------
// Stable Graph Layout for egui_graphs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StableLayoutState {
    triggered: bool,
}
impl egui_graphs::LayoutState for StableLayoutState {}

#[derive(Debug, Default)]
struct StableLayout {
    state: StableLayoutState,
}

impl egui_graphs::Layout<StableLayoutState> for StableLayout {
    fn from_state(state: StableLayoutState) -> impl egui_graphs::Layout<StableLayoutState> {
        Self { state }
    }

    fn next<N, E, Ty, Ix, Dn, De>(&mut self, g: &mut Graph<N, E, Ty, Ix, Dn, De>, _ui: &egui::Ui)
    where
        N: Clone,
        E: Clone,
        Ty: petgraph::EdgeType,
        Ix: petgraph::stable_graph::IndexType,
        Dn: egui_graphs::DisplayNode<N, E, Ty, Ix>,
        De: egui_graphs::DisplayEdge<N, E, Ty, Ix, Dn>,
    {
        if self.state.triggered {
            return;
        }

        let mut rng = rand::thread_rng();
        for node in g.g_mut().node_weights_mut() {
            if node.location() == egui::Pos2::ZERO {
                use rand::Rng;
                node.set_location(egui::Pos2::new(
                    rng.gen_range(0.0..250.0),
                    rng.gen_range(0.0..250.0),
                ));
            }
        }

        self.state.triggered = true;
    }

    fn state(&self) -> StableLayoutState {
        self.state.clone()
    }
}

// ---------------------------------------------------------------------------
// View Mode Enum
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum ViewMode {
    Graph,
    Table,
    Explain,
}

// ---------------------------------------------------------------------------
// GUI App State
// ---------------------------------------------------------------------------

struct GuiApp {
    db_path: String,
    graph_instance: Option<IssunGraph>,
    cypher_input: String,
    error_message: Option<String>,
    query_success_message: Option<String>,

    // Graph display state
    pet_graph: StableGraph<NodePayload, EdgePayload>,
    visual_graph: Option<Graph<NodePayload, EdgePayload>>,

    // Inspector Selection State
    selected_node: Option<NodePayload>,
    selected_edge: Option<EdgePayload>,

    // Features transferred from FalkorDB GUI
    view_mode: ViewMode,
    query_history: Vec<String>,
    last_query_result: Option<QueryResult>,
    explain_plan: Option<String>,
    node_positions: HashMap<u64, egui::Pos2>,
    reset_layout_flag: bool,
}

impl GuiApp {
    fn new(_cc: &CreationContext<'_>, initial_path: Option<String>) -> Self {
        let db_path = initial_path.unwrap_or_else(|| "./issundb-data".to_string());
        let mut app = Self {
            db_path,
            graph_instance: None,
            cypher_input: "MATCH (n) RETURN n LIMIT 10".to_string(),
            error_message: None,
            query_success_message: None,
            pet_graph: StableGraph::new(),
            visual_graph: None,
            selected_node: None,
            selected_edge: None,
            view_mode: ViewMode::Graph,
            query_history: Vec::new(),
            last_query_result: None,
            explain_plan: None,
            node_positions: HashMap::new(),
            reset_layout_flag: false,
        };
        app.try_open_db();
        app
    }

    fn history_file_path(&self) -> String {
        format!("{}/.issundb_query_history.json", self.db_path)
    }

    fn load_history(&mut self) {
        self.query_history.clear();
        let path = self.history_file_path();
        if let Ok(file_content) = std::fs::read_to_string(path) {
            if let Ok(history) = serde_json::from_str::<Vec<String>>(&file_content) {
                self.query_history = history;
            }
        }
    }

    fn save_history(&self) {
        let path = self.history_file_path();
        if let Ok(json) = serde_json::to_string(&self.query_history) {
            let _ = std::fs::write(path, json);
        }
    }

    fn try_open_db(&mut self) {
        self.error_message = None;
        self.selected_node = None;
        self.selected_edge = None;
        self.node_positions.clear();
        match IssunGraph::open(std::path::Path::new(&self.db_path), 1) {
            Ok(g) => {
                self.graph_instance = Some(g);
                self.query_success_message = Some("Database opened successfully.".to_string());
                self.load_history();
                self.run_query();
            }
            Err(e) => {
                self.graph_instance = None;
                self.error_message = Some(format!("Failed to open database: {e}"));
            }
        }
    }

    fn run_query(&mut self) {
        self.error_message = None;
        self.query_success_message = None;
        let Some(ref g) = self.graph_instance else {
            self.error_message = Some("No database open.".to_string());
            return;
        };

        if self.cypher_input.trim().is_empty() {
            return;
        }

        // Rebuild CSR snapshot to make sure GraphBLAS physical plans are in sync with latest DB modifications
        let _ = g.rebuild_csr();

        // Execute Cypher
        match g.query(&self.cypher_input) {
            Ok(res) => {
                let cypher = self.cypher_input.clone();
                // Add to history
                self.query_history.retain(|x| x != &cypher);
                self.query_history.insert(0, cypher);
                if self.query_history.len() > 20 {
                    self.query_history.truncate(20);
                }
                self.save_history();

                self.last_query_result = Some(res.clone());
                self.load_query_result(res);
                self.query_success_message = Some("Query executed successfully.".to_string());
                self.view_mode = ViewMode::Graph;
                self.reset_layout_flag = true;
            }
            Err(e) => {
                self.error_message = Some(format!("Cypher execution error: {e}"));
            }
        }
    }

    fn explain_query(&mut self) {
        self.error_message = None;
        self.query_success_message = None;
        let Some(ref g) = self.graph_instance else {
            self.error_message = Some("No database open.".to_string());
            return;
        };

        if self.cypher_input.trim().is_empty() {
            return;
        }

        // Rebuild CSR snapshot
        let _ = g.rebuild_csr();

        match g.explain(&self.cypher_input) {
            Ok(plan) => {
                self.explain_plan = Some(plan);
                self.query_success_message = Some("Plan generated successfully.".to_string());
                self.view_mode = ViewMode::Explain;
                self.reset_layout_flag = true;
            }
            Err(e) => {
                self.error_message = Some(format!("Cypher explain error: {e}"));
            }
        }
    }

    fn load_query_result(&mut self, res: QueryResult) {
        self.pet_graph.clear();
        self.selected_node = None;
        self.selected_edge = None;

        let mut node_map: HashMap<u64, NodeIndex> = HashMap::new();

        // We traverse the QueryResult and extract NodeRecord or EdgeRecord bindings
        for rec in res.records {
            for val in rec.values {
                if let Some(obj) = val.as_object() {
                    // Check if it's a Node or Relationship representation
                    if obj.contains_key("id")
                        && obj.contains_key("label")
                        && obj.contains_key("properties")
                    {
                        // It's a Node!
                        if let (Some(id_val), Some(lbl), Some(props)) =
                            (obj.get("id"), obj.get("label"), obj.get("properties"))
                        {
                            if let (Some(id), Some(label)) = (id_val.as_u64(), lbl.as_str()) {
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    node_map.entry(id)
                                {
                                    let payload = NodePayload {
                                        id,
                                        label: label.to_string(),
                                        properties: props.clone(),
                                    };
                                    let idx = self.pet_graph.add_node(payload);
                                    e.insert(idx);
                                }
                            }
                        }
                    } else if obj.contains_key("id")
                        && obj.contains_key("src")
                        && obj.contains_key("dst")
                        && obj.contains_key("type")
                        && obj.contains_key("properties")
                    {
                        // It's a Relationship/Edge!
                        if let (
                            Some(id_val),
                            Some(src_val),
                            Some(dst_val),
                            Some(typ_val),
                            Some(props),
                        ) = (
                            obj.get("id"),
                            obj.get("src"),
                            obj.get("dst"),
                            obj.get("type"),
                            obj.get("properties"),
                        ) {
                            if let (Some(id), Some(src), Some(dst), Some(etype)) = (
                                id_val.as_u64(),
                                src_val.as_u64(),
                                dst_val.as_u64(),
                                typ_val.as_str(),
                            ) {
                                // Add nodes if they do not exist
                                let src_idx = *node_map.entry(src).or_insert_with(|| {
                                    let payload = NodePayload {
                                        id: src,
                                        label: "Node".to_string(),
                                        properties: Value::Object(serde_json::Map::new()),
                                    };
                                    self.pet_graph.add_node(payload)
                                });
                                let dst_idx = *node_map.entry(dst).or_insert_with(|| {
                                    let payload = NodePayload {
                                        id: dst,
                                        label: "Node".to_string(),
                                        properties: Value::Object(serde_json::Map::new()),
                                    };
                                    self.pet_graph.add_node(payload)
                                });

                                let edge = EdgePayload {
                                    id,
                                    type_name: etype.to_string(),
                                    properties: props.clone(),
                                };
                                self.pet_graph.add_edge(src_idx, dst_idx, edge);
                            }
                        }
                    }
                }
            }
        }

        let mut visual_g = Graph::from(&self.pet_graph);

        // Pre-build mapping of node index to ID to satisfy borrow checker
        let node_id_mappings: Vec<(NodeIndex, u64)> = visual_g
            .g()
            .node_indices()
            .filter_map(|idx| visual_g.g().node_weight(idx).map(|n| (idx, n.payload().id)))
            .collect();

        // Apply previously saved coordinates to preserve node positions across queries
        for (idx, id) in node_id_mappings {
            if let Some(&pos) = self.node_positions.get(&id) {
                if let Some(node_mut) = visual_g.g_mut().node_weight_mut(idx) {
                    node_mut.set_location(pos);
                }
            }
        }

        self.visual_graph = Some(visual_g);
    }
}

// Custom layouter for Cypher Syntax Highlighting
fn cypher_layouter(ui: &egui::Ui, text: &str, wrap_width: f32) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;

    let default_color = ui.visuals().text_color();
    let keyword_color = Color32::from_rgb(180, 110, 240); // vibrant purple
    let string_color = Color32::from_rgb(110, 190, 110); // soft green
    let number_color = Color32::from_rgb(230, 130, 50); // soft orange
    let comment_color = Color32::from_rgb(120, 130, 140); // blue-gray

    let font_id = egui::TextStyle::Monospace.resolve(ui.style());

    let mut chars = text.chars().peekable();
    let mut word = String::new();

    let mut in_string = None;
    let mut in_comment = false;

    let flush_word = |word: &mut String, job: &mut LayoutJob| {
        if word.is_empty() {
            return;
        }
        let is_keyword = matches!(
            word.to_uppercase().as_str(),
            "MATCH"
                | "RETURN"
                | "WHERE"
                | "CREATE"
                | "DELETE"
                | "DETACH"
                | "LIMIT"
                | "SET"
                | "REMOVE"
                | "WITH"
                | "MERGE"
                | "ORDER"
                | "BY"
                | "ASC"
                | "DESC"
                | "AND"
                | "OR"
                | "NOT"
                | "XOR"
                | "IN"
                | "IS"
                | "NULL"
                | "TRUE"
                | "FALSE"
        );
        let is_number = word.chars().all(|c| c.is_ascii_digit() || c == '.');
        let color = if is_keyword {
            keyword_color
        } else if is_number {
            number_color
        } else {
            default_color
        };
        job.append(
            word,
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        );
        word.clear();
    };

    while let Some(c) = chars.next() {
        if in_comment {
            let mut s = String::new();
            s.push(c);
            while let Some(&nc) = chars.peek() {
                if nc == '\n' {
                    break;
                }
                if let Some(next_c) = chars.next() {
                    s.push(next_c);
                }
            }
            job.append(
                &s,
                0.0,
                TextFormat {
                    font_id: font_id.clone(),
                    color: comment_color,
                    ..Default::default()
                },
            );
            in_comment = false;
        } else if let Some(q) = in_string {
            let mut s = String::new();
            s.push(c);
            if c == q {
                in_string = None;
            } else {
                while let Some(&nc) = chars.peek() {
                    if let Some(next_c) = chars.next() {
                        s.push(next_c);
                    }
                    if nc == q {
                        in_string = None;
                        break;
                    }
                }
            }
            job.append(
                &s,
                0.0,
                TextFormat {
                    font_id: font_id.clone(),
                    color: string_color,
                    ..Default::default()
                },
            );
        } else {
            if c == '/' && chars.peek() == Some(&'/') {
                flush_word(&mut word, &mut job);
                in_comment = true;
                job.append(
                    "/",
                    0.0,
                    TextFormat {
                        font_id: font_id.clone(),
                        color: comment_color,
                        ..Default::default()
                    },
                );
                continue;
            }

            if c == '\'' || c == '"' {
                flush_word(&mut word, &mut job);
                in_string = Some(c);
                job.append(
                    &c.to_string(),
                    0.0,
                    TextFormat {
                        font_id: font_id.clone(),
                        color: string_color,
                        ..Default::default()
                    },
                );
                continue;
            }

            if c.is_alphanumeric() || c == '_' || c == '.' {
                word.push(c);
            } else {
                flush_word(&mut word, &mut job);
                job.append(
                    &c.to_string(),
                    0.0,
                    TextFormat {
                        font_id: font_id.clone(),
                        color: default_color,
                        ..Default::default()
                    },
                );
            }
        }
    }
    flush_word(&mut word, &mut job);

    ui.fonts(|f| f.layout_job(job))
}

impl App for GuiApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // --- Top Menu Bar ---
        egui::TopBottomPanel::top("top_menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("IssunDB Desktop Graph GUI");
                ui.separator();
                ui.label("Database Path:");
                ui.text_edit_singleline(&mut self.db_path);
                if ui.button("Open").clicked() {
                    self.try_open_db();
                }
                if ui.button("📁 Browse").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        self.db_path = folder.to_string_lossy().to_string();
                        self.try_open_db();
                    }
                }
            });
        });

        // --- Left Control Panel ---
        egui::SidePanel::left("left_controls")
            .width_range(280.0..=400.0)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.heading("Cypher Query Console");
                    ui.add_space(5.0);

                    ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.cypher_input)
                                .hint_text("MATCH (n) RETURN n LIMIT 10")
                                .desired_width(f32::INFINITY)
                                .desired_rows(5)
                                .layouter(&mut |ui, text, wrap| {
                                    cypher_layouter(ui, text.as_str(), wrap)
                                }),
                        );
                    });

                    ui.add_space(5.0);
                    ui.horizontal(|ui| {
                        if ui.button("🚀 Execute Query").clicked() {
                            self.run_query();
                        }
                        if ui.button("🔍 Explain Query").clicked() {
                            self.explain_query();
                        }
                    });

                    ui.separator();

                    // Status messages
                    if let Some(ref err) = self.error_message {
                        ui.colored_label(Color32::from_rgb(220, 50, 50), err);
                    }
                    if let Some(ref success) = self.query_success_message {
                        ui.colored_label(Color32::from_rgb(50, 200, 50), success);
                    }

                    ui.separator();

                    // Query History List
                    ui.heading("Query History");
                    ui.add_space(3.0);
                    if self.query_history.is_empty() {
                        ui.weak("No queries in history yet.");
                    } else {
                        ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                            let mut selected_history = None;
                            for q in &self.query_history {
                                let label = if q.len() > 38 {
                                    format!("{}...", &q[..35])
                                } else {
                                    q.clone()
                                };
                                if ui.button(label).on_hover_text(q).clicked() {
                                    selected_history = Some(q.clone());
                                }
                            }
                            if let Some(q) = selected_history {
                                self.cypher_input = q;
                            }
                        });
                    }

                    ui.separator();

                    // Node and Edge Stats
                    ui.label(format!("Loaded Nodes: {}", self.pet_graph.node_count()));
                    ui.label(format!(
                        "Loaded Relationships: {}",
                        self.pet_graph.edge_count()
                    ));
                });
            });

        // --- Right Inspector Panel ---
        egui::SidePanel::right("right_inspector")
            .width_range(250.0..=350.0)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.heading("Element Inspector");
                    ui.separator();

                    if let Some(ref node) = self.selected_node {
                        ui.colored_label(Color32::from_rgb(100, 200, 255), "Selected Node");
                        ui.label(format!("ID: {}", node.id));
                        ui.label(format!("Label: {}", node.label));
                        ui.add_space(5.0);
                        ui.label("Properties:");
                        ScrollArea::vertical().show(ui, |ui| {
                            if let Ok(pretty) = serde_json::to_string_pretty(&node.properties) {
                                ui.monospace(&pretty);
                            } else {
                                ui.monospace(format!("{:?}", node.properties));
                            }
                        });
                    } else if let Some(ref edge) = self.selected_edge {
                        ui.colored_label(Color32::from_rgb(255, 200, 100), "Selected Relationship");
                        ui.label(format!("ID: {}", edge.id));
                        ui.label(format!("Type: {}", edge.type_name));
                        ui.add_space(5.0);
                        ui.label("Properties:");
                        ScrollArea::vertical().show(ui, |ui| {
                            if let Ok(pretty) = serde_json::to_string_pretty(&edge.properties) {
                                ui.monospace(&pretty);
                            } else {
                                ui.monospace(format!("{:?}", edge.properties));
                            }
                        });
                    } else {
                        ui.weak(
                            "Click a node or edge in the visualization to inspect its details.",
                        );
                    }
                });
            });

        // --- Central Panel containing tabs and active mode view ---
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view_mode, ViewMode::Graph, "🕸 Graph View");
                ui.selectable_value(&mut self.view_mode, ViewMode::Table, "📋 Table View");
                ui.selectable_value(&mut self.view_mode, ViewMode::Explain, "🔍 Explain Plan");
            });
            ui.separator();

            match self.view_mode {
                ViewMode::Graph => {
                    if self.reset_layout_flag {
                        ui.data_mut(|data| {
                            data.insert_persisted(
                                egui::Id::new("egui_graphs_layout"),
                                StableLayoutState::default(),
                            );
                        });
                        self.reset_layout_flag = false;
                    }

                    if let Some(ref mut vis_g) = self.visual_graph {
                        // Render the interactive GraphView widget specifying layout type parameters
                        ui.add(&mut GraphView::<
                            _,
                            _,
                            _,
                            _,
                            _,
                            _,
                            StableLayoutState,
                            StableLayout,
                        >::new(vis_g));

                        // Process selections from the interactive graph view
                        let mut found_node = None;
                        for idx in vis_g.g().node_indices() {
                            if let Some(vis_node) = vis_g.g().node_weight(idx) {
                                if vis_node.selected() {
                                    found_node = Some(vis_node.payload().clone());
                                    break;
                                }
                            }
                        }
                        self.selected_node = found_node;

                        if self.selected_node.is_none() {
                            let mut found_edge = None;
                            for idx in vis_g.g().edge_indices() {
                                if let Some(vis_edge) = vis_g.g().edge_weight(idx) {
                                    if vis_edge.selected() {
                                        found_edge = Some(vis_edge.payload().clone());
                                        break;
                                    }
                                }
                            }
                            self.selected_edge = found_edge;
                        } else {
                            self.selected_edge = None;
                        }

                        // Save locations of nodes so that we preserve their positions in the next query
                        for idx in vis_g.g().node_indices() {
                            if let Some(vis_node) = vis_g.g().node_weight(idx) {
                                let id = vis_node.payload().id;
                                self.node_positions.insert(id, vis_node.location());
                            }
                        }
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("Execute a query to visualize the graph.");
                        });
                    }
                }
                ViewMode::Table => {
                    if let Some(ref res) = self.last_query_result {
                        let col_count = res.columns.len();
                        if col_count == 0 {
                            ui.centered_and_justified(|ui| {
                                ui.label("Query returned empty result.");
                            });
                        } else {
                            use egui_extras::{Column, TableBuilder};
                            ScrollArea::both().show(ui, |ui| {
                                TableBuilder::new(ui)
                                    .striped(true)
                                    .resizable(true)
                                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                                    .columns(Column::auto(), col_count)
                                    .header(20.0, |mut header| {
                                        for col in &res.columns {
                                            header.col(|ui| {
                                                ui.strong(col);
                                            });
                                        }
                                    })
                                    .body(|body| {
                                        body.rows(20.0, res.records.len(), |mut row| {
                                            let r_idx = row.index();
                                            let record = &res.records[r_idx];
                                            for val in &record.values {
                                                row.col(|ui| {
                                                    let val_str = match val {
                                                        Value::String(s) => s.clone(),
                                                        other => other.to_string(),
                                                    };
                                                    ui.label(val_str);
                                                });
                                            }
                                        });
                                    });
                            });
                        }
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("Execute a query to view results in a table.");
                        });
                    }
                }
                ViewMode::Explain => {
                    if let Some(ref plan) = self.explain_plan {
                        ScrollArea::both().show(ui, |ui| {
                            ui.monospace(plan);
                        });
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("Click 'Explain Query' to generate the physical plan.");
                        });
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Entry Point
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    let initial_path = std::env::args().nth(1);
    let options = NativeOptions {
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "IssunDB Graph Visualizer",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc, initial_path)))),
    )
}
