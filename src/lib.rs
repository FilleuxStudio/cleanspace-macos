//! Logique partagée entre la CLI (`clean-uninstall`) et l'interface graphique
//! (`clean-uninstall-gui`) : recherche du bundle .app, du bundle id, scan des
//! fichiers annexes, arrêt des services launchd, suppression, etc.

pub mod sysinfo;

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::IsTerminal;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

/// Un chemin trouvé lors du scan, avec l'info de savoir s'il nécessite sudo.
#[derive(Debug, Clone)]
pub struct FoundItem {
    pub path: PathBuf,
    pub needs_sudo: bool,
}

/// Ce que CleanSpace peut faire d'une application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalStatus {
    /// Supprimable sans droits particuliers.
    Removable,
    /// Supprimable, mais une autorisation administrateur sera demandée.
    NeedsAdmin,
    /// Protégée par macOS (SIP, volume système) : impossible à supprimer.
    Protected,
}

/// Résultat complet d'une analyse d'application.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub app_path: Option<PathBuf>,
    pub app_status: Option<RemovalStatus>,
    pub bundle_id: Option<String>,
    pub items: Vec<FoundItem>,
}

/// Une application installée détectée dans /Applications ou ~/Applications,
/// candidate à la désinstallation.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: Option<String>,
    pub status: RemovalStatus,
    /// Vrai si au moins un process est lancé depuis le bundle au moment du scan.
    pub running: bool,
}

/// Liste toutes les applications installées dans /Applications et
/// ~/Applications, triées par nom (insensible à la casse). Lit aussi le
/// bundle id de chacune via `defaults read`, ce qui peut prendre quelques
/// centaines de ms au total selon le nombre d'apps : à appeler dans un
/// thread de fond (voir la GUI), jamais depuis le thread UI.
pub fn list_installed_apps() -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    let mut seen_paths = BTreeSet::new();
    // Une seule lecture de la liste des process pour tout le scan.
    let running_cmds = running_command_lines();

    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = home_dir() {
        roots.push(home.join("Applications"));
    }

    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "app").unwrap_or(false) && seen_paths.insert(path.clone()) {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let bundle_id = get_bundle_id(&path);
                let status = app_status(&path);
                let running = running_cmds.contains(&format!("{}/Contents/", path.display()));
                apps.push(InstalledApp { name, path, bundle_id, status, running });
            }
        }
    }

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Nom "de recherche" dérivé de la saisie : si `query` est un chemin (ou un nom)
/// se terminant par `.app`, on garde uniquement le nom du bundle sans extension
/// (`/Applications/Zoom.app` -> `Zoom`) ; sinon la saisie telle quelle.
fn search_name(query: &str) -> String {
    let path = Path::new(query);
    if path.extension().map(|e| e == "app").unwrap_or(false) {
        if let Some(stem) = path.file_stem() {
            return stem.to_string_lossy().to_string();
        }
    }
    query.to_string()
}

/// Effectue l'analyse complète : trouve le .app, son bundle id, et tous les
/// fichiers annexes. Ne supprime rien.
///
/// `query` peut être un nom ("Zoom") ou un chemin complet vers un `.app`.
pub fn analyze(query: &str, depth: usize) -> ScanResult {
    let query = query.trim();
    let app_path = find_app_bundle(query);
    let bundle_id = app_path.as_deref().and_then(get_bundle_id);

    // Les mots-clés doivent venir du nom de l'app, jamais d'un chemin complet.
    let name = search_name(query);
    let mut related = scan_related_paths(&name, bundle_id.as_deref(), depth);
    if let Some(p) = &app_path {
        related.insert(p.clone());
    }

    let items = related
        .into_iter()
        .filter(|p| !is_protected(p))
        .map(|p| {
            let needs_sudo = needs_sudo(&p);
            FoundItem { path: p, needs_sudo }
        })
        .collect();

    let app_status = app_path.as_deref().map(app_status);

    ScanResult {
        app_path,
        app_status,
        bundle_id,
        items,
    }
}

