use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui::{self};
use sysinfo::Disks;

use crate::core::disk::get_disks;
use crate::core::tree::{Msg, Node, format_size, scan_depth};

const PREFETCH_DEPTH: usize = 2;

fn normalize_path(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.len() == 2 && s.ends_with(':') {
        return PathBuf::from(format!("{}\\", s));
    }
    path
}

fn sort_children(children: &mut Vec<Node>) {
    children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

struct ScanState {
    rx: Receiver<Msg>,
    is_initial: bool,
}

pub struct FalconApp {
    disks: Disks,
    root_path: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    cache: HashMap<PathBuf, Vec<Node>>,
    scans: HashMap<PathBuf, ScanState>,
    /// Dirs whose size is still pending (showing ⏳)
    pending_sizes: HashSet<PathBuf>,
    initial_loading: bool,
    display_progress: f32,
    selected_disk: Option<PathBuf>,
    /// Bytes accounted so far during initial scan (from SizeReady of top-level dirs + files)
    scanned_bytes: u64,
    /// Total used bytes on the selected disk (used as progress denominator)
    disk_used_bytes: u64,
    /// When the initial scan started
    scan_start: Option<std::time::Instant>,
    /// When the initial scan ended
    scan_end: Option<std::time::Instant>
}

impl FalconApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            disks: get_disks(),
            root_path: None,
            expanded: HashSet::new(),
            cache: HashMap::new(),
            scans: HashMap::new(),
            pending_sizes: HashSet::new(),
            initial_loading: false,
            display_progress: 0.0,
            selected_disk: if get_disks().list().len() == 1 {
                Some(normalize_path(
                    get_disks().list()[0].mount_point().to_path_buf(),
                ))
            } else {
                None
            },
            scanned_bytes: 0,
            disk_used_bytes: 0,
            scan_start: None,
            scan_end: None,
        }
    }

    fn select_disk(&mut self, path: PathBuf, ctx: &egui::Context) {
        let path = normalize_path(path);

        // Compute used bytes for this disk to use as progress denominator
        let (total, available) = self.disks.list().iter()
            .find(|d| d.mount_point() == path.as_path())
            .map(|d| (d.total_space(), d.available_space()))
            .unwrap_or((0, 0));
        self.disk_used_bytes = total.saturating_sub(available);
        self.scanned_bytes = 0;

        self.selected_disk = Some(path.clone());
        self.root_path = Some(path.clone());
        self.expanded.clear();
        self.expanded.insert(path.clone());
        self.cache.clear();
        self.scans.clear();
        self.pending_sizes.clear();
        self.initial_loading = true;
        self.display_progress = 0.0;
        self.scan_start = Some(std::time::Instant::now());
        self.scan_end = None;
        self.start_scan(path, ctx, true, PREFETCH_DEPTH);
    }

    fn toggle(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.expanded.contains(&path) {
            self.expanded.remove(&path);
            // Clear cached children deeper than depth 2
            let mut to_remove: Vec<PathBuf> = Vec::new();
            for cached_path in self.cache.keys() {
                if cached_path.starts_with(&path) && cached_path != &path {
                    let depth = cached_path.components().count() - path.components().count();
                    if depth > 2 {
                        to_remove.push(cached_path.clone());
                    }
                }
            }
            for path_to_remove in to_remove {
                self.cache.remove(&path_to_remove);
            }
        } else {
            self.expanded.insert(path.clone());
            if !self.cache.contains_key(&path) && !self.scans.contains_key(&path) {
                self.start_scan(path, ctx, false, 2);
            }
        }
    }

    fn start_scan(&mut self, root: PathBuf, ctx: &egui::Context, is_initial: bool, depth: usize) {
        if self.scans.contains_key(&root) {
            return;
        }
        let (tx, rx) = mpsc::channel::<Msg>();
        let ctx_clone = ctx.clone();
        let scan_path = root.clone();
        thread::spawn(move || {
            scan_depth(scan_path, depth, tx, move || {
                ctx_clone.request_repaint()
            });
        });
        self.scans.insert(
            root,
            ScanState {
                rx,
                is_initial,
            },
        );
    }

    fn poll_scans(&mut self, ctx: &egui::Context) {
        let keys: Vec<PathBuf> = self.scans.keys().cloned().collect();
        let mut needs_repaint = false;

        for scan_root in keys {
            let done = {
                let state = self.scans.get_mut(&scan_root).unwrap();
                let mut finished = false;

                match state.rx.try_recv() {
                    Ok(Msg::DirCount { .. }) => {
                        needs_repaint = true;
                    }
                    Ok(Msg::Entry { parent, node }) => {
                        if node.is_dir {
                            self.pending_sizes.insert(node.path.clone());
                        }
                        let children = self.cache.entry(parent).or_default();
                        if !children.iter().any(|c| c.path == node.path) {
                            children.push(node);
                            sort_children(children);
                        }
                        needs_repaint = true;
                    }
                    Ok(Msg::SizeReady { path, size }) => {
                        let is_initial = state.is_initial;
                        self.pending_sizes.remove(&path);
                        if size == 0 {
                            for children in self.cache.values_mut() {
                                children.retain(|n| n.path != path);
                            }
                        } else {
                            for children in self.cache.values_mut() {
                                if let Some(node) = children.iter_mut().find(|n| n.path == path) {
                                    node.size = size;
                                    break;
                                }
                            }
                        }
                        // Only count bytes for direct children of the root to avoid
                        // double-counting (a parent's size already includes its children's).
                        if is_initial && self.disk_used_bytes > 0 {
                            let is_root_child = self.root_path.as_ref()
                                .and_then(|_root| path.parent())
                                .map(|parent| Some(parent) == self.root_path.as_deref())
                                .unwrap_or(false);
                            if is_root_child {
                                self.scanned_bytes = self.scanned_bytes.saturating_add(size);
                                let raw = (self.scanned_bytes as f64 / self.disk_used_bytes as f64).min(0.99) as f32;
                                if raw > self.display_progress {
                                    self.display_progress = raw;
                                }
                            }
                        }
                        needs_repaint = true;
                    }
                    Ok(Msg::Done) => {
                        finished = true;
                    }
                    Err(_) => {}
                }
                finished
            };

            if !done {
                needs_repaint = true;
            }

            if done {
                let is_initial = self.scans.get(&scan_root).map(|s| s.is_initial).unwrap_or(false);
                self.scans.remove(&scan_root);
                if is_initial {
                    self.display_progress = 1.0;
                    self.initial_loading = false;
                    self.scan_end = Some(std::time::Instant::now());
                }
            }
        }

        if !self.pending_sizes.is_empty() {
            needs_repaint = true;
        }

        if needs_repaint {
            ctx.request_repaint();
        }
    }

    fn render_tree(&self, path: &PathBuf, depth: usize, out: &mut Vec<(usize, Node)>) {
        let children = match self.cache.get(path) {
            Some(c) => c,
            None => return,
        };
        for child in children {
            out.push((depth, child.clone()));
            if child.is_dir && self.expanded.contains(&child.path) {
                self.render_tree(&child.path, depth + 1, out);
            }
        }
    }
}

