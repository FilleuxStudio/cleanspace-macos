//! Interface graphique de CleanSpace, en trois onglets :
//!
//! - **Désinstaller** : on choisit une application (liste ou saisie), on analyse,
//!   on coche/décoche ce qu'on veut garder, puis on confirme la suppression.
//! - **Nettoyage** : on scanne les fichiers inutiles (caches, journaux,
//!   corbeille, caches de développement) et on lance le nettoyage.
//! - **Système** : modèle, version de macOS, stockage, mémoire et batterie.
//!
//! Les scans et suppressions tournent dans des threads séparés pour ne jamais
//! geler l'UI.

use clean_uninstall_core::{
    analyze, clean_junk, disk_size, home_dir, junk_specs, list_installed_apps, perform_removal,
    scan_junk_category, sysinfo, FoundItem, InstalledApp, JunkCategory, RemovalStatus,
};
use eframe::egui::{self, Align, Color32, Layout, RichText, Stroke};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

const ORANGE: Color32 = Color32::from_rgb(200, 130, 0);
const GREEN: Color32 = Color32::from_rgb(60, 160, 60);
const RED: Color32 = Color32::from_rgb(200, 50, 50);
const BLUE: Color32 = Color32::from_rgb(40, 120, 220);
/// Au-dessus de ce seuil, la taille est mise en évidence dans les listes.
const BIG_ITEM_BYTES: u64 = 100_000_000;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CleanSpace")
            .with_inner_size([980.0, 700.0])
            .with_min_inner_size([760.0, 480.0])
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "CleanSpace",
        options,
        Box::new(|cc| {
            // Police un peu plus grande pour la lisibilité.
            let mut style = (*cc.egui_ctx.style()).clone();
            for (_, font_id) in style.text_styles.iter_mut() {
                font_id.size *= 1.05;
            }
            cc.egui_ctx.set_style(style);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
}

// ---------------------------------------------------------------------------
// Utilitaires
// ---------------------------------------------------------------------------

const ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");
const ICON_SMALL_PNG: &[u8] = include_bytes!("../../assets/icon-64.png");

/// Icône de la fenêtre (Dock / barre des tâches). Icône vide si le PNG est illisible.
fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(ICON_PNG).unwrap_or_default()
}

/// Logo de l'en-tête, chargé en texture.
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let icon = eframe::icon_data::from_png_bytes(ICON_SMALL_PNG).ok()?;
    let img = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    Some(ctx.load_texture("logo", img, egui::TextureOptions::LINEAR))
}

/// Vert → orange → rouge selon le taux d'occupation (0.0 – 1.0).
fn usage_color(frac: f32) -> Color32 {
    if frac >= 0.9 {
        RED
    } else if frac >= 0.75 {
        ORANGE
    } else {
        GREEN
    }
}

/// Barre de jauge pleine largeur.
fn gauge(ui: &mut egui::Ui, frac: f32, color: Color32) {
    ui.add(
        egui::ProgressBar::new(frac.clamp(0.0, 1.0))
            .fill(color)
            .desired_height(14.0),
    );
}

/// Encadré avec titre, pour regrouper un bloc d'informations.
fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(15.0));
        ui.add_space(6.0);
        add(ui);
    });
    ui.add_space(8.0);
}

/// Ligne « libellé : valeur » d'une grille.
fn row(ui: &mut egui::Ui, label: &str, value: String) {
    ui.label(RichText::new(label).weak());
    ui.label(value);
    ui.end_row();
}

/// Format façon Finder (unités décimales : 1 Go = 1000 Mo).
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    if bytes < 1000 {
        return format!("{} o", bytes);
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{:.1} {}", value, UNITS[unit])
}

/// Remplace le dossier personnel par `~` pour raccourcir l'affichage.
fn pretty_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Bilan d'une suppression : (octets réellement libérés, éléments restants).
/// On regarde ce qui a réellement disparu du disque.
fn measure_removal(selected: &[(PathBuf, u64)]) -> (u64, usize) {
    let mut freed = 0;
    let mut failed = 0;
    for (path, size) in selected {
        if std::fs::symlink_metadata(path).is_err() {
            freed += size;
        } else {
            failed += 1;
        }
    }
    (freed, failed)
}

fn status_color(status: RemovalStatus) -> Color32 {
    match status {
        RemovalStatus::Removable => GREEN,
        RemovalStatus::NeedsAdmin => ORANGE,
        RemovalStatus::Protected => RED,
    }
}

fn status_short(status: RemovalStatus) -> &'static str {
    match status {
        RemovalStatus::Removable => "Libre",
        RemovalStatus::NeedsAdmin => "Admin",
        RemovalStatus::Protected => "Protégée",
    }
}

fn status_tooltip(status: RemovalStatus) -> &'static str {
    match status {
        RemovalStatus::Removable => "Peut être supprimée sans droits particuliers",
        RemovalStatus::NeedsAdmin => "Peut être supprimée : une autorisation administrateur sera demandée",
        RemovalStatus::Protected => "Protégée par macOS : ne peut pas être supprimée",
    }
}