/// Exécute la suppression réelle des chemins fournis + arrêt des services et
/// des process. `log` reçoit chaque ligne de progression (println côté CLI,
/// envoi dans un channel côté GUI).
///
/// - Les process sont arrêtés en ciblant le **chemin du bundle** `.app`
///   présent dans `paths` (et non le nom de l'app, trop générique).
/// - Toutes les opérations nécessitant les droits administrateur sont
///   regroupées en **une seule** exécution : `sudo` si on tourne dans un
///   Terminal, sinon la fenêtre d'authentification macOS (osascript).
/// - Les chemins protégés (`~/Library`, `/Applications`, …) sont toujours ignorés.
pub fn perform_removal(
    app_name: &str,
    bundle_id: Option<&str>,
    paths: &[PathBuf],
    keep_receipts: bool,
    log: &mut dyn FnMut(String),
) {
    let mut safe: Vec<&Path> = Vec::new();
    for p in paths {
        if is_protected(p) {
            log(format!("⛔ Chemin protégé, ignoré : {}", p.display()));
        } else if is_app_bundle(p) && app_status(p) == RemovalStatus::Protected {
            log(format!("⛔ Application protégée par macOS, ignorée : {}", p.display()));
        } else {
            safe.push(p.as_path());
        }
    }

    // Commandes à exécuter en root, dans l'ordre (voir `run_as_root`).
    let mut root_cmds: Vec<String> = Vec::new();
    let mut root_paths: Vec<&Path> = Vec::new();

    // 1. Décharger les services launchd AVANT de supprimer leurs plist
    for p in &safe {
        if !is_launch_plist(p) {
            continue;
        }
        log(format!("🛑 Arrêt du service : {}", p.display()));
        if needs_sudo(p) {
            root_cmds.push(format!(
                "launchctl unload -w {} 2>/dev/null || true",
                sh_quote(&p.to_string_lossy())
            ));
        } else {
            let ok = Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(p)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                log("   (déjà arrêté ou non chargé)".to_string());
            }
        }
    }

    // 2. Arrêter les process de l'app : uniquement ceux lancés depuis le bundle.
    let bundles: Vec<&Path> = safe.iter().copied().filter(|p| is_app_bundle(p)).collect();
    if bundles.is_empty() {
        log(format!(
            "ℹ️  Aucun .app dans la sélection : aucun process arrêté pour « {} ».",
            app_name
        ));
    }
    for bundle in bundles {
        log(format!("🔪 Arrêt des process de : {}", bundle.display()));
        let pattern = process_pattern(bundle);
        let _ = Command::new("pkill").arg("-f").arg(&pattern).status();
        if needs_sudo(bundle) {
            // Process éventuellement lancés en root (helpers, daemons).
            root_cmds.push(format!("pkill -f {} 2>/dev/null || true", sh_quote(&pattern)));
        }
    }

    // 3. Supprimer tous les chemins
    for p in &safe {
        if !p.exists() && !p.is_symlink() {
            continue;
        }
        if needs_sudo(p) {
            log(format!("🗑️  Suppression (administrateur) : {}", p.display()));
            root_cmds.push(format!("rm -rf -- {}", sh_quote(&p.to_string_lossy())));
            root_paths.push(p);
        } else if !remove_user_path(p, log) && (p.exists() || p.is_symlink()) {
            // Fichiers appartenant à root à l'intérieur d'un dossier à nous, etc.
            log("   ↪ Nouvelle tentative avec les droits administrateur".to_string());
            root_cmds.push(format!("rm -rf -- {}", sh_quote(&p.to_string_lossy())));
            root_paths.push(p);
        }
    }

    // 4. Oublier le reçu pkgutil (seulement s'il existe : évite une demande
    //    de mot de passe inutile pour les apps installées par glisser-déposer).
    if !keep_receipts {
        if let Some(id) = bundle_id {
            if receipt_exists(id) {
                log(format!("🧾 Oubli du reçu pkgutil : {}", id));
                root_cmds.push(format!("pkgutil --forget {} >/dev/null 2>&1 || true", sh_quote(id)));
            }
        }
    }

    // 5. Tout ce qui demande les droits administrateur, en une seule fois.
    if !root_cmds.is_empty() {
        log("🔐 Droits administrateur requis…".to_string());
        let ok = run_as_root(&root_cmds.join("; "), log);
        if ok {
            for p in root_paths {
                if p.exists() || p.is_symlink() {
                    log(format!("   ⚠️  Échec de la suppression : {}", p.display()));
                }
            }
        }
    }

    log("✅ Terminé.".to_string());
}