impl eframe::App for FalconApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scans(ctx);

        // Disk picker modal
        if self.selected_disk.is_none() {
            egui::CentralPanel::default().show(ctx, |_ui| {});
            ctx.style_mut(|s| s.visuals.window_shadow = egui::epaint::Shadow::NONE);
            egui::Window::new("Select Disk")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui: &mut egui::Ui| {
                    ui.label("Choose a disk to explore:");
                    ui.add_space(8.0);
                    let disk_paths: Vec<PathBuf> = self
                        .disks
                        .list()
                        .iter()
                        .map(|d| d.mount_point().to_path_buf())
                        .collect();
                    let mut chosen: Option<PathBuf> = None;
                    for path in &disk_paths {
                        if ui.button(path.display().to_string()).clicked() {
                            chosen = Some(path.clone());
                        }
                    }
                    if let Some(path) = chosen {
                        self.select_disk(path, ctx);
                    }
                });
            return;
        }

        // Top bar
        egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Disk:");
                let current_label = self
                    .selected_disk
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let disk_paths: Vec<PathBuf> = self
                    .disks
                    .list()
                    .iter()
                    .map(|d| d.mount_point().to_path_buf())
                    .collect();
                egui::ComboBox::from_id_source("disk_selector")
                    .selected_text(&current_label)
                    .show_ui(ui, |ui| {
                        let mut chosen: Option<PathBuf> = None;
                        for path in &disk_paths {
                            let label = path.display().to_string();
                            let selected = self.selected_disk.as_deref() == Some(path.as_path());
                            if ui.selectable_label(selected, &label).clicked() {
                                chosen = Some(path.clone());
                            }
                        }
                        if let Some(path) = chosen.clone() {
                            if Some(&path) != self.selected_disk.as_ref() {
                                self.select_disk(path, ctx);
                            }
                        }
                    });
                
                ui.separator();

                let available = 
                        self.selected_disk
                            .as_ref()
                            .and_then(|disk_path| {
                                self.disks
                                    .list()
                                    .iter()
                                    .find(|d| d.mount_point() == disk_path.as_path())
                                    .map(|d| d.available_space())
                            })
                            .unwrap_or(0);

                let total = self.selected_disk
                            .as_ref()
                            .and_then(|disk_path| {
                                self.disks
                                    .list()
                                    .iter()
                                    .find(|d| d.mount_point() == disk_path.as_path())
                                    .map(|d| {
                                        d.total_space()
                                    })
                            })
                            .unwrap_or(0);

                let used = (total as f64 - available as f64) / total as f64 * 100.0;

                ui.label(format!(
                    "{} free of {} ({:.2}% used)", format_size(available), format_size(total), used
                ));
                    
                let elapsed = self.scan_start.map(|ts| {
                    if let Some(te) = self.scan_end {
                        te.duration_since(ts).as_secs()
                    } else {
                        ts.elapsed().as_secs()
                    }
                }).unwrap_or(0);

                ui.separator();
                ui.label(if elapsed < 60 {
                    format!("{}s", elapsed)
                } else {
                    format!("{}m {}s", elapsed / 60, elapsed % 60)
                });
            });
        });

        // Progress bar (initial load only)
        if self.initial_loading {
            egui::TopBottomPanel::bottom("progress_panel").show(ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::ProgressBar::new(self.display_progress)
                            .text(&format!("Scanning... {:.0}%", self.display_progress * 100.0)),
                    );
                });
            });
            ctx.request_repaint();
        }

        // Central panel
        egui::CentralPanel::default().show(ctx, |ui| {
            let root = match &self.root_path {
                Some(p) => p.clone(),
                None => return,
            };

            let mut flat: Vec<(usize, Node)> = Vec::new();
            self.render_tree(&root, 0, &mut flat);

            let mut toggle_path: Option<PathBuf> = None;
            let spinner_width = 20.0;

            egui::ScrollArea::vertical()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                        for (depth, node) in &flat {
                            let indent = *depth as f32 * 16.0;
                            let size_pending =
                                node.is_dir && self.pending_sizes.contains(&node.path);
                            let dir_scanning = self.scans.contains_key(&node.path);
                            let show_spinner = size_pending || dir_scanning;

                            ui.horizontal(|ui| {
                                ui.add_space(indent);

                                if node.is_dir {
                                    let chevron = if self.expanded.contains(&node.path) {
                                        "▼"
                                    } else {
                                        "▶"
                                    };
                                    ui.label(egui::RichText::new(chevron).monospace());
                                } else {
                                    ui.label(egui::RichText::new(" ").monospace());
                                }

                                let icon = if node.is_dir { "📁" } else { "📄" };
                                let size_str = if size_pending {
                                    "...".to_string()
                                } else {
                                    format_size(node.size)
                                };
                                let label = format!("{} {}   {}", icon, node.name, size_str);

                                if ui.selectable_label(false, label).clicked() && node.is_dir {
                                    toggle_path = Some(node.path.clone());
                                }

                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(spinner_width, ui.spacing().interact_size.y),
                                    egui::Sense::hover(),
                                );
                                if show_spinner {
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "⏳",
                                        egui::FontId::proportional(12.0),
                                        ui.visuals().text_color(),
                                    );
                                }
                            });
                        }
                    });
                });

            if let Some(path) = toggle_path {
                self.toggle(path, ctx);
            }
        });
    }
}