/// Petite icône ronde dessinée à la main (pas d'emoji, donc pas de risque de
/// glyphe manquant) : coche verte, cadenas orange, barre rouge ; un point bleu
/// en coin indique que l'application est en cours d'exécution.
fn status_icon(ui: &mut egui::Ui, status: RemovalStatus, running: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    let c = rect.center();
    let white = Color32::WHITE;

    painter.circle_filled(c, 7.0, status_color(status));
    match status {
        RemovalStatus::Removable => {
            let s = Stroke::new(1.7_f32, white);
            painter.line_segment([c + egui::vec2(-3.2, 0.4), c + egui::vec2(-1.0, 2.8)], s);
            painter.line_segment([c + egui::vec2(-1.0, 2.8), c + egui::vec2(3.2, -2.6)], s);
        }
        RemovalStatus::NeedsAdmin => {
            // Cadenas : corps plein + anse.
            painter.rect_filled(
                egui::Rect::from_center_size(c + egui::vec2(0.0, 1.8), egui::vec2(7.0, 5.0)),
                1.0,
                white,
            );
            let s = Stroke::new(1.3_f32, white);
            painter.line_segment([c + egui::vec2(-2.0, -0.6), c + egui::vec2(-2.0, -2.6)], s);
            painter.line_segment([c + egui::vec2(-2.0, -2.6), c + egui::vec2(2.0, -2.6)], s);
            painter.line_segment([c + egui::vec2(2.0, -2.6), c + egui::vec2(2.0, -0.6)], s);
        }
        RemovalStatus::Protected => {
            painter.line_segment(
                [c + egui::vec2(-3.6, 0.0), c + egui::vec2(3.6, 0.0)],
                Stroke::new(2.3_f32, white),
            );
        }
    }
    if running {
        let dot = rect.right_top() + egui::vec2(-2.5, 2.5);
        painter.circle_filled(dot, 3.4, BLUE);
        painter.circle_stroke(dot, 3.4, Stroke::new(1.0_f32, white));
    }

    let mut tip = status_tooltip(status).to_string();
    if running {
        tip.push_str("\nEn cours d'exécution : ses processus seront fermés avant la suppression.");
    }
    response.on_hover_text(tip)
}

/// Petit point bleu de la légende (application en cours d'exécution).
fn running_dot(ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(12.0, 16.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.6, BLUE);
    response.on_hover_text("Application en cours d'exécution");
}

// ---------------------------------------------------------------------------
// État de l'application
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Uninstall,
    Clean,
    System,
}

/// Un élément affiché dans la liste de désinstallation, avec sa case à cocher.
struct Item {
    path: PathBuf,
    display: String,
    needs_sudo: bool,
    selected: bool,
    /// Non cochable (le bundle d'une application protégée par macOS).
    locked: bool,
    size: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Scanning,
    Ready,
    Removing,
    Done,
}

/// Messages envoyés depuis les threads de fond vers l'UI (désinstallation).
enum WorkerMsg {
    ScanDone {
        app_path: Option<PathBuf>,
        app_status: Option<RemovalStatus>,
        bundle_id: Option<String>,
        items: Vec<(FoundItem, u64)>,
    },
    Log(String),
    RemoveDone {
        freed: u64,
        failed: usize,
    },
}

/// Un élément proposé par le scanner de nettoyage.
struct JunkEntry {
    path: PathBuf,
    display: String,
    size: u64,
    selected: bool,
}

/// Une catégorie du scanner de nettoyage (caches, journaux, …).
struct JunkGroup {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    note: Option<String>,
    entries: Vec<JunkEntry>,
}

impl JunkGroup {
    fn from_category(cat: JunkCategory) -> Self {
        Self {
            id: cat.id,
            name: cat.name,
            description: cat.description,
            note: cat.note,
            entries: cat
                .items
                .into_iter()
                .map(|i| JunkEntry {
                    display: pretty_path(&i.path),
                    path: i.path,
                    size: i.size,
                    selected: i.selected_by_default,
                })
                .collect(),
        }
    }

    fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CleanPhase {
    Idle,
    Scanning,
    Ready,
    Cleaning,
    Done,
}

/// Messages envoyés depuis les threads de fond vers l'UI (nettoyage).
enum CleanMsg {
    Progress(String),
    Category(JunkCategory),
    ScanDone,
    Log(String),
    CleanDone { freed: u64, failed: usize },
}

struct App {
    tab: Tab,

    // --- Onglet Désinstaller ---
    /// Texte du champ de saisie.
    app_name: String,
    /// Texte réellement analysé lors du dernier scan.
    scanned_query: String,
    /// Nom affiché dans les journaux (nom du .app trouvé, sinon la saisie analysée).
    removal_name: String,
    phase: Phase,
    app_path: Option<PathBuf>,
    app_status: Option<RemovalStatus>,
    app_running: bool,
    bundle_id: Option<String>,
    items: Vec<Item>,
    logs: Vec<String>,
    confirm_checked: bool,
    keep_receipts: bool,
    /// (octets libérés, nombre d'éléments non supprimés) après une suppression.
    summary: Option<(u64, usize)>,
    rx: Option<Receiver<WorkerMsg>>,

    // Liste des applications installées, chargée en tâche de fond.
    installed_apps: Vec<InstalledApp>,
    apps_loading: bool,
    apps_rx: Option<Receiver<Vec<InstalledApp>>>,
    app_filter: String,
    selected_path: Option<PathBuf>,

    // --- Onglet Nettoyage ---
    clean_phase: CleanPhase,
    groups: Vec<JunkGroup>,
    clean_progress: String,
    clean_logs: Vec<String>,
    clean_confirm: bool,
    clean_summary: Option<(u64, usize)>,
    clean_rx: Option<Receiver<CleanMsg>>,

