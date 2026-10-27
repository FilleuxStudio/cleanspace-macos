//! Informations système : modèle, macOS, stockage, batterie, mémoire.
//!
//! Les commandes macOS sont appelées via `Command`, et leur sortie est analysée
//! par des fonctions pures (testées sous Linux). Toute information introuvable
//! devient `None` : l'interface affiche alors « — » au lieu d'échouer.

use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Storage {
    pub total: u64,
    pub free: u64,
}

impl Storage {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub total: u64,
    pub used: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Battery {
    pub percent: Option<u8>,
    pub charging: bool,
    pub on_ac: bool,
    pub cycle_count: Option<u32>,
    /// Santé maximale en % (capacité max / capacité d'origine).
    pub health_percent: Option<u32>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemInfo {
    pub model_name: Option<String>,
    pub model_id: Option<String>,
    pub chip: Option<String>,
    pub cores: Option<u32>,
    pub macos_name: Option<String>,
    pub macos_version: Option<String>,
    pub macos_build: Option<String>,
    pub uptime_secs: Option<u64>,
    pub storage: Option<Storage>,
    pub memory: Option<Memory>,
    /// `None` : pas de batterie (Mac de bureau) ou information indisponible.
    pub battery: Option<Battery>,
}

// ───────────────────────── Collecte ─────────────────────────

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Collecte tout. Appelé depuis un thread : peut prendre ~1 s (system_profiler).
pub fn collect() -> SystemInfo {
    let mut info = SystemInfo::default();

    // Matériel : system_profiler (clés JSON stables quelle que soit la langue).
    if let Some(j) = run("system_profiler", &["-json", "SPHardwareDataType"]) {
        info.model_name = json_value(&j, "machine_name");
        info.model_id = json_value(&j, "machine_model");
        info.chip = json_value(&j, "chip_type").or_else(|| json_value(&j, "cpu_type"));
        info.cores = json_value(&j, "number_processors").and_then(|s| parse_cores(&s));
    }
    if info.model_id.is_none() {
        info.model_id = run("sysctl", &["-n", "hw.model"]).map(|s| s.trim().to_string());
    }
    if info.chip.is_none() {
        info.chip = run("sysctl", &["-n", "machdep.cpu.brand_string"]).map(|s| s.trim().to_string());
    }
    if info.cores.is_none() {
        info.cores = run("sysctl", &["-n", "hw.ncpu"]).and_then(|s| s.trim().parse().ok());
    }

    // macOS.
    if let Some(s) = run("sw_vers", &[]) {
        let (n, v, b) = parse_sw_vers(&s);
        info.macos_version = v.clone();
        info.macos_build = b;
        info.macos_name = v.as_deref().and_then(marketing_name).map(String::from).or(n);
    }

    // Démarrage.
    if let Some(s) = run("sysctl", &["-n", "kern.boottime"]) {
        if let (Some(boot), Some(now)) = (parse_boottime(&s), unix_now()) {
            info.uptime_secs = Some(now.saturating_sub(boot));
        }
    }

    info.storage = read_storage("/");

    // Mémoire.
    let total = run("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.trim().parse::<u64>().ok());
    if let Some(total) = total {
        let used = run("vm_stat", &[]).and_then(|s| parse_vm_stat(&s, total));
        let (swap_used, swap_total) = run("sysctl", &["-n", "vm.swapusage"])
            .and_then(|s| parse_swapusage(&s))
            .unwrap_or((0, 0));
        info.memory = Some(Memory {
            total,
            used: used.unwrap_or(0),
            swap_used,
            swap_total,
        });
    }

    info.battery = read_battery();
    info
}

fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn read_battery() -> Option<Battery> {
    let batt = run("pmset", &["-g", "batt"])?;
    let mut b = parse_pmset_batt(&batt)?;

    // Cycles + santé : system_profiler SPPowerDataType.
    if let Some(j) = run("system_profiler", &["-json", "SPPowerDataType"]) {
        b.cycle_count = json_value(&j, "sppower_battery_cycle_count").and_then(|s| s.parse().ok());
        b.condition = json_value(&j, "sppower_battery_health").map(|s| map_condition(&s));
        b.health_percent = json_value(&j, "sppower_battery_health_maximum_capacity")
            .and_then(|s| s.trim_end_matches('%').trim().parse().ok());
    }
    // Repli : ioreg donne les cycles et les capacités brutes.
    if b.cycle_count.is_none() || b.health_percent.is_none() {
        if let Some(io) = run("ioreg", &["-rn", "AppleSmartBattery"]) {
            if b.cycle_count.is_none() {
                b.cycle_count = ioreg_int(&io, "CycleCount").map(|v| v as u32);
            }
            if b.health_percent.is_none() {
                let max = ioreg_int(&io, "AppleRawMaxCapacity").or_else(|| ioreg_int(&io, "MaxCapacity"));
                let design = ioreg_int(&io, "DesignCapacity");
                if let (Some(m), Some(d)) = (max, design) {
                    if d > 0 && m > 0 {
                        b.health_percent = Some(((m * 100 + d / 2) / d) as u32);
                    }
                }
            }
        }
    }
    Some(b)
}

/// Espace total / libre du volume contenant `path` (via `statvfs`).
pub fn read_storage(path: &str) -> Option<Storage> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bavail as u64 * frsize;
    if total == 0 {
        return None;
    }
    Some(Storage { total, free })
}

// ───────────────────────── Analyse (fonctions pures) ─────────────────────────

/// Cherche `"key" : valeur` dans un JSON (chaîne ou nombre) sans dépendance serde.
/// Renvoie la première occurrence.
pub fn json_value(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut from = 0;
    while let Some(pos) = json[from..].find(&needle) {
        let after = &json[from + pos + needle.len()..];
        let after_trim = after.trim_start();
        if let Some(rest) = after_trim.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(s) = rest.strip_prefix('"') {
                let mut out = String::new();
                let mut chars = s.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => return Some(out),
                        '\\' => {
                            if let Some(n) = chars.next() {
                                out.push(match n {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        _ => out.push(c),
                    }
                }
                return None;
            }
            let end = rest
                .find(|c: char| c == ',' || c == '}' || c == ']' || c == '\n')
                .unwrap_or(rest.len());
            let v = rest[..end].trim();
            if v.is_empty() || v == "null" {
                return None;
            }
            return Some(v.to_string());
        }
        from += pos + needle.len();
    }
    None
}

/// « proc 8:4:4 » (Apple Silicon) ou « 8 » → nombre total de cœurs.
pub fn parse_cores(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("proc ") {
        return rest.split(':').next().and_then(|n| n.trim().parse().ok());
    }
    s.parse().ok()
}

/// `sw_vers` → (ProductName, ProductVersion, BuildVersion).
pub fn parse_sw_vers(s: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut n, mut v, mut b) = (None, None, None);
    for line in s.lines() {
        if let Some((k, val)) = line.split_once(':') {
            let val = val.trim().to_string();
            match k.trim() {
                "ProductName" => n = Some(val),
                "ProductVersion" => v = Some(val),
                "BuildVersion" => b = Some(val),
                _ => {}
            }
        }
    }
    (n, v, b)
}

/// Nom commercial à partir de la version majeure de macOS.
pub fn marketing_name(version: &str) -> Option<&'static str> {
    let major: u32 = version.split('.').next()?.parse().ok()?;
    let minor: u32 = version.split('.').nth(1).and_then(|m| m.parse().ok()).unwrap_or(0);
    Some(match major {
        10 => match minor {
            15 => "macOS Catalina",
            14 => "macOS Mojave",
            13 => "macOS High Sierra",
            _ => return None,
        },
        11 => "macOS Big Sur",
        12 => "macOS Monterey",
        13 => "macOS Ventura",
        14 => "macOS Sonoma",
        15 => "macOS Sequoia",
        26 => "macOS Tahoe",
        _ => return None,
    })
}

/// `kern.boottime` : « { sec = 1700000000, usec = 0 } Mon Nov ... » → secondes epoch.
pub fn parse_boottime(s: &str) -> Option<u64> {
    let after = s.split("sec =").nth(1)?;
    let digits: String = after.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// `vm_stat` → mémoire utilisée, comme le « Mémoire utilisée » du Moniteur d'activité :
/// app (anonymes) + filaire + compressée.
pub fn parse_vm_stat(s: &str, total: u64) -> Option<u64> {
    let first = s.lines().next()?;
    let page: u64 = first
        .split("page size of")
        .nth(1)?
        .trim()
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;

    let get = |name: &str| -> Option<u64> {
        s.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                v.trim().trim_end_matches('.').parse().ok()
            } else {
                None
            }
        })
    };

    let wired = get("Pages wired down")?;
    let compressed = get("Pages occupied by compressor").unwrap_or(0);
    // Pages « app » = anonymes - purgeables.
    let anon = get("Anonymous pages");
    let purgeable = get("Pages purgeable").unwrap_or(0);
    let app = match anon {
        Some(a) => a.saturating_sub(purgeable),
        None => {
            // Repli : actives + inactives.
            get("Pages active").unwrap_or(0) + get("Pages inactive").unwrap_or(0)
        }
    };
    Some(((app + wired + compressed) * page).min(total))
}

