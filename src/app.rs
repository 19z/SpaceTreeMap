use crate::{
    format_size,
    model::{Index, NodeId, ROOT},
    scanner::{self, ScanProgress, Volume},
    shell, treemap,
};
use egui::{Align2, Color32, FontId, Rect, Sense, Stroke};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HWND;

struct ScanTask {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
    receiver: mpsc::Receiver<Result<Index, String>>,
    started: Instant,
}
struct Rename {
    path: PathBuf,
    name: String,
}
struct Tile {
    id: NodeId,
    rect: Rect,
    depth: usize,
}
#[derive(Clone, Copy)]
enum Action {
    Navigate(NodeId),
    Open(NodeId),
    Menu(NodeId),
}

pub struct SpaceTreeMapApp {
    volumes: Vec<Volume>,
    volume: usize,
    index: Option<Index>,
    current: NodeId,
    selected: NodeId,
    history: Vec<NodeId>,
    scan: Option<ScanTask>,
    error: Option<String>,
    status: String,
    stale: bool,
    hwnd: HWND,
    apartment: Option<shell::Apartment>,
    rename: Option<Rename>,
    depth: usize,
    filter: String,
    list: Vec<NodeId>,
    list_key: Option<(NodeId, String)>,
    tiles: Vec<Tile>,
    tile_key: Option<(NodeId, Rect, usize)>,
}

