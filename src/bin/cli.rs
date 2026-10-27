use clap::Parser;
use clean_uninstall_core::{analyze, perform_removal, RemovalStatus};
use std::io::{self, Write};

/// Désinstalleur propre pour macOS : supprime une app, ses services (LaunchAgents/
/// LaunchDaemons), ses préférences, caches, logs, containers, etc.
#[derive(Parser, Debug)]
#[command(
    name = "clean-uninstall",
    about = "Désinstalle proprement une application macOS et tous ses fichiers associés",
    version
)]
struct Args {
    /// Nom de l'application (ex: "Zoom", "zoom.us") ou chemin complet vers le .app
    app: String,

    /// N'affiche que ce qui serait supprimé, ne supprime rien
    #[arg(long)]
    dry_run: bool,

    /// Ne pas demander de confirmation avant de supprimer
    #[arg(short = 'y', long)]
    yes: bool,

    /// Conserver le reçu du package (ne pas faire `pkgutil --forget`)
    #[arg(long)]
    keep_receipts: bool,

    /// Profondeur de recherche annexe dans ~/Library et /Library (défaut: 4)
    #[arg(long, default_value_t = 4)]
    depth: usize,
}

fn main() {
    let args = Args::parse();

    if !cfg!(target_os = "macos") {
        eprintln!("⚠️  Cet outil est conçu pour macOS (utilise defaults/launchctl/pkgutil/pkill).\n");
    }

    println!("== Désinstallation de : {} ==\n", args.app);

    let result = analyze(&args.app, args.depth);

    match &result.app_path {
        Some(p) => {
            println!("📦 Application trouvée : {}", p.display());
            match result.app_status {
                Some(RemovalStatus::Protected) => {
                    println!("   ⛔ Application protégée par macOS : elle ne sera pas supprimée.")
                }
                Some(RemovalStatus::NeedsAdmin) => {
                    println!("   🔒 Une autorisation administrateur sera demandée.")
                }
                _ => {}
            }
        }
        None => {
            println!("⚠️  Aucun .app trouvé dans /Applications ou ~/Applications pour \"{}\".", args.app);
            println!("   La recherche continue par nom sur les fichiers annexes uniquement.\n");
        }
    }

    match &result.bundle_id {
        Some(id) => println!("🆔 Bundle identifier : {}", id),
        None => println!("🆔 Bundle identifier : (inconnu)"),
    }
    println!();

    if result.items.is_empty() {
        println!("Aucun fichier annexe trouvé.");
    } else {
        println!("📋 Éléments trouvés ({}) :", result.items.len());
        for item in &result.items {
            let tag = if item.needs_sudo { " (sudo)" } else { "" };
            println!("   - {}{}", item.path.display(), tag);
        }
    }
    println!();

    if args.dry_run {
        println!("(mode --dry-run : rien n'a été supprimé)");
        return;
    }

    if result.items.is_empty() {
        println!("Rien à supprimer.");
        return;
    }

    if !args.yes
        && !confirm(&format!(
            "⚠️  Confirmer la suppression DÉFINITIVE de ces {} éléments ? [o/N] ",
            result.items.len()
        ))
    {
        println!("Annulé.");
        return;
    }

    let paths: Vec<_> = result.items.iter().map(|i| i.path.clone()).collect();
    perform_removal(
        &args.app,
        result.bundle_id.as_deref(),
        &paths,
        args.keep_receipts,
        &mut |line| println!("{}", line),
    );
}

fn confirm(prompt: &str) -> bool {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "o" | "oui" | "y" | "yes")
}