    // --- Onglet Système ---
    logo: Option<egui::TextureHandle>,
    sys: Option<sysinfo::SystemInfo>,
    sys_loading: bool,
    sys_rx: Option<Receiver<sysinfo::SystemInfo>>,
    sys_last: Option<std::time::Instant>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let mut app = Self {
            tab: Tab::Uninstall,
            app_name: String::new(),
            scanned_query: String::new(),
            removal_name: String::new(),
            phase: Phase::Idle,
            app_path: None,
            app_status: None,
            app_running: false,
            bundle_id: None,
            items: Vec::new(),
            logs: Vec::new(),
            confirm_checked: false,
            keep_receipts: false,
            summary: None,
            rx: None,
            installed_apps: Vec::new(),
            apps_loading: false,
            apps_rx: None,
            app_filter: String::new(),
            selected_path: None,
            clean_phase: CleanPhase::Idle,
            groups: Vec::new(),
            clean_progress: String::new(),
            clean_logs: Vec::new(),
            clean_confirm: false,
            clean_summary: None,
            clean_rx: None,
            logo: load_logo(ctx),
            sys: None,
            sys_loading: false,
            sys_rx: None,
            sys_last: None,
        };
        app.reload_apps();
        app
    }

    /// Relance, dans un thread séparé, le scan des applications installées.
    fn reload_apps(&mut self) {
        let (tx, rx) = channel();
        self.apps_rx = Some(rx);
        self.apps_loading = true;
        thread::spawn(move || {
            let _ = tx.send(list_installed_apps());
        });
    }

    // ----- Désinstallation -----