/// Cherche un bundle .app dans /Applications et ~/Applications, ou accepte un chemin direct.
pub fn find_app_bundle(query: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(query);
    if direct.extension().map(|e| e == "app").unwrap_or(false) && direct.exists() {
        return Some(direct);
    }

    let mut candidates: Vec<(PathBuf, bool)> = Vec::new(); // (path, exact_match)
    // "Zoom.app" saisi comme un nom doit aussi fonctionner.
    let needle = search_name(query).to_lowercase();

    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = home_dir() {
        roots.push(home.join("Applications"));
    }

    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "app").unwrap_or(false) {
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if stem == needle {
                    candidates.push((path, true));
                } else if stem.contains(&needle) {
                    candidates.push((path, false));
                }
            }
        }
    }

    candidates.sort_by_key(|(_, exact)| !*exact);
    candidates.into_iter().next().map(|(p, _)| p)
}

/// Lit le CFBundleIdentifier depuis Contents/Info.plist via `defaults read`.
pub fn get_bundle_id(app_path: &Path) -> Option<String> {
    let info_plist = app_path.join("Contents").join("Info");
    let output = Command::new("defaults")
        .arg("read")
        .arg(&info_plist)
        .arg("CFBundleIdentifier")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Construit la liste des "mots-clés" à rechercher : nom de l'app, nom sans espaces,
/// bundle id complet, et ses segments (ex: "us.zoom.xos" -> "zoom", "xos").
pub fn build_needles(app_name: &str, bundle_id: Option<&str>) -> Vec<String> {
    let mut needles = BTreeSet::new();
    let name_lower = app_name.to_lowercase();
    needles.insert(name_lower.clone());
    needles.insert(name_lower.replace(' ', ""));
    needles.insert(name_lower.replace(' ', "."));

    if let Some(id) = bundle_id {
        let id_lower = id.to_lowercase();
        needles.insert(id_lower.clone());
        for part in id_lower.split('.') {
            if part.len() > 2 && !["com", "org", "www", "app", "inc"].contains(&part) {
                needles.insert(part.to_string());
            }
        }
    }
    needles.retain(|n| n.len() >= 3);
    needles.into_iter().collect()
}

fn matches_needle(name: &str, needles: &[String]) -> bool {
    let name_lower = name.to_lowercase();
    needles.iter().any(|n| name_lower.contains(n.as_str()))
}

/// Répertoires "usuels" où une app laisse des traces, en plus d'un scan générique.
fn candidate_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/Application Support"),
        home.join("Library/Preferences"),
        home.join("Library/Caches"),
        home.join("Library/Logs"),
        home.join("Library/Saved Application State"),
        home.join("Library/Containers"),
        home.join("Library/Group Containers"),
        home.join("Library/LaunchAgents"),
        home.join("Library/WebKit"),
        home.join("Library/HTTPStorages"),
        home.join("Library/Cookies"),
        PathBuf::from("/Library/Application Support"),
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
        PathBuf::from("/Library/Preferences"),
        PathBuf::from("/Library/Caches"),
        PathBuf::from("/Library/Logs"),
        PathBuf::from("/var/db/receipts"),
    ]
}

/// Scanne les emplacements usuels + un scan générique plus profond dans
/// ~/Library et /Library, façon `find ~/Library /Library -iname "*name*"`.
pub fn scan_related_paths(app_name: &str, bundle_id: Option<&str>, depth: usize) -> BTreeSet<PathBuf> {
    let needles = build_needles(app_name, bundle_id);
    let mut found = BTreeSet::new();

    let Some(home) = home_dir() else {
        return found;
    };

    for root in candidate_roots(&home) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if matches_needle(&name, &needles) {
                found.insert(path);
            }
        }
    }

    for root in [home.join("Library"), PathBuf::from("/Library")] {
        if !root.exists() {
            continue;
        }
        // min_depth(1) : on ne teste jamais la racine elle-même (un mot-clé
        // comme "lib" correspondrait sinon à "Library" et sélectionnerait
        // tout ~/Library).
        for entry in WalkDir::new(&root)
            .min_depth(1)
            .max_depth(depth)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if found.iter().any(|f| path.starts_with(f) && path != *f) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if matches_needle(&name, &needles) {
                found.insert(path.to_path_buf());
            }
        }
    }

    let all: Vec<PathBuf> = found.iter().cloned().collect();
    let mut cleaned = BTreeSet::new();
    for p in &all {
        let has_parent_in_set = all.iter().any(|other| other != p && p.starts_with(other));
        if !has_parent_in_set {
            cleaned.insert(p.clone());
        }
    }
    cleaned
}