impl SpaceTreeMapApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        for path in [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simhei.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                let mut fonts = egui::FontDefinitions::default();
                fonts
                    .font_data
                    .insert("chinese".into(), egui::FontData::from_owned(bytes).into());
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .insert(0, "chinese".into());
                }
                cc.egui_ctx.set_fonts(fonts);
                break;
            }
        }
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let hwnd = cc
            .window_handle()
            .ok()
            .and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
                _ => None,
            })
            .unwrap_or_default();
        let apartment = shell::Apartment::new();
        let error = apartment.as_ref().err().cloned();
        let demo = std::env::args().any(|arg| arg == "--demo");
        Self {
            volumes: scanner::volumes(),
            volume: 0,
            index: demo.then(demo_index),
            current: ROOT,
            selected: ROOT,
            history: Vec::new(),
            scan: None,
            error,
            status: if demo {
                "演示数据 · 不对应磁盘上的真实路径".into()
            } else {
                "选择 NTFS 卷，开始读取 MFT".into()
            },
            stale: false,
            hwnd,
            apartment: apartment.ok(),
            rename: None,
            depth: 3,
            filter: String::new(),
            list: Vec::new(),
            list_key: None,
            tiles: Vec::new(),
            tile_key: None,
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let Some(volume) = self.volumes.get(self.volume) else {
            return;
        };
        let root = volume.root.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(ScanProgress::default()));
        let (sender, receiver) = mpsc::channel();
        let (worker_cancel, worker_progress, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        self.error = None;
        let spawned = std::thread::Builder::new()
            .name("mft-scan".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scanner::scan_volume(&root, &worker_cancel, |update| {
                        if let Ok(mut progress) = worker_progress.lock() {
                            *progress = update;
                        }
                        ctx.request_repaint();
                    })
                }))
                .unwrap_or_else(|_| Err("MFT 解析线程异常退出。可以重新扫描。".into()));
                let _ = sender.send(result);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.scan = Some(ScanTask {
                    cancel,
                    progress,
                    receiver,
                    started: Instant::now(),
                })
            }
            Err(e) => self.error = Some(format!("无法启动扫描线程：{e}")),
        }
    }

    fn poll(&mut self) {
        let result = self
            .scan
            .as_ref()
            .and_then(|scan| match scan.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("扫描线程已退出".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = result {
            let scan = self.scan.take().unwrap();
            match result {
                Ok(index) => {
                    self.status = format!("快照完成 · {:.1} 秒 · {} 个 MFT 记录 · {} 文件路径 · {} 目录 · {} 异常记录 · {} 无法还原路径",
                        scan.started.elapsed().as_secs_f64(), index.stats.records_read, index.stats.files,
                        index.stats.directories, index.stats.skipped_records, index.stats.orphaned);
                    self.index = Some(index);
                    self.current = ROOT;
                    self.selected = ROOT;
                    self.history.clear();
                    self.filter.clear();
                    self.list_key = None;
                    self.tile_key = None;
                    self.stale = false;
                }
                Err(e) => {
                    self.status = e.clone();
                    if !scan.cancel.load(Ordering::Relaxed) {
                        self.error = Some(e);
                    }
                }
            }
        }
    }

    fn navigate(&mut self, id: NodeId) {
        if id == self.current {
            return;
        }
        self.history.push(self.current);
        self.current = id;
        self.selected = id;
        self.filter.clear();
    }

    fn action(&mut self, action: Action) {
        let id = match action {
            Action::Navigate(id) | Action::Open(id) | Action::Menu(id) => id,
        };
        self.selected = id;
        if matches!(action, Action::Navigate(_)) {
            self.navigate(id);
            return;
        }
        if self.apartment.is_none() {
            self.error = Some("Shell COM 未初始化，无法操作路径".into());
            return;
        }
        let Some(path) = self.index.as_ref().and_then(|index| index.path(id)) else {
            self.error = Some("此记录无法还原为有效路径，不能执行文件操作。".into());
            return;
        };
        match action {
            Action::Open(_) => {
                if let Err(e) = shell::open(self.hwnd, &path) {
                    self.error = Some(e);
                }
            }
            Action::Menu(_) => match shell::context_menu(self.hwnd, &path) {
                Ok(shell::MenuOutcome::Rename) => {
                    self.rename = Some(Rename {
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        path,
                    })
                }
                Ok(shell::MenuOutcome::Invoked) => self.stale = true,
                Ok(shell::MenuOutcome::Cancelled) => {}
                Err(e) => self.error = Some(e),
            },
            _ => {}
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("SpaceTreeMap");
            ui.separator();
            ui.add_enabled_ui(self.scan.is_none(), |ui| {
                egui::ComboBox::from_id_salt("volume")
                    .width(220.0)
                    .selected_text(
                        self.volumes
                            .get(self.volume)
                            .map(|v| format!("{}  {}", v.root.display(), v.label))
                            .unwrap_or_else(|| "没有可用 NTFS 卷".into()),
                    )
                    .show_ui(ui, |ui| {
                        for (id, volume) in self.volumes.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.volume,
                                id,
                                format!("{}  {}", volume.root.display(), volume.label),
                            );
                        }
                    });
                if ui.button("扫描 / 刷新").clicked() {
                    self.start_scan(ui.ctx());
                }
                if ui.button("重载卷列表").clicked() {
                    self.volumes = scanner::volumes();
                    self.volume = 0;
                }
            });
            if let Some(scan) = &self.scan {
                if ui
                    .add_enabled(
                        !scan.cancel.load(Ordering::Relaxed),
                        egui::Button::new("取消扫描"),
                    )
                    .clicked()
                {
                    scan.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        if let Some(scan) = &self.scan {
            let progress = scan.progress.lock().map(|p| p.clone()).unwrap_or_default();
            let fraction = if progress.total == 0 {
                0.0
            } else {
                progress.read as f32 / progress.total as f32
            };
            ui.add(egui::ProgressBar::new(fraction).animate(true).text(format!(
                "{} · {}/{} 记录 · {} 活动记录 · {:.1}s{}",
                progress.phase,
                progress.read,
                progress.total,
                progress.active,
                scan.started.elapsed().as_secs_f64(),
                if scan.cancel.load(Ordering::Relaxed) {
                    " · 正在取消…"
                } else {
                    ""
                }
            )));
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if let Some(volume) = self.volumes.get(self.volume) {
            ui.small(format!(
                "卷容量 {}  ·  可用 {}  ·  逻辑大小计量  ·  硬链接按路径计入",
                format_size(volume.total),
                format_size(volume.free)
            ));
        }
        if self.stale {
            ui.colored_label(
                Color32::YELLOW,
                "已调用 Shell 操作。图表仍为扫描快照，请手动刷新以反映变化。",
            );
        }
        if let Some(error) = self.error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(Color32::from_rgb(255, 135, 120), error);
                if ui.small_button("关闭提示").clicked() {
                    self.error = None;
                }
            });
        }
        ui.separator();
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        let Some(index) = &self.index else {
            ui.vertical_centered(|ui| {
                ui.add_space(110.0);
                ui.heading("从 MFT 直接建立磁盘空间图");
                ui.label("选择一个 NTFS 卷，然后点击“扫描 / 刷新”。");
                ui.label(
                    "双击目录进入 · 滚轮向上进入指针下目录，向下返回 · 右击打开 Windows 原生菜单",
                );
                ui.weak("扫描在后台运行，可以取消；文件变化后手动刷新。");
            });
            return;
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.history.is_empty(), egui::Button::new("← 后退"))
                .clicked()
            {
                if let Some(id) = self.history.pop() {
                    self.current = id;
                    self.selected = id;
                    self.filter.clear();
                }
            }
            if ui
                .add_enabled(self.current != ROOT, egui::Button::new("↑ 上一级"))
                .clicked()
            {
                action = index.nodes[self.current].parent.map(Action::Navigate);
            }
            for id in index.breadcrumbs(self.current) {
                if ui
                    .selectable_label(id == self.current, index.nodes[id].name.to_string_lossy())
                    .clicked()
                {
                    action = Some(Action::Navigate(id));
                }
                ui.weak("/");
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!(
                "当前目录 {} · {} 个直接子项",
                format_size(index.nodes[self.current].size),
                index.nodes[self.current].children.len()
            ));
            ui.separator();
            ui.label("展开层数");
            ui.add(egui::Slider::new(&mut self.depth, 1..=6));
        });
        let height = (ui.available_height() - 64.0).max(100.0);
        ui.horizontal_top(|ui| {
            let list_width = (ui.available_width() * 0.25).clamp(210.0, 340.0);
            ui.allocate_ui_with_layout(
                egui::vec2(list_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(list_width);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("筛选当前目录…")
                            .desired_width(list_width),
                    );
                    let key = (self.current, self.filter.clone());
                    if self.list_key.as_ref() != Some(&key) {
                        let filter = self.filter.to_lowercase();
                        self.list = index.nodes[self.current]
                            .children
                            .iter()
                            .copied()
                            .filter(|id| {
                                index.nodes[*id]
                                    .name
                                    .to_string_lossy()
                                    .to_lowercase()
                                    .contains(&filter)
                            })
                            .collect();
                        self.list_key = Some(key);
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("files")
                        .max_height(height - 35.0)
                        .show_rows(ui, 27.0, self.list.len(), |ui, range| {
                            for row in range {
                                let id = self.list[row];
                                let node = &index.nodes[id];
                                let text = format!(
                                    "{} {}  ·  {}",
                                    if node.is_dir { "▸" } else { "·" },
                                    node.name.to_string_lossy(),
                                    format_size(node.size)
                                );
                                let response = ui
                                    .add_sized(
                                        [list_width - 12.0, 25.0],
                                        egui::Button::new(text).selected(self.selected == id),
                                    )
                                    .on_hover_text(
                                        index
                                            .path(id)
                                            .map(|p| p.display().to_string())
                                            .unwrap_or_else(|| "无法还原路径".into()),
                                    );
                                if response.clicked() {
                                    self.selected = id;
                                }
                                if response.double_clicked() {
                                    action = Some(if node.is_dir {
                                        Action::Navigate(id)
                                    } else {
                                        Action::Open(id)
                                    });
                                }
                                if response.secondary_clicked() {
                                    action = Some(Action::Menu(id));
                                }
                            }
                        });
                },
            );
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
            ui.painter()
                .rect_filled(rect, 3.0, Color32::from_rgb(14, 19, 27));
            let key = (self.current, rect, self.depth);
            if self.tile_key != Some(key) {
                self.tiles.clear();
                make_tiles(
                    index,
                    self.current,
                    rect.shrink(4.0),
                    0,
                    self.depth,
                    &mut self.tiles,
                );
                self.tile_key = Some(key);
            }
            let pointer = response.hover_pos();
            let hovered = pointer.and_then(|p| {
                self.tiles
                    .iter()
                    .rev()
                    .find(|t| t.rect.contains(p))
                    .map(|t| t.id)
            });
            let painter = ui.painter().with_clip_rect(rect);
            for tile in &self.tiles {
                let node = &index.nodes[tile.id];
                let color = tile_color(node, tile.depth);
                painter.rect_filled(tile.rect.shrink(0.8), 2.0, color);
                if tile.rect.width() > 44.0 && tile.rect.height() > 17.0 {
                    let label_rect = Rect::from_min_max(
                        tile.rect.min + egui::vec2(5.0, 2.0),
                        tile.rect.max - egui::vec2(3.0, 1.0),
                    );
                    let text = if tile.rect.height() > 44.0 && !node.is_dir {
                        format!(
                            "{}\n{}",
                            node.name.to_string_lossy(),
                            format_size(node.size)
                        )
                    } else {
                        node.name.to_string_lossy().into_owned()
                    };
                    painter.with_clip_rect(label_rect).text(
                        label_rect.min,
                        Align2::LEFT_TOP,
                        text,
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                }
            }
            for tile in &self.tiles {
                if self.selected == tile.id || hovered == Some(tile.id) {
                    painter.rect_stroke(
                        tile.rect.shrink(1.0),
                        2.0,
                        Stroke::new(
                            2.0,
                            if hovered == Some(tile.id) {
                                Color32::WHITE
                            } else {
                                Color32::from_rgb(255, 211, 98)
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            if self.tiles.is_empty() {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "此目录没有占用逻辑空间的子项\n空目录和零字节文件可从左侧列表访问",
                    FontId::proportional(17.0),
                    Color32::GRAY,
                );
            }
            let hit = hovered.unwrap_or(self.current);
            if response.clicked() {
                self.selected = hit;
            }
            if response.double_clicked() {
                action = Some(if index.nodes[hit].is_dir {
                    Action::Navigate(hit)
                } else {
                    Action::Open(hit)
                });
            }
            if response.secondary_clicked() {
                action = Some(Action::Menu(hit));
            }
            if response.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll > 0.0 {
                    let directory = if index.nodes[hit].is_dir {
                        Some(hit)
                    } else {
                        index.nodes[hit].parent
                    };
                    action = directory
                        .filter(|id| *id != self.current)
                        .map(Action::Navigate)
                        .or(action);
                } else if scroll < 0.0 {
                    action = index.nodes[self.current]
                        .parent
                        .map(Action::Navigate)
                        .or(action);
                }
            }
            if let Some(id) = hovered {
                response.on_hover_ui(|ui| {
                    ui.label(index.nodes[id].name.to_string_lossy());
                    ui.label(format!(
                        "{} · {} 字节",
                        format_size(index.nodes[id].size),
                        index.nodes[id].size
                    ));
                    ui.weak(
                        index
                            .path(id)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "无法还原路径".into()),
                    );
                });
            }
        });
        let selected = &index.nodes[self.selected];
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} · {}",
                selected.name.to_string_lossy(),
                format_size(selected.size)
            ));
            ui.weak(
                index
                    .path(self.selected)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "无法还原路径".into()),
            );
            if ui.small_button("右键菜单").clicked() {
                action = Some(Action::Menu(self.selected));
            }
        });
        if let Some(action) = action {
            self.action(action);
        }
    }
}