    fn start_scan(&mut self) {
        let name = self.app_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.phase = Phase::Scanning;
        self.scanned_query = name.clone();
        self.items.clear();
        self.logs.clear();
        self.summary = None;
        self.confirm_checked = false;

        let (tx, rx) = channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let result = analyze(&name, 4);
            // Les tailles sont calculées ici, hors du thread UI.
            let mut items: Vec<(FoundItem, u64)> = result
                .items
                .into_iter()
                .map(|f| {
                    let size = disk_size(&f.path);
                    (f, size)
                })
                .collect();
            // Les plus gros en premier : c'est là qu'on gagne de la place.
            items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.path.cmp(&b.0.path)));
            let _ = tx.send(WorkerMsg::ScanDone {
                app_path: result.app_path,
                app_status: result.app_status,
                bundle_id: result.bundle_id,
                items,
            });
        });
    }

    fn start_removal(&mut self) {
        let selected: Vec<(PathBuf, u64)> = self
            .items
            .iter()
            .filter(|i| i.selected)
            .map(|i| (i.path.clone(), i.size))
            .collect();
        if selected.is_empty() {
            return;
        }
        self.phase = Phase::Removing;
        self.logs.clear();
        self.summary = None;

        let (tx, rx) = channel();
        self.rx = Some(rx);
        let name = self.removal_name.clone();
        let bundle_id = self.bundle_id.clone();
        let keep_receipts = self.keep_receipts;

        thread::spawn(move || {
            let paths: Vec<PathBuf> = selected.iter().map(|(p, _)| p.clone()).collect();
            let tx_log = tx.clone();
            let mut log_fn = move |line: String| {
                let _ = tx_log.send(WorkerMsg::Log(line));
            };
            perform_removal(&name, bundle_id.as_deref(), &paths, keep_receipts, &mut log_fn);

            let (freed, failed) = measure_removal(&selected);
            let _ = tx.send(WorkerMsg::RemoveDone { freed, failed });
        });
    }

    /// Récupère le résultat du scan des applications installées. Non bloquant.
    fn poll_apps_list(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.apps_rx {
            match rx.try_recv() {
                Ok(apps) => {
                    self.installed_apps = apps;
                    self.apps_loading = false;
                    self.apps_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.apps_loading = false;
                    self.apps_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.apps_loading {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut scan_done = None;
        let mut removal_done = None;
        let mut disconnected = false;

        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(WorkerMsg::ScanDone { app_path, app_status, bundle_id, items }) => {
                        scan_done = Some((app_path, app_status, bundle_id, items));
                    }
                    Ok(WorkerMsg::Log(line)) => self.logs.push(line),
                    Ok(WorkerMsg::RemoveDone { freed, failed }) => {
                        removal_done = Some((freed, failed));
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if let Some((app_path, app_status, bundle_id, items)) = scan_done {
            let protected = app_status == Some(RemovalStatus::Protected);
            // Sans .app trouvé, on n'a que des correspondances par nom ; pour une
            // application protégée, on ne touche à rien sans choix explicite :
            // dans les deux cas rien n'est coché par défaut.
            let preselect = app_path.is_some() && !protected;
            let locked_path = if protected { app_path.clone() } else { None };

            self.removal_name = app_path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| self.scanned_query.clone());
            self.app_running = self
                .installed_apps
                .iter()
                .any(|a| Some(&a.path) == app_path.as_ref() && a.running);
            self.selected_path = app_path.clone();
            self.app_path = app_path;
            self.app_status = app_status;
            self.bundle_id = bundle_id;
            self.items = items
                .into_iter()
                .map(|(f, size)| Item {
                    display: pretty_path(&f.path),
                    needs_sudo: f.needs_sudo,
                    locked: locked_path.as_ref() == Some(&f.path),
                    selected: preselect,
                    size,
                    path: f.path,
                })
                .collect();
            self.phase = Phase::Ready;
            self.rx = None;
        }

        if let Some((freed, failed)) = removal_done {
            // On ne garde dans la liste que ce qui existe encore (échecs).
            self.items.retain(|i| std::fs::symlink_metadata(&i.path).is_ok());
            for item in &mut self.items {
                item.selected = !item.locked;
            }
            self.summary = Some((freed, failed));
            self.confirm_checked = false;
            self.selected_path = None;
            self.phase = Phase::Done;
            self.rx = None;
            self.reload_apps();
        }

        if disconnected {
            // Le thread s'est arrêté sans message final (panique) : on ne reste
            // pas bloqué sur un spinner.
            match self.phase {
                Phase::Scanning => self.phase = Phase::Idle,
                Phase::Removing => {
                    self.summary = Some((0, self.items.len()));
                    self.phase = Phase::Done;
                }
                _ => {}
            }
            self.rx = None;
        }

        // Tant qu'un thread de fond tourne, on redemande un rafraîchissement.
        if matches!(self.phase, Phase::Scanning | Phase::Removing) {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    // ----- Nettoyage -----

    fn start_clean_scan(&mut self) {
        self.clean_phase = CleanPhase::Scanning;
        self.groups.clear();
        self.clean_logs.clear();
        self.clean_summary = None;
        self.clean_confirm = false;
        self.clean_progress.clear();

        let (tx, rx) = channel();
        self.clean_rx = Some(rx);
        thread::spawn(move || {
            for spec in junk_specs() {
                let _ = tx.send(CleanMsg::Progress(spec.name.to_string()));
                let category = scan_junk_category(&spec);
                let _ = tx.send(CleanMsg::Category(category));
            }
            let _ = tx.send(CleanMsg::ScanDone);
        });
    }

    fn start_clean(&mut self) {
        let selected: Vec<(PathBuf, u64)> = self
            .groups
            .iter()
            .flat_map(|g| g.entries.iter())
            .filter(|e| e.selected)
            .map(|e| (e.path.clone(), e.size))
            .collect();
        if selected.is_empty() {
            return;
        }
        self.clean_phase = CleanPhase::Cleaning;
        self.clean_logs.clear();
        self.clean_summary = None;

        let (tx, rx) = channel();
        self.clean_rx = Some(rx);
        thread::spawn(move || {
            let paths: Vec<PathBuf> = selected.iter().map(|(p, _)| p.clone()).collect();
            let tx_log = tx.clone();
            let mut log_fn = move |line: String| {
                let _ = tx_log.send(CleanMsg::Log(line));
            };
            clean_junk(&paths, &mut log_fn);

            let (freed, failed) = measure_removal(&selected);
            let _ = tx.send(CleanMsg::CleanDone { freed, failed });
        });
    }

    fn poll_clean(&mut self, ctx: &egui::Context) {
        let mut scan_done = false;
        let mut clean_done = None;
        let mut disconnected = false;

        if let Some(rx) = &self.clean_rx {
            loop {
                match rx.try_recv() {
                    Ok(CleanMsg::Progress(name)) => self.clean_progress = name,
                    Ok(CleanMsg::Category(cat)) => {
                        if !cat.items.is_empty() || cat.note.is_some() {
                            self.groups.push(JunkGroup::from_category(cat));
                        }
                    }
                    Ok(CleanMsg::ScanDone) => scan_done = true,
                    Ok(CleanMsg::Log(line)) => self.clean_logs.push(line),
                    Ok(CleanMsg::CleanDone { freed, failed }) => clean_done = Some((freed, failed)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if scan_done {
            self.clean_phase = CleanPhase::Ready;
            self.clean_rx = None;
        }

        if let Some((freed, failed)) = clean_done {
            // On ne garde que ce qui existe encore (échecs) et les catégories non vides.
            for group in &mut self.groups {
                group.entries.retain(|e| std::fs::symlink_metadata(&e.path).is_ok());
                for entry in &mut group.entries {
                    entry.selected = true;
                }
            }
            self.groups.retain(|g| !g.entries.is_empty() || g.note.is_some());
            self.clean_summary = Some((freed, failed));
            self.clean_confirm = false;
            self.clean_phase = CleanPhase::Done;
            self.clean_rx = None;
        }

        if disconnected {
            match self.clean_phase {
                CleanPhase::Scanning => {
                    self.clean_phase =
                        if self.groups.is_empty() { CleanPhase::Idle } else { CleanPhase::Ready };
                }
                CleanPhase::Cleaning => {
                    let count = self.groups.iter().map(|g| g.entries.len()).sum();
                    self.clean_summary = Some((0, count));
                    self.clean_phase = CleanPhase::Done;
                }
                _ => {}
            }
            self.clean_rx = None;
        }

        if matches!(self.clean_phase, CleanPhase::Scanning | CleanPhase::Cleaning) {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }
}

// ---------------------------------------------------------------------------
// Interface
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker(ctx);
        self.poll_apps_list(ctx);
        self.poll_clean(ctx);
        self.poll_system(ctx);

        self.ui_tabs(ctx);
        match self.tab {
            Tab::Uninstall => self.ui_uninstall(ctx),
            Tab::Clean => self.ui_clean(ctx),
            Tab::System => self.ui_system(ctx),
        }
    }
}

impl App {
    /// Bandeau du haut : titre et onglets.
    fn ui_tabs(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Some(logo) = &self.logo {
                    ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(28.0, 28.0)));
                }
                ui.heading("CleanSpace");
                ui.add_space(24.0);
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Uninstall,
                    RichText::new("Désinstaller").size(15.0),
                );
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Clean,
                    RichText::new("Nettoyage").size(15.0),
                );
                ui.selectable_value(
                    &mut self.tab,
                    Tab::System,
                    RichText::new("Système").size(15.0),
                );
            });
            ui.add_space(6.0);
        });
    }

    // ===================== Onglet « Système » =====================

    /// Lance la collecte des infos système dans un thread (system_profiler est lent).
    fn start_system(&mut self) {
        if self.sys_loading {
            return;
        }
        self.sys_loading = true;
        let (tx, rx) = channel();
        self.sys_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(sysinfo::collect());
        });
    }

    fn poll_system(&mut self, ctx: &egui::Context) {
        // Premier affichage de l'onglet, puis rafraîchissement automatique toutes les 10 s.
        if self.tab == Tab::System && !self.sys_loading {
            let stale = self.sys_last.map_or(true, |t| t.elapsed() >= Duration::from_secs(10));
            if stale {
                self.start_system();
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
        if let Some(rx) = &self.sys_rx {
            match rx.try_recv() {
                Ok(info) => {
                    self.sys = Some(info);
                    self.sys_loading = false;
                    self.sys_last = Some(std::time::Instant::now());
                    self.sys_rx = None;
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(80)),
                Err(TryRecvError::Disconnected) => {
                    self.sys_loading = false;
                    self.sys_last = Some(std::time::Instant::now());
                    self.sys_rx = None;
                }
            }
        }
    }

    fn ui_system(&mut self, ctx: &egui::Context) {
        let mut refresh = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Informations système");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(!self.sys_loading, egui::Button::new("⟳  Actualiser"))
                        .clicked()
                    {
                        refresh = true;
                    }
                    if self.sys_loading {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(8.0);

            let Some(info) = self.sys.clone() else {
                ui.add_space(20.0);
                ui.label("Lecture des informations…");
                return;
            };

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let dash = || "—".to_string();

                // --- Mac ---
                card(ui, "Mac", |ui| {
                    egui::Grid::new("sys_mac").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                        row(ui, "Modèle", info.model_name.clone().unwrap_or_else(dash));
                        row(ui, "Identifiant", info.model_id.clone().unwrap_or_else(dash));
                        row(ui, "Puce / processeur", info.chip.clone().unwrap_or_else(dash));
                        row(
                            ui,
                            "Cœurs",
                            info.cores.map(|c| c.to_string()).unwrap_or_else(dash),
                        );
                        let os = match (&info.macos_name, &info.macos_version) {
                            (Some(n), Some(v)) => format!("{} {}", n, v),
                            (None, Some(v)) => format!("macOS {}", v),
                            _ => dash(),
                        };
                        row(ui, "Système", os);
                        row(ui, "Build", info.macos_build.clone().unwrap_or_else(dash));
                        row(
                            ui,
                            "Allumé depuis",
                            info.uptime_secs.map(sysinfo::format_uptime).unwrap_or_else(dash),
                        );
                    });
                });

                // --- Stockage ---
                card(ui, "Stockage", |ui| match &info.storage {
                    Some(st) => {
                        let frac = st.used() as f32 / st.total.max(1) as f32;
                        gauge(ui, frac, usage_color(frac));
                        ui.add_space(4.0);
                        ui.label(format!(
                            "{} utilisés sur {}  ·  {} disponibles",
                            format_size(st.used()),
                            format_size(st.total),
                            format_size(st.free)
                        ));
                    }
                    None => {
                        ui.label("Indisponible");
                    }
                });

                // --- Mémoire ---
                card(ui, "Mémoire (RAM)", |ui| match &info.memory {
                    Some(m) => {
                        let frac = m.used as f32 / m.total.max(1) as f32;
                        gauge(ui, frac, usage_color(frac));
                        ui.add_space(4.0);
                        ui.label(format!(
                            "{} utilisés sur {}  ({:.0} %)",
                            format_size(m.used),
                            format_size(m.total),
                            frac * 100.0
                        ));
                        if m.swap_total > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "Swap : {} sur {}",
                                    format_size(m.swap_used),
                                    format_size(m.swap_total)
                                ))
                                .weak(),
                            );
                        }
                    }
                    None => {
                        ui.label("Indisponible");
                    }
                });

                // --- Batterie ---
                card(ui, "Batterie", |ui| match &info.battery {
                    Some(b) => {
                        if let Some(p) = b.percent {
                            let frac = p as f32 / 100.0;
                            // Pour la batterie, « peu » est mauvais : couleur inversée.
                            gauge(ui, frac, usage_color(1.0 - frac));
                            ui.add_space(4.0);
                            let state = if b.charging {
                                "en charge"
                            } else if b.on_ac {
                                "sur secteur"
                            } else {
                                "sur batterie"
                            };
                            ui.label(format!("{} %  ·  {}", p, state));
                            ui.add_space(6.0);
                        }
                        egui::Grid::new("sys_batt").num_columns(2).spacing([24.0, 6.0]).show(
                            ui,
                            |ui| {
                                row(
                                    ui,
                                    "Cycles de charge",
                                    b.cycle_count.map(|c| c.to_string()).unwrap_or_else(dash),
                                );
                                row(
                                    ui,
                                    "Capacité maximale",
                                    b.health_percent.map(|h| format!("{} %", h)).unwrap_or_else(dash),
                                );
                                row(ui, "État", b.condition.clone().unwrap_or_else(dash));
                            },
                        );
                    }
                    None => {
                        ui.label("Aucune batterie détectée.");
                    }
                });
            });
        });
        if refresh {
            self.sys_last = None;
            self.start_system();
        }
    }

    // ===================== Onglet « Désinstaller » =====================

    fn ui_uninstall(&mut self, ctx: &egui::Context) {
        let busy = matches!(self.phase, Phase::Scanning | Phase::Removing);

        self.ui_apps_panel(ctx, busy);
        self.ui_header(ctx, busy);

        let show_actions = matches!(self.phase, Phase::Ready | Phase::Removing | Phase::Done)
            && (!self.items.is_empty() || !self.logs.is_empty());
        if show_actions {
            self.ui_actions_panel(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.phase {
            Phase::Idle => {
                ui.add_space(20.0);
                ui.label(
                    "Choisissez une application dans la liste de gauche, ou saisissez son \
                     nom ci-dessus puis cliquez sur « Analyser ».",
                );
            }
            Phase::Scanning => {
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Recherche du .app, du bundle id et des fichiers annexes…");
                });
            }
            Phase::Ready | Phase::Removing | Phase::Done => self.show_results_ui(ui),
        });
    }

    /// Panneau de gauche : applications installées, avec filtre et statuts.
    fn ui_apps_panel(&mut self, ctx: &egui::Context, busy: bool) {
        let mut clicked: Option<InstalledApp> = None;
        let mut reload = false;

        egui::SidePanel::left("apps_panel")
            .resizable(true)
            .default_width(290.0)
            .min_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Applications");
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    if self.apps_loading {
                        ui.spinner();
                        ui.label("Lecture des applications…");
                    } else {
                        ui.label(
                            RichText::new(format!("{} applications", self.installed_apps.len())).weak(),
                        );
                        if ui.small_button("Actualiser").clicked() {
                            reload = true;
                        }
                    }
                });

                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.app_filter)
                        .hint_text("Filtrer…")
                        .desired_width(f32::INFINITY),
                );

                // Légende des icônes.
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for status in [
                        RemovalStatus::Removable,
                        RemovalStatus::NeedsAdmin,
                        RemovalStatus::Protected,
                    ] {
                        status_icon(ui, status, false);
                        ui.small(status_short(status));
                    }
                    running_dot(ui);
                    ui.small("Active");
                });
                ui.add_space(2.0);
                ui.separator();

                let filter = self.app_filter.to_lowercase();

                egui::ScrollArea::vertical()
                    .id_source("installed_apps_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                            for app in &self.installed_apps {
                                if !filter.is_empty() && !app.name.to_lowercase().contains(&filter) {
                                    continue;
                                }
                                let selected = self.selected_path.as_ref() == Some(&app.path);

                                // Nom en normal, bundle id en petit et grisé.
                                let mut job = egui::text::LayoutJob::default();
                                job.append(
                                    &app.name,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::TextStyle::Body.resolve(ui.style()),
                                        color: ui.visuals().text_color(),
                                        ..Default::default()
                                    },
                                );
                                if let Some(id) = &app.bundle_id {
                                    job.append(
                                        &format!("\n{}", id),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::TextStyle::Small.resolve(ui.style()),
                                            color: ui.visuals().weak_text_color(),
                                            ..Default::default()
                                        },
                                    );
                                }

                                ui.horizontal(|ui| {
                                    status_icon(ui, app.status, app.running);
                                    ui.add_enabled_ui(!busy, |ui| {
                                        if ui.selectable_label(selected, job).clicked() {
                                            clicked = Some(app.clone());
                                        }
                                    });
                                });
                            }
                        });
                    });
            });

        if reload && !self.apps_loading {
            self.reload_apps();
        }
        if let Some(app) = clicked {
            self.app_name = app.name;
            self.selected_path = Some(app.path);
            self.start_scan();
        }
    }

    /// Bandeau du haut de l'onglet : saisie du nom de l'application.
    fn ui_header(&mut self, ctx: &egui::Context, busy: bool) {
        let mut do_scan = false;

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Désinstallez une application et supprimez tous ses fichiers annexes \
                     (préférences, caches, services…) pour récupérer de l'espace.",
                )
                .weak(),
            );
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("Application :");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.app_name)
                        .hint_text("ex : Zoom, Slack, Docker…")
                        .desired_width(300.0),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let can_scan = !busy && !self.app_name.trim().is_empty();
                if ui
                    .add_enabled(can_scan, egui::Button::new("🔍  Analyser"))
                    .clicked()
                    || (enter && can_scan)
                {
                    do_scan = true;
                }
            });
            ui.add_space(8.0);
        });

        if do_scan {
            self.start_scan();
        }
    }

    /// Bandeau du bas : options, confirmation, bouton Supprimer et journal.
    fn ui_actions_panel(&mut self, ctx: &egui::Context) {
        let removing = self.phase == Phase::Removing;
        let selected_count = self.items.iter().filter(|i| i.selected).count();
        let selected_size: u64 = self.items.iter().filter(|i| i.selected).map(|i| i.size).sum();
        let mut start = false;

        egui::TopBottomPanel::bottom("actions_panel").show(ctx, |ui| {
            ui.add_space(8.0);

            if !self.items.is_empty() {
                ui.add_enabled(
                    !removing,
                    egui::Checkbox::new(
                        &mut self.keep_receipts,
                        "Conserver le reçu du package (pkgutil)",
                    ),
                );
                ui.add_enabled(
                    !removing,
                    egui::Checkbox::new(
                        &mut self.confirm_checked,
                        format!(
                            "⚠  Je confirme vouloir supprimer DÉFINITIVEMENT ces {} éléments ({})",
                            selected_count,
                            format_size(selected_size)
                        ),
                    ),
                );
                ui.add_space(6.0);

                let can_delete = self.confirm_checked && selected_count > 0 && !removing;
                let button = egui::Button::new(
                    RichText::new(format!(
                        "🗑  Supprimer ({}) — {}",
                        selected_count,
                        format_size(selected_size)
                    ))
                    .strong()
                    .color(Color32::WHITE),
                )
                .fill(if can_delete { RED } else { Color32::from_gray(90) });

                ui.horizontal(|ui| {
                    if ui.add_enabled(can_delete, button).clicked() {
                        start = true;
                    }
                    if removing {
                        ui.spinner();
                        ui.label("Suppression en cours…");
                    }
                });
                if removing {
                    ui.label(
                        RichText::new(
                            "Les éléments 🔒 demandent les droits administrateur : une fenêtre \
                             d'authentification macOS peut s'afficher (ou le mot de passe sudo \
                             dans le Terminal si l'application a été lancée depuis un Terminal).",
                        )
                        .weak(),
                    );
                }
            }

            show_log(ui, &self.logs, "log_scroll");
            ui.add_space(8.0);
        });

        if start {
            self.start_removal();
        }
    }

    fn show_results_ui(&mut self, ui: &mut egui::Ui) {
        let removing = self.phase == Phase::Removing;

        // --- Bilan de la dernière suppression ---
        if self.phase == Phase::Done {
            if let Some((freed, failed)) = self.summary {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.colored_label(
                        GREEN,
                        RichText::new(format!("✅  {} libérés", format_size(freed))).strong(),
                    );
                    if failed > 0 {
                        ui.colored_label(
                            ORANGE,
                            format!(
                                "⚠  {} élément(s) n'ont pas pu être supprimés (autorisation administrateur \
                                 refusée ou annulée ?). Ils restent listés ci-dessous : vous pouvez \
                                 relancer la suppression pour réessayer.",
                                failed
                            ),
                        );
                    }
                });
                ui.add_space(8.0);
            }
        } else {
            // --- Résumé de l'app trouvée ---
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &self.app_path {
                    Some(p) => {
                        ui.horizontal(|ui| {
                            if let Some(status) = self.app_status {
                                status_icon(ui, status, self.app_running);
                            }
                            ui.label(format!("Application trouvée : {}", pretty_path(p)));
                        });
                        match self.app_status {
                            Some(RemovalStatus::Protected) => {
                                ui.colored_label(
                                    RED,
                                    "Cette application est protégée par macOS : elle ne peut pas être \
                                     supprimée. Ses fichiers annexes ne sont pas cochés par défaut.",
                                );
                            }
                            Some(RemovalStatus::NeedsAdmin) => {
                                ui.label(
                                    RichText::new("Une autorisation administrateur sera demandée.").weak(),
                                );
                            }
                            _ => {}
                        }
                        if self.app_running {
                            ui.colored_label(
                                BLUE,
                                "Cette application est en cours d'exécution : ses processus seront \
                                 fermés avant la suppression. Pensez à enregistrer votre travail.",
                            );
                        }
                    }
                    None => {
                        ui.colored_label(
                            ORANGE,
                            "⚠  Aucun .app trouvé dans /Applications ni ~/Applications : recherche \
                             par nom uniquement. Rien n'est coché par défaut, vérifiez chaque \
                             élément avant de supprimer.",
                        );
                    }
                }
                match &self.bundle_id {
                    Some(id) => {
                        ui.label(format!("Bundle identifier : {}", id));
                    }
                    None => {
                        ui.label("Bundle identifier : (inconnu)");
                    }
                }
            });
            ui.add_space(8.0);
        }

        if self.items.is_empty() {
            if self.phase != Phase::Done {
                ui.label("Aucun fichier annexe trouvé pour cette application.");
            }
            return;
        }

        // --- En-tête de liste ---
        let total_size: u64 = self.items.iter().map(|i| i.size).sum();
        ui.horizontal(|ui| {
            ui.strong(format!(
                "{} éléments trouvés — {}",
                self.items.len(),
                format_size(total_size)
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!removing, |ui| {
                    if ui.small_button("Tout décocher").clicked() {
                        for it in &mut self.items {
                            it.selected = false;
                        }
                    }
                    if ui.small_button("Tout cocher").clicked() {
                        for it in &mut self.items {
                            it.selected = !it.locked;
                        }
                    }
                });
            });
        });
        ui.add_space(4.0);

        // --- Liste des éléments avec cases à cocher ---
        egui::ScrollArea::vertical()
            .id_source("items_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_enabled_ui(!removing, |ui| {
                    for item in &mut self.items {
                        ui.horizontal(|ui| {
                            ui.add_enabled(!item.locked, egui::Checkbox::new(&mut item.selected, ""));

                            let size_text =
                                RichText::new(format!("{:>9}", format_size(item.size))).monospace();
                            ui.label(if item.size >= BIG_ITEM_BYTES {
                                size_text.strong()
                            } else {
                                size_text.weak()
                            });

                            if item.locked {
                                status_icon(ui, RemovalStatus::Protected, false);
                            }
                            let mut label = item.display.clone();
                            if item.needs_sudo {
                                label.push_str("  🔒");
                            }
                            ui.label(label).on_hover_text(item.path.display().to_string());
                        });
                    }
                });
            });
    }

    // ===================== Onglet « Nettoyage » =====================

    fn ui_clean(&mut self, ctx: &egui::Context) {
        let scanning = self.clean_phase == CleanPhase::Scanning;
        let cleaning = self.clean_phase == CleanPhase::Cleaning;
        let busy = scanning || cleaning;
        let mut do_scan = false;

        egui::TopBottomPanel::top("clean_header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Repérez les fichiers inutiles (caches, journaux, corbeille, caches de \
                     développement). Vous choisissez ce qui est supprimé avant de lancer le nettoyage.",
                )
                .weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = if self.clean_phase == CleanPhase::Idle {
                    "🔍  Scanner"
                } else {
                    "🔍  Relancer le scan"
                };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    do_scan = true;
                }
                if scanning {
                    ui.spinner();
                    ui.label(format!("Analyse : {}…", self.clean_progress));
                }
            });
            ui.add_space(8.0);
        });
        if do_scan {
            self.start_clean_scan();
        }

        // --- Bandeau du bas : confirmation, bouton Nettoyer, journal ---
        let selected_count: usize = self
            .groups
            .iter()
            .flat_map(|g| g.entries.iter())
            .filter(|e| e.selected)
            .count();
        let selected_size: u64 = self
            .groups
            .iter()
            .flat_map(|g| g.entries.iter())
            .filter(|e| e.selected)
            .map(|e| e.size)
            .sum();
        let mut start = false;

        if self.clean_phase != CleanPhase::Idle && (!self.groups.is_empty() || !self.clean_logs.is_empty()) {
            egui::TopBottomPanel::bottom("clean_actions").show(ctx, |ui| {
                ui.add_space(8.0);

                if !self.groups.is_empty() {
                    ui.add_enabled(
                        !busy,
                        egui::Checkbox::new(
                            &mut self.clean_confirm,
                            format!(
                                "⚠  Je confirme vouloir supprimer DÉFINITIVEMENT ces {} éléments ({})",
                                selected_count,
                                format_size(selected_size)
                            ),
                        ),
                    );
                    ui.add_space(6.0);

                    let can_clean = self.clean_confirm && selected_count > 0 && !busy;
                    let button = egui::Button::new(
                        RichText::new(format!(
                            "Nettoyer ({}) — {}",
                            selected_count,
                            format_size(selected_size)
                        ))
                        .strong()
                        .color(Color32::WHITE),
                    )
                    .fill(if can_clean { RED } else { Color32::from_gray(90) });

                    ui.horizontal(|ui| {
                        if ui.add_enabled(can_clean, button).clicked() {
                            start = true;
                        }
                        if cleaning {
                            ui.spinner();
                            ui.label("Nettoyage en cours…");
                        }
                    });
                    ui.label(
                        RichText::new("Quittez les applications concernées avant de nettoyer leurs caches.")
                            .weak(),
                    );
                }

                show_log(ui, &self.clean_logs, "clean_log_scroll");
                ui.add_space(8.0);
            });
        }
        if start {
            self.start_clean();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.clean_phase == CleanPhase::Idle {
                ui.add_space(20.0);
                ui.label("Cliquez sur « Scanner » pour rechercher les fichiers que vous pouvez supprimer.");
                return;
            }

            // --- Bilan du dernier nettoyage ---
            if self.clean_phase == CleanPhase::Done {
                if let Some((freed, failed)) = self.clean_summary {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.colored_label(
                            GREEN,
                            RichText::new(format!("✅  {} libérés", format_size(freed))).strong(),
                        );
                        if failed > 0 {
                            ui.colored_label(
                                ORANGE,
                                format!(
                                    "⚠  {} élément(s) n'ont pas pu être supprimés (fichiers en cours \
                                     d'utilisation ou accès refusé). Ils restent listés ci-dessous.",
                                    failed
                                ),
                            );
                        }
                    });
                    ui.add_space(8.0);
                }
            }

            if self.groups.is_empty() {
                ui.add_space(12.0);
                if scanning {
                    ui.label("Recherche en cours…");
                } else {
                    ui.label("Rien à nettoyer : aucun fichier inutile trouvé.");
                }
                return;
            }

            // --- En-tête de liste ---
            let total: u64 = self.groups.iter().map(|g| g.total()).sum();
            ui.horizontal(|ui| {
                ui.strong(format!(
                    "{} catégories — {} récupérables",
                    self.groups.len(),
                    format_size(total)
                ));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled_ui(!cleaning, |ui| {
                        if ui.small_button("Tout décocher").clicked() {
                            for e in self.groups.iter_mut().flat_map(|g| g.entries.iter_mut()) {
                                e.selected = false;
                            }
                        }
                        if ui.small_button("Tout cocher").clicked() {
                            for e in self.groups.iter_mut().flat_map(|g| g.entries.iter_mut()) {
                                e.selected = true;
                            }
                        }
                    });
                });
            });
            ui.add_space(4.0);

            // --- Catégories dépliables ---
            egui::ScrollArea::vertical()
                .id_source("junk_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_enabled_ui(!cleaning, |ui| {
                        for group in &mut self.groups {
                            let id = ui.make_persistent_id(("junk_group", group.id));
                            egui::collapsing_header::CollapsingState::load_with_default_open(
                                ui.ctx(),
                                id,
                                false,
                            )
                            .show_header(ui, |ui| {
                                let mut all =
                                    !group.entries.is_empty() && group.entries.iter().all(|e| e.selected);
                                if ui.checkbox(&mut all, "").changed() {
                                    for e in &mut group.entries {
                                        e.selected = all;
                                    }
                                }
                                ui.strong(group.name);
                                ui.label(
                                    RichText::new(format!(
                                        "{} — {} élément(s)",
                                        format_size(group.total()),
                                        group.entries.len()
                                    ))
                                    .weak(),
                                );
                            })
                            .body(|ui| {
                                ui.label(RichText::new(group.description).weak());
                                if let Some(note) = &group.note {
                                    ui.colored_label(ORANGE, note);
                                }
                                for entry in &mut group.entries {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut entry.selected, "");
                                        let size_text =
                                            RichText::new(format!("{:>9}", format_size(entry.size)))
                                                .monospace();
                                        ui.label(if entry.size >= BIG_ITEM_BYTES {
                                            size_text.strong()
                                        } else {
                                            size_text.weak()
                                        });
                                        ui.label(&entry.display)
                                            .on_hover_text(entry.path.display().to_string());
                                    });
                                }
                            });
                            ui.add_space(2.0);
                        }
                    });
                });
        });
    }
}

/// Journal défilant (ne s'affiche que s'il y a des lignes).
fn show_log(ui: &mut egui::Ui, lines: &[String], id: &str) {
    if lines.is_empty() {
        return;
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Journal").strong());
    egui::ScrollArea::vertical()
        .max_height(130.0)
        .id_source(id)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in lines {
                ui.monospace(line);
            }
        });
}