pub fn is_launch_plist(path: &Path) -> bool {
    let is_plist = path.extension().map(|e| e == "plist").unwrap_or(false);
    let in_launch_dir =
        path.to_string_lossy().contains("LaunchAgents/") || path.to_string_lossy().contains("LaunchDaemons/");
    is_plist && in_launch_dir && path.is_file()
}

/// Vrai si supprimer ce chemin demandera les droits administrateur : il est
/// dans une zone système (/Applications, /Library, …) **et** l'utilisateur ne
/// peut pas le supprimer lui-même (propriétaire différent, dossier parent non
/// modifiable). Une app glissée dans /Applications par l'utilisateur est donc
/// supprimable sans mot de passe.
pub fn needs_sudo(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let in_system_area = s.starts_with("/Applications")
        || s.starts_with("/Library")
        || s.starts_with("/var")
        || s.starts_with("/private")
        || s.starts_with("/usr");
    in_system_area && !user_can_delete(path)
}

/// Le propriétaire est l'utilisateur courant et il peut modifier le dossier parent.
fn user_can_delete(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    // SAFETY: geteuid n'a aucune précondition et ne peut pas échouer.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(c_parent) = CString::new(parent.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_parent` est une chaîne C valide pour la durée de l'appel.
    unsafe { libc::access(c_parent.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// Vrai si macOS interdit de modifier ce chemin (drapeau `restricted` = SIP).
fn has_restricted_flag(path: &Path) -> bool {
    Command::new("stat")
        .args(["-f", "%Sf"])
        .arg(path)
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("restricted"))
        .unwrap_or(false)
}

/// Statut de suppression d'une application (voir [`RemovalStatus`]).
pub fn app_status(path: &Path) -> RemovalStatus {
    if path.starts_with("/System") || has_restricted_flag(path) {
        RemovalStatus::Protected
    } else if needs_sudo(path) {
        RemovalStatus::NeedsAdmin
    } else {
        RemovalStatus::Removable
    }
}

/// Lignes de commande de tous les process en cours (une par ligne).
fn running_command_lines() -> String {
    Command::new("ps")
        .args(["-axww", "-o", "command="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Taille réellement occupée sur le disque (blocs alloués) d'un fichier ou
/// d'un dossier. Les liens symboliques ne sont pas suivis ; ce qui n'est pas
/// lisible (droits) est ignoré.
pub fn disk_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.blocks() * 512;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| !m.is_dir())
        .map(|m| m.blocks() * 512)
        .sum()
}

// ---------------------------------------------------------------------------
// Nettoyage : scanner de fichiers inutiles
// ---------------------------------------------------------------------------

/// Un dossier à scanner : ses **enfants directs** sont les éléments proposés
/// au nettoyage (jamais le dossier lui-même).
#[derive(Debug, Clone)]
pub struct JunkSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub root: PathBuf,
}

/// Un élément proposé au nettoyage.
#[derive(Debug, Clone)]
pub struct JunkItem {
    pub path: PathBuf,
    pub size: u64,
    pub selected_by_default: bool,
}

/// Résultat du scan d'un [`JunkSpec`].
#[derive(Debug, Clone)]
pub struct JunkCategory {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Éléments triés du plus gros au plus petit (taille nulle ignorée).
    pub items: Vec<JunkItem>,
    /// Message d'information (ex. accès refusé).
    pub note: Option<String>,
}

/// Les emplacements scannés (tous dans le dossier personnel : aucun droit
/// administrateur n'est nécessaire pour nettoyer).
pub fn junk_specs() -> Vec<JunkSpec> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let spec = |id, name, description, root: PathBuf| JunkSpec { id, name, description, root };
    vec![
        spec(
            "caches",
            "Caches des applications",
            "Fichiers temporaires que les applications recréent toutes seules. \
             Les caches com.apple.* (système) sont décochés par défaut.",
            home.join("Library/Caches"),
        ),
        spec(
            "logs",
            "Journaux et rapports de plantage",
            "Anciens journaux d'applications et rapports de diagnostic.",
            home.join("Library/Logs"),
        ),
        spec(
            "trash",
            "Corbeille",
            "Éléments déjà jetés. Nécessite l'accès complet au disque pour CleanSpace.",
            home.join(".Trash"),
        ),
        spec(
            "xcode_derived",
            "Xcode : données dérivées",
            "Résultats de compilation (DerivedData), régénérés au prochain build.",
            home.join("Library/Developer/Xcode/DerivedData"),
        ),
        spec(
            "xcode_devsupport",
            "Xcode : support des appareils iOS",
            "Symboles téléchargés pour vos appareils, re-téléchargés à la prochaine connexion.",
            home.join("Library/Developer/Xcode/iOS DeviceSupport"),
        ),
        spec(
            "simulator_caches",
            "Simulateurs iOS : caches",
            "Caches des simulateurs Xcode.",
            home.join("Library/Developer/CoreSimulator/Caches"),
        ),
        spec(
            "npm",
            "Cache npm",
            "Paquets téléchargés par npm, re-téléchargés au besoin.",
            home.join(".npm/_cacache"),
        ),
        spec(
            "cargo",
            "Cache Cargo",
            "Archives de crates téléchargées (registre), re-téléchargées au prochain build.",
            home.join(".cargo/registry/cache"),
        ),
        spec(
            "gradle",
            "Cache Gradle",
            "Dépendances téléchargées par Gradle, re-téléchargées au besoin.",
            home.join(".gradle/caches"),
        ),
    ]
}

/// Scanne un emplacement : liste ses enfants directs avec leur taille.
/// À appeler dans un thread de fond (le calcul des tailles peut être long).
pub fn scan_junk_category(spec: &JunkSpec) -> JunkCategory {
    let mut items = Vec::new();
    let mut note = None;

    match std::fs::read_dir(&spec.root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == ".DS_Store" || name == ".localized" {
                    continue;
                }
                let path = entry.path();
                let size = disk_size(&path);
                if size == 0 {
                    continue;
                }
                let selected_by_default = !(spec.id == "caches" && name.starts_with("com.apple."));
                items.push(JunkItem { path, size, selected_by_default });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            note = Some(
                "Accès refusé : autorisez l'accès complet au disque pour CleanSpace \
                 (Réglages Système › Confidentialité et sécurité)."
                    .to_string(),
            );
        }
        Err(_) => {} // dossier absent : rien à nettoyer ici
    }

    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    JunkCategory {
        id: spec.id,
        name: spec.name,
        description: spec.description,
        items,
        note,
    }
}

