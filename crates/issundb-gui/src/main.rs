use std::collections::HashMap;

use eframe::{App, CreationContext, NativeOptions};
use egui::{Color32, Context, ScrollArea};
use egui_graphs::{Graph, GraphView, LayoutRandom, LayoutStateRandom};
use petgraph::stable_graph::{NodeIndex, StableGraph};
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
        };
        app.try_open_db();
        app
    }

    fn try_open_db(&mut self) {
        self.error_message = None;
        self.selected_node = None;
        self.selected_edge = None;
        match IssunGraph::open(std::path::Path::new(&self.db_path), 1) {
            Ok(g) => {
                self.graph_instance = Some(g);
                self.query_success_message = Some("Database opened successfully.".to_string());
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
                self.load_query_result(res);
                self.query_success_message = Some("Query executed successfully.".to_string());
            }
            Err(e) => {
                self.error_message = Some(format!("Cypher execution error: {e}"));
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

        self.visual_graph = Some(Graph::from(&self.pet_graph));
    }
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
                                .desired_rows(5),
                        );
                    });

                    ui.add_space(5.0);
                    if ui.button("🚀 Execute Query").clicked() {
                        self.run_query();
                    }

                    ui.separator();

                    // Status messages
                    if let Some(ref err) = self.error_message {
                        ui.colored_label(Color32::from_rgb(220, 50, 50), err);
                    }
                    if let Some(ref success) = self.query_success_message {
                        ui.colored_label(Color32::from_rgb(50, 200, 50), success);
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

        // --- Central Visual Graph Canvas ---
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ref mut vis_g) = self.visual_graph {
                // Render the interactive GraphView widget specifying layout type parameters
                ui.add(&mut GraphView::<
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    LayoutStateRandom,
                    LayoutRandom,
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
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Execute a query to visualize the graph.");
                });
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