impl eframe::App for SpaceTreeMapApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.toolbar(ui);
        self.content(ui);
        ui.separator();
        ui.small(&self.status);
        if let Some(rename) = &mut self.rename {
            let (mut save, mut close) = (false, false);
            egui::Window::new("重命名")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(rename.path.display().to_string());
                    ui.text_edit_singleline(&mut rename.name);
                    ui.horizontal(|ui| {
                        save = ui.button("确定").clicked();
                        close = ui.button("取消").clicked();
                    });
                });
            if save {
                match shell::rename(&rename.path, &rename.name) {
                    Ok(()) => {
                        self.stale = true;
                        close = true;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if close {
                self.rename = None;
            }
        }
    }
}

impl Drop for SpaceTreeMapApp {
    fn drop(&mut self) {
        if let Some(scan) = &self.scan {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn make_tiles(
    index: &Index,
    parent: NodeId,
    rect: Rect,
    depth: usize,
    limit: usize,
    out: &mut Vec<Tile>,
) {
    if depth >= limit || rect.width() < 4.0 || rect.height() < 4.0 || out.len() >= 18000 {
        return;
    }
    let items: Vec<_> = index.nodes[parent]
        .children
        .iter()
        .map(|id| (*id, index.nodes[*id].size))
        .collect();
    for (id, rect) in treemap::layout(&items, rect) {
        if rect.width() < 1.0 || rect.height() < 1.0 {
            continue;
        }
        if out.len() >= 18000 {
            break;
        }
        out.push(Tile { id, rect, depth });
        if index.nodes[id].is_dir && rect.height() > 40.0 && rect.width() > 50.0 {
            let content = Rect::from_min_max(
                rect.min + egui::vec2(3.0, 22.0),
                rect.max - egui::vec2(3.0, 3.0),
            );
            make_tiles(index, id, content, depth + 1, limit, out);
        }
    }
}

fn tile_color(node: &crate::model::Node, depth: usize) -> Color32 {
    if node.is_dir {
        return Color32::from_rgb(
            36 + (depth * 9) as u8,
            65 + (depth * 9) as u8,
            83 + (depth * 9) as u8,
        );
    }
    let extension = std::path::Path::new(&node.name)
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let hash = extension.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as u32)
    });
    let palette = [
        [49, 117, 157],
        [72, 128, 108],
        [147, 103, 57],
        [119, 91, 153],
        [148, 79, 95],
        [66, 128, 139],
        [109, 119, 60],
    ];
    let rgb = palette[hash as usize % palette.len()];
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

fn demo_index() -> Index {
    use crate::model::{FileRecord, LinkName, Statistics};
    use std::collections::HashMap;
    let mut records = HashMap::new();
    for (id, parent, name, dir, size) in [
        (5, 5, ".", true, 0),
        (10, 5, "开发项目", true, 0),
        (11, 5, "图片与视频", true, 0),
        (12, 5, "文档", true, 0),
        (13, 5, "系统备份.vhdx", false, 14_000_000_000),
        (14, 10, "Rust", true, 0),
        (15, 14, "target", true, 0),
        (16, 15, "应用构建产物", false, 5_000_000_000),
        (17, 14, "源代码", false, 1_000_000_000),
        (18, 10, "Web", true, 0),
        (19, 18, "node_modules", false, 3_000_000_000),
        (20, 11, "旅行影片.mp4", false, 7_000_000_000),
        (21, 11, "照片归档.zip", false, 4_000_000_000),
        (22, 12, "设计文档.pdf", false, 600_000_000),
        (23, 12, "零字节.txt", false, 0),
        (24, 12, "空目录", true, 0),
    ] {
        records.insert(
            id,
            FileRecord {
                sequence: 1,
                is_dir: dir,
                logical_size: size,
                names: vec![LinkName {
                    parent: mft::MftReference::new(parent, 1),
                    name: name.into(),
                    dos: false,
                }],
            },
        );
    }
    let mut index = Index::build(
        "演示卷\\".into(),
        records,
        Statistics::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    for node in &mut index.nodes {
        node.valid_path = false;
    }
    index
}