/// « total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted) » → (used, total) en octets.
pub fn parse_swapusage(s: &str) -> Option<(u64, u64)> {
    let grab = |key: &str| -> Option<u64> {
        let after = s.split(key).nth(1)?.trim_start().strip_prefix('=')?.trim_start();
        let tok: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .collect();
        let unit = after[tok.len()..].chars().next()?;
        let n: f64 = tok.replace(',', ".").parse().ok()?;
        let mul = match unit.to_ascii_uppercase() {
            'K' => 1024.0,
            'M' => 1024.0 * 1024.0,
            'G' => 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        };
        Some((n * mul) as u64)
    };
    Some((grab("used")?, grab("total")?))
}

/// `pmset -g batt` → pourcentage, état de charge, source. `None` si pas de batterie.
pub fn parse_pmset_batt(s: &str) -> Option<Battery> {
    let mut b = Battery::default();
    let first = s.lines().next().unwrap_or("");
    b.on_ac = first.contains("AC Power");

    let line = s.lines().find(|l| l.contains('%'))?;
    let pct_pos = line.find('%')?;
    let digits: String = line[..pct_pos]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    b.percent = digits.parse().ok();

    let lower = line.to_lowercase();
    // « discharging » contient « charging » : tester dans le bon ordre.
    b.charging = !lower.contains("discharging")
        && !lower.contains("not charging")
        && (lower.contains("charging") || lower.contains("finishing charge"));
    Some(b)
}

/// « Normal » / « Service Recommended »… → libellé français.
pub fn map_condition(s: &str) -> String {
    match s.trim() {
        "Good" | "Normal" | "Good condition" => "Normal".into(),
        "Fair" => "Correct".into(),
        "Poor" | "Check Battery" | "Service Battery" | "Service Recommended" | "Replace Soon"
        | "Replace Now" => "À remplacer".into(),
        other => other.to_string(),
    }
}

/// Valeur entière d'une clé ioreg : `"CycleCount" = 312`.
pub fn ioreg_int(s: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{}\"", key);
    s.lines().find_map(|l| {
        // Les lignes ioreg sont préfixées par « | | » : on cherche la clé n'importe où.
        let pos = l.find(&needle)?;
        let rest = l[pos + needle.len()..].trim_start().strip_prefix('=')?.trim();
        rest.parse().ok()
    })
}

/// « 3 j 4 h », « 5 h 12 min », « 7 min ».
pub fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{} j {} h", d, h)
    } else if h > 0 {
        format!("{} h {:02} min", h, m)
    } else {
        format!("{} min", m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_values() {
        let j = r#"{ "SPHardwareDataType" : [ { "chip_type" : "Apple M2 Pro",
            "machine_model" : "Mac14,12", "machine_name" : "Mac mini",
            "number_processors" : "proc 10:6:4", "n" : 42, "z" : null } ] }"#;
        assert_eq!(json_value(j, "chip_type").as_deref(), Some("Apple M2 Pro"));
        assert_eq!(json_value(j, "machine_name").as_deref(), Some("Mac mini"));
        assert_eq!(json_value(j, "n").as_deref(), Some("42"));
        assert_eq!(json_value(j, "z"), None);
        assert_eq!(json_value(j, "absent"), None);
        assert_eq!(json_value(r#"{"a":"x\"y"}"#, "a").as_deref(), Some("x\"y"));
    }

    #[test]
    fn cores() {
        assert_eq!(parse_cores("proc 10:6:4"), Some(10));
        assert_eq!(parse_cores("8"), Some(8));
        assert_eq!(parse_cores("?"), None);
    }

    #[test]
    fn sw_vers_and_names() {
        let s = "ProductName:\t\tmacOS\nProductVersion:\t\t14.5\nBuildVersion:\t\t23F79\n";
        let (n, v, b) = parse_sw_vers(s);
        assert_eq!(n.as_deref(), Some("macOS"));
        assert_eq!(v.as_deref(), Some("14.5"));
        assert_eq!(b.as_deref(), Some("23F79"));
        assert_eq!(marketing_name("14.5"), Some("macOS Sonoma"));
        assert_eq!(marketing_name("10.15.7"), Some("macOS Catalina"));
        assert_eq!(marketing_name("99.0"), None);
    }

    #[test]
    fn boottime() {
        let s = "{ sec = 1700000000, usec = 123456 } Tue Nov 14 22:13:20 2023";
        assert_eq!(parse_boottime(s), Some(1_700_000_000));
        assert_eq!(parse_boottime("rien"), None);
    }

    #[test]
    fn vm_stat() {
        let s = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                               10000.\n\
Pages active:                            200000.\n\
Pages inactive:                          190000.\n\
Pages wired down:                         50000.\n\
Pages purgeable:                          10000.\n\
Anonymous pages:                         100000.\n\
Pages occupied by compressor:             20000.\n";
        // (100000-10000 + 50000 + 20000) * 16384
        let used = parse_vm_stat(s, 16 << 30).unwrap();
        assert_eq!(used, 160_000 * 16384);
        assert_eq!(parse_vm_stat("n'importe quoi", 1), None);
    }

    #[test]
    fn swap() {
        let s = "total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted)";
        let (u, t) = parse_swapusage(s).unwrap();
        assert_eq!(t, 2048 * 1024 * 1024);
        assert_eq!(u, (1024.5 * 1024.0 * 1024.0) as u64);
        // Locale française : virgule décimale.
        assert!(parse_swapusage("total = 0,00M  used = 0,00M  free = 0,00M").is_some());
    }

    #[test]
    fn pmset() {
        let ac = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n";
        let b = parse_pmset_batt(ac).unwrap();
        assert_eq!(b.percent, Some(100));
        assert!(b.on_ac && !b.charging);

        let ch = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t54%; charging; 1:10 remaining present: true\n";
        let b = parse_pmset_batt(ch).unwrap();
        assert!(b.charging && b.on_ac);

        let dis = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t54%; discharging; 3:10 remaining present: true\n";
        let b = parse_pmset_batt(dis).unwrap();
        assert!(!b.charging && !b.on_ac);
        assert_eq!(b.percent, Some(54));

        let none = "Now drawing from 'AC Power'\n";
        assert!(parse_pmset_batt(none).is_none());
    }

    #[test]
    fn condition_and_ioreg() {
        assert_eq!(map_condition("Normal"), "Normal");
        assert_eq!(map_condition("Service Recommended"), "À remplacer");
        let io = "    | |           \"CycleCount\" = 312\n    | |           \"DesignCapacity\" = 5000\n";
        assert_eq!(ioreg_int(io, "CycleCount"), Some(312));
        assert_eq!(ioreg_int(io, "DesignCapacity"), Some(5000));
        assert_eq!(ioreg_int(io, "Nope"), None);
    }

    #[test]
    fn uptime() {
        assert_eq!(format_uptime(300), "5 min");
        assert_eq!(format_uptime(3 * 3600 + 5 * 60), "3 h 05 min");
        assert_eq!(format_uptime(2 * 86_400 + 4 * 3600), "2 j 4 h");
    }

    #[test]
    fn storage_root() {
        let s = read_storage("/").expect("statvfs /");
        assert!(s.total > 0 && s.free <= s.total);
        assert!(read_storage("/chemin/inexistant/xyz").is_none());
    }
}