/// Supprime des éléments proposés par le scanner. Garde-fou : seuls les
/// enfants des dossiers de [`junk_specs`] sont acceptés ; tout le reste est ignoré.
pub fn clean_junk(paths: &[PathBuf], log: &mut dyn FnMut(String)) {
    clean_junk_in(&junk_specs(), paths, log);
}

fn clean_junk_in(specs: &[JunkSpec], paths: &[PathBuf], log: &mut dyn FnMut(String)) {
    for p in paths {
        let escapes = p.components().any(|c| matches!(c, Component::ParentDir));
        let allowed = !escapes && specs.iter().any(|s| p.starts_with(&s.root) && *p != s.root);
        if !allowed {
            log(format!("⛔ Hors des dossiers de nettoyage, ignoré : {}", p.display()));
            continue;
        }
        remove_user_path(p, log);
    }
    log("✅ Nettoyage terminé.".to_string());
}

/// Chemins qu'on ne supprime JAMAIS, même s'ils correspondent à un mot-clé :
/// racines système, dossiers personnels de premier niveau et dossiers usuels
/// scannés (Caches, Preferences, …). Seuls leurs *contenus* peuvent l'être.
pub fn is_protected(path: &Path) -> bool {
    let mut protected: Vec<PathBuf> = [
        "/", "/Applications", "/Library", "/System", "/Users", "/usr", "/var", "/private", "/bin",
        "/sbin", "/etc", "/opt", "/tmp",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();

    if let Some(home) = home_dir() {
        protected.extend(candidate_roots(&home));
        protected.push(home.join("Library"));
        protected.push(home.join("Applications"));
        protected.push(home);
    }
    protected.iter().any(|p| p == path)
}

/// Vrai si le chemin est un bundle `.app`.
fn is_app_bundle(path: &Path) -> bool {
    path.extension().map(|e| e == "app").unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Process, droits administrateur, suppression
// ---------------------------------------------------------------------------

/// Échappe les caractères spéciaux d'une expression régulière étendue.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Motif pour `pkill -f` : ligne de commande contenant `<bundle>/Contents/`
/// (exécutable principal et helpers de l'app). Le premier `/` est mis entre
/// crochets pour que le motif ne matche pas sa propre ligne de commande
/// (ex. `sh -c "pkill -f …"` lancé en root).
fn process_pattern(app_path: &Path) -> String {
    let full = format!("{}/Contents/", app_path.to_string_lossy());
    match full.strip_prefix('/') {
        Some(rest) => format!("[/]{}", regex_escape(rest)),
        None => regex_escape(&full),
    }
}

/// Entoure de quotes simples pour un shell POSIX.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Littéral de chaîne AppleScript.
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Vrai si un reçu pkgutil existe pour cet identifiant.
fn receipt_exists(id: &str) -> bool {
    Command::new("pkgutil")
        .args(["--pkg-info", id])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Exécute un script shell en root, en une seule demande d'autorisation :
/// - dans un Terminal : `sudo` (mot de passe saisi dans le Terminal) ;
/// - sinon (GUI lancée par double-clic) : fenêtre d'authentification macOS
///   via `osascript … with administrator privileges`.
fn run_as_root(script: &str, log: &mut dyn FnMut(String)) -> bool {
    if cfg!(target_os = "macos") && !std::io::stdin().is_terminal() {
        let result = Command::new("osascript")
            .args(["-e", "with timeout of 3600 seconds"])
            .arg("-e")
            .arg(format!(
                "do shell script {} with administrator privileges",
                applescript_quote(script)
            ))
            .args(["-e", "end timeout"])
            .output();
        match result {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                if err.contains("-128") || err.to_lowercase().contains("cancel") {
                    log("   ⚠️  Autorisation administrateur annulée.".to_string());
                } else {
                    log(format!("   ⚠️  Échec de l'opération administrateur : {}", err.trim()));
                }
                false
            }
            Err(e) => {
                log(format!("   ⚠️  Impossible de lancer osascript : {}", e));
                false
            }
        }
    } else {
        match Command::new("sudo").args(["sh", "-c"]).arg(script).status() {
            Ok(s) if s.success() => true,
            _ => {
                log("   ⚠️  Échec ou refus de sudo.".to_string());
                false
            }
        }
    }
}

/// Supprime un fichier ou dossier sans droits particuliers. Renvoie `false`
/// en cas d'échec.
fn remove_user_path(path: &Path, log: &mut dyn FnMut(String)) -> bool {
    if !path.exists() && !path.is_symlink() {
        return true;
    }
    log(format!("🗑️  Suppression : {}", path.display()));

    let result = if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };

    match result {
        Ok(()) => true,
        Err(e) => {
            log(format!("   ⚠️  Échec de la suppression de {} : {}", path.display(), e));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_name_strips_app_path() {
        assert_eq!(search_name("/Applications/Zoom.app"), "Zoom");
        assert_eq!(search_name("/Applications/Zoom.app/"), "Zoom");
        assert_eq!(search_name("Zoom.app"), "Zoom");
        assert_eq!(search_name("Zoom"), "Zoom");
        assert_eq!(search_name("zoom.us"), "zoom.us");
    }

    #[test]
    fn quoting_for_shell_roundtrips() {
        for input in ["simple", "with space", "it's", "a\"b\\c", "$(echo pwned)", "`id`"] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(input)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), input);
        }
    }

    #[test]
    fn quoting_for_applescript() {
        assert_eq!(applescript_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn process_pattern_matches_bundle_processes_only() {
        let p = process_pattern(Path::new("/Applications/My App (beta).app"));
        assert_eq!(p, "[/]Applications/My App \\(beta\\)\\.app/Contents/");
        // Le motif ne doit pas matcher sa propre représentation.
        assert!(!p.contains("/Applications/My App (beta).app/Contents/"));
    }

    #[test]
    fn protected_paths() {
        assert!(is_protected(Path::new("/")));
        assert!(is_protected(Path::new("/Library")));
        assert!(is_protected(Path::new("/Applications")));
        assert!(!is_protected(Path::new("/Applications/Zoom.app")));
        assert!(!is_protected(Path::new("/Library/Caches/us.zoom.xos")));
    }

    fn test_dir(name: &str) -> PathBuf {
        // Dans target/ plutôt que dans le dossier temporaire (voir ci-dessous).
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cleanspace-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn app_status_of_system_and_user_apps() {
        assert_eq!(app_status(Path::new("/System/Applications/Safari.app")), RemovalStatus::Protected);
        assert_eq!(
            app_status(Path::new("/Users/someone/Applications/Nope.app")),
            RemovalStatus::Removable
        );
    }

    #[test]
    fn junk_scan_sorts_by_size_and_unchecks_apple_caches() {
        let root = test_dir("junk-scan");
        std::fs::create_dir_all(root.join("big.app.cache")).unwrap();
        std::fs::write(root.join("big.app.cache/data"), vec![1u8; 200_000]).unwrap();
        std::fs::create_dir_all(root.join("com.apple.thing")).unwrap();
        std::fs::write(root.join("com.apple.thing/data"), vec![1u8; 50_000]).unwrap();
        std::fs::write(root.join("small"), vec![1u8; 10]).unwrap();
        std::fs::write(root.join(".DS_Store"), b"x").unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();

        let spec = JunkSpec { id: "caches", name: "t", description: "t", root: root.clone() };
        let cat = scan_junk_category(&spec);

        let names: Vec<_> = cat.items.iter().map(|i| i.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["big.app.cache", "com.apple.thing", "small"]);
        assert!(cat.items[0].selected_by_default);
        assert!(!cat.items[1].selected_by_default);
        assert!(cat.items[0].size >= 200_000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_only_touches_children_of_junk_roots() {
        let root = test_dir("junk-clean");
        let outside = test_dir("junk-outside");
        std::fs::write(root.join("a"), b"x").unwrap();
        std::fs::write(outside.join("keep"), b"x").unwrap();
        let spec = JunkSpec { id: "t", name: "t", description: "t", root: root.clone() };

        let mut logs = Vec::new();
        clean_junk_in(
            &[spec],
            &[
                root.join("a"),
                root.clone(),                 // la racine elle-même : refusée
                outside.join("keep"),         // hors périmètre : refusé
                root.join("../../etc/passwd"), // évasion par `..` : refusée
            ],
            &mut |l| logs.push(l),
        );

        assert!(!root.join("a").exists());
        assert!(root.exists());
        assert!(outside.join("keep").exists());
        assert_eq!(logs.iter().filter(|l| l.contains("ignoré")).count(), 3);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn removal_deletes_user_paths_and_skips_protected() {
        // Dans target/ plutôt que dans le dossier temporaire : sous macOS, il est
        // sous /var et déclencherait une demande de droits administrateur.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cleanspace-test-{}", std::process::id()));
        let app = dir.join("Fake.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("fake"), b"x").unwrap();
        let other = dir.join("fake-cache");
        std::fs::create_dir_all(&other).unwrap();

        let mut logs = Vec::new();
        perform_removal(
            "Fake",
            None,
            &[dir.join("Fake.app"), other.clone(), PathBuf::from("/Library")],
            true,
            &mut |l| logs.push(l),
        );

        assert!(!dir.join("Fake.app").exists());
        assert!(!other.exists());
        assert!(logs.iter().any(|l| l.contains("protégé")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
