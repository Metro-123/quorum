//! quorum Desktop UI — native egui/eframe frontend for the quorum SQL database.

use eframe::egui;
use quorum_proto::{ClientRequest, ClientResponse, encode_request, decode_response, write_frame, read_frame, QueryResult as ProtoQueryResult};
use quorum_types::{Value, DbError};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::result::Result as StdResult;

// -----------------------------------------------------------------------------
// App state
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConnectionConfig {
    addr: String,
    name: String,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:7300".to_string(),
            name: "local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryHistoryEntry {
    sql: String,
    timestamp: String,
    success: bool,
    rows: usize,
    message: String,
}

struct Connection {
    stream: Option<TcpStream>,
    config: ConnectionConfig,
    connected: bool,
    last_error: Option<String>,
}

impl Connection {
    fn new(config: ConnectionConfig) -> Self {
        Self {
            stream: None,
            config,
            connected: false,
            last_error: None,
        }
    }

    fn connect(&mut self) -> StdResult<(), DbError> {
        let stream = TcpStream::connect(&self.config.addr)
            .map_err(|e| DbError::Network(format!("connect failed: {}", e)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        self.stream = Some(stream);
        self.connected = true;
        self.last_error = None;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.stream = None;
        self.connected = false;
    }

    fn execute(&mut self, sql: &str) -> StdResult<Vec<QueryResult>, DbError> {
        let stream = self.stream.as_mut()
            .ok_or_else(|| DbError::Network("not connected".into()))?;

        let req = ClientRequest::Execute { sql: sql.to_string() };
        let payload = encode_request(&req);
        write_frame(stream, &payload)?;

        let resp_payload = read_frame(stream)?;
        let resp = decode_response(&resp_payload)?;

        match resp {
            ClientResponse::Results { results } => Ok(results.into_iter().map(Into::into).collect()),
            ClientResponse::Error { message } => Err(DbError::Internal(message)),
            ClientResponse::Pong => Ok(vec![]),
        }
    }

    fn ping(&mut self) -> StdResult<(), DbError> {
        let stream = self.stream.as_mut()
            .ok_or_else(|| DbError::Network("not connected".into()))?;
        let payload = encode_request(&ClientRequest::Ping);
        write_frame(stream, &payload)?;
        let resp_payload = read_frame(stream)?;
        let resp = decode_response(&resp_payload)?;
        match resp {
            ClientResponse::Pong => Ok(()),
            ClientResponse::Error { message } => Err(DbError::Internal(message)),
            _ => Err(DbError::Internal("unexpected response".into())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    message: String,
}

impl From<ProtoQueryResult> for QueryResult {
    fn from(qr: ProtoQueryResult) -> Self {
        QueryResult {
            columns: qr.columns,
            rows: qr.rows,
            message: qr.message,
        }
    }
}

struct QuorumApp {
    // Connection
    connection: Connection,
    saved_connections: Vec<ConnectionConfig>,

    // Query editor
    query_text: String,
    query_history: Vec<QueryHistoryEntry>,

    // Results
    current_results: Option<QueryResult>,

    // Schema browser
    schema_cache: Option<SchemaInfo>,
    selected_table: Option<String>,

    // UI state
    show_connection_dialog: bool,
    new_connection_name: String,
    new_connection_addr: String,
    status_message: String,
    dark_mode: bool,
}

#[derive(Debug, Clone)]
struct SchemaInfo {
    tables: HashMap<String, TableInfo>,
}

#[derive(Debug, Clone)]
struct TableInfo {
    columns: Vec<ColumnInfo>,
    row_count: Option<u64>,
}

#[derive(Debug, Clone)]
struct ColumnInfo {
    name: String,
    data_type: String,
    nullable: bool,
    primary_key: bool,
}

impl Default for QuorumApp {
    fn default() -> Self {
        let config = ConnectionConfig::default();
        let mut app = Self {
            connection: Connection::new(config.clone()),
            saved_connections: vec![config],
            query_text: String::new(),
            query_history: Vec::new(),
            current_results: None,
            schema_cache: None,
            selected_table: None,
            show_connection_dialog: false,
            new_connection_name: String::new(),
            new_connection_addr: String::new(),
            status_message: "Ready".to_string(),
            dark_mode: true,
        };
        app.load_settings();
        app
    }
}

impl QuorumApp {
    fn load_settings(&mut self) {
        if let Some(path) = dirs::config_dir().map(|p| p.join("quorum").join("settings.json")) {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(settings) = serde_json::from_str::<SavedSettings>(&data) {
                    self.saved_connections = settings.connections;
                    self.dark_mode = settings.dark_mode;
                    if let Some(last) = settings.last_connection {
                        self.connection.config = last;
                    }
                }
            }
        }
    }

    fn save_settings(&self) {
        if let Some(path) = dirs::config_dir().map(|p| p.join("quorum").join("settings.json")) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let settings = SavedSettings {
                connections: self.saved_connections.clone(),
                last_connection: Some(self.connection.config.clone()),
                dark_mode: self.dark_mode,
            };
            let _ = std::fs::write(path, serde_json::to_string_pretty(&settings).unwrap());
        }
    }

    fn connect(&mut self) {
        match self.connection.connect() {
            Ok(_) => {
                self.status_message = format!("Connected to {}", self.connection.config.addr);
                self.refresh_schema();
            }
            Err(e) => {
                self.connection.last_error = Some(e.to_string());
                self.status_message = format!("Connection failed: {}", e);
            }
        }
    }

    fn disconnect(&mut self) {
        self.connection.disconnect();
        self.schema_cache = None;
        self.status_message = "Disconnected".to_string();
    }

    fn execute_query(&mut self) {
        let sql = self.query_text.trim();
        if sql.is_empty() {
            return;
        }

        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();

        match self.connection.execute(sql) {
            Ok(results) => {
                let result = results.last().cloned().unwrap_or(QueryResult {
                    columns: vec![],
                    rows: vec![],
                    message: "OK".to_string(),
                });
                let rows = result.rows.len();
                let message = result.message.clone();

                self.current_results = Some(result.clone());
                self.query_history.insert(0, QueryHistoryEntry {
                    sql: sql.to_string(),
                    timestamp,
                    success: true,
                    rows,
                    message: message.clone(),
                });
                self.status_message = format!("Query OK: {}", message);
            }
            Err(e) => {
                self.query_history.insert(0, QueryHistoryEntry {
                    sql: sql.to_string(),
                    timestamp,
                    success: false,
                    rows: 0,
                    message: e.to_string(),
                });
                self.status_message = format!("Error: {}", e);
            }
        }
        self.save_settings();
    }

    fn refresh_schema(&mut self) {
        // For now, just try to list tables via a simple query
        // The actual schema depends on the server implementation
        let _ = self.connection.execute("SELECT 1"); // Test connection
    }

    fn add_connection(&mut self) {
        let config = ConnectionConfig {
            name: self.new_connection_name.clone(),
            addr: self.new_connection_addr.clone(),
        };
        if !self.saved_connections.iter().any(|c| c.name == config.name) {
            self.saved_connections.push(config);
        }
        self.show_connection_dialog = false;
        self.new_connection_name.clear();
        self.new_connection_addr.clear();
        self.save_settings();
    }

    fn switch_connection(&mut self, idx: usize) {
        if idx < self.saved_connections.len() {
            self.disconnect();
            self.connection.config = self.saved_connections[idx].clone();
            self.connect();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedSettings {
    connections: Vec<ConnectionConfig>,
    last_connection: Option<ConnectionConfig>,
    dark_mode: bool,
}

// -----------------------------------------------------------------------------
// eframe App
// -----------------------------------------------------------------------------

impl eframe::App for QuorumApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Theme
        let mut style = (*ctx.style()).clone();
        if self.dark_mode {
            style.visuals = egui::Visuals::dark();
        } else {
            style.visuals = egui::Visuals::light();
        }
        ctx.set_style(style);

        // Top menu bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New Connection…").clicked() {
                        self.show_connection_dialog = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Toggle Dark Mode").clicked() {
                        self.dark_mode = !self.dark_mode;
                        self.save_settings();
                        ui.close_menu();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About").clicked() {
                        // TODO: show about dialog
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let status_color = if self.connection.connected {
                        egui::Color32::GREEN
                    } else {
                        egui::Color32::RED
                    };
                    ui.colored_label(status_color, if self.connection.connected { "● Connected" } else { "○ Disconnected" });
                    ui.separator();
                    ui.label(&self.connection.config.name);
                });
            });
        });

        // Status bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status_message);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear Results").clicked() {
                        self.current_results = None;
                    }
                });
            });
        });

        // Left panel: Schema browser + Connections
        egui::SidePanel::left("sidebar")
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("📊 Schema");
                ui.separator();

                if let Some(schema) = &self.schema_cache {
                    for (table_name, table) in &schema.tables {
                        let is_selected = self.selected_table.as_ref() == Some(table_name);
                        if ui.selectable_label(is_selected, format!("📋 {}", table_name)).clicked() {
                            self.selected_table = Some(table_name.clone());
                        }
                        if is_selected {
                            ui.indent("columns", |ui| {
                                for col in &table.columns {
                                    let pk = if col.primary_key { " 🔑" } else { "" };
                                    let null = if col.nullable { " NULL" } else { " NOT NULL" };
                                    ui.small(format!("  {} : {}{}{}", col.name, col.data_type, null, pk));
                                }
                            });
                        }
                    }
                } else {
                    ui.label("Not connected or no schema loaded");
                }

                ui.separator();
                ui.heading("🔌 Connections");
                // Collect indices to avoid borrow checker issues
                let indices: Vec<usize> = (0..self.saved_connections.len()).collect();
                for idx in indices {
                    let conn = &self.saved_connections[idx];
                    let is_active = self.connection.config.name == conn.name;
                    let label = if is_active { format!("▶ {}", conn.name) } else { conn.name.clone() };
                    if ui.selectable_label(is_active, label).clicked() {
                        self.switch_connection(idx);
                    }
                }
                if ui.button("➕ New Connection").clicked() {
                    self.show_connection_dialog = true;
                }
            });

        // Main panel: Query editor + Results
        egui::CentralPanel::default().show(ctx, |ui| {
            // Query editor
            egui::TopBottomPanel::top("query_editor")
                .resizable(true)
                .min_height(200.0)
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("SQL:");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("▶ Run (Ctrl+Enter)").clicked() {
                                self.execute_query();
                            }
                            if ui.button("Clear").clicked() {
                                self.query_text.clear();
                            }
                            if ui.button("Format").clicked() {
                                // TODO: SQL formatter
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let font = egui::FontId::monospace(14.0);
                    egui::ScrollArea::vertical()
                        .id_salt("query_editor")
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.query_text)
                                    .font(font)
                                    .desired_rows(10)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("-- Enter SQL here...\n-- Ctrl+Enter to execute")
                            );
                        });
                });

            // Results
            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("📈 Results");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(r) = &self.current_results {
                            ui.label(format!("{} rows", r.rows.len()));
                        }
                    });
                });
                ui.separator();

                if let Some(result) = &self.current_results {
                    if result.columns.is_empty() {
                        ui.label(&result.message);
                    } else {
                        egui::ScrollArea::both().show(ui, |ui| {
                            egui::Grid::new("results_table")
                                .striped(true)
                                .min_col_width(80.0)
                                .show(ui, |ui| {
                                    // Header
                                    for col in &result.columns {
                                        ui.strong(col);
                                    }
                                    ui.end_row();

                                    // Rows
                                    for row in &result.rows {
                                        for val in row {
                                            ui.label(val.to_string());
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(50.0);
                        ui.label("Execute a query to see results");
                    });
                }
            });
        });

        // Connection dialog
        if self.show_connection_dialog {
            egui::Window::new("New Connection")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.text_edit_singleline(&mut self.new_connection_name);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Address:");
                        ui.text_edit_singleline(&mut self.new_connection_addr);
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            self.add_connection();
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_connection_dialog = false;
                        }
                    });
                });
        }

        // Keyboard shortcuts
        ctx.input(|i| {
            if i.modifiers.ctrl && i.key_pressed(egui::Key::Enter) {
                self.execute_query();
            }
            if i.key_pressed(egui::Key::Escape) {
                self.show_connection_dialog = false;
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings();
    }
}

fn main() -> StdResult<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("quorum — Distributed SQL Database"),
        ..Default::default()
    };

    eframe::run_native(
        "quorum",
        options,
        Box::new(|cc| {
            Ok(Box::new(QuorumApp::default()))
        }),
    )
}