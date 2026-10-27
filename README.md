<div align="center">

<img src="assets/icon.png" width="128" alt="Icône CleanSpace" />

# CleanSpace

**Désinstallez vos apps macOS proprement. Récupérez vraiment votre espace disque.**

Glisser une app dans la corbeille laisse des centaines de Mo de caches, préférences, logs et services en arrière-plan.
CleanSpace retrouve tout ça et le supprime, en toute transparence.

[![Rust](https://img.shields.io/badge/Rust-2021-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/macOS-supported-000000?logo=apple&logoColor=white)](#-prérequis)
[![GUI](https://img.shields.io/badge/GUI-egui-blue)](https://github.com/emilk/egui)
[![License](https://img.shields.io/badge/license-MIT-green)](#-licence)

</div>

---

## ✨ Pourquoi CleanSpace ?

Quand on « désinstalle » une application macOS en la glissant dans la corbeille, seul le bundle `.app` disparaît. Restent dans le système :

- 🗂️ des dossiers dans `~/Library/Application Support`
- ⚙️ des fichiers de préférences (`.plist`)
- 💾 des caches parfois très volumineux
- 📜 des logs et états sauvegardés
- 🧩 des containers et group containers
- 🚀 des **services launchd** (LaunchAgents / LaunchDaemons) qui continuent de tourner
- 🧾 des reçus d'installation `pkgutil`

**CleanSpace** retrouve tout cela, vous le montre, et vous laisse décider de ce qui part.

## 🚀 Fonctionnalités

| | |
|---|---|
| 📱 **Liste des apps installées** | Détecte automatiquement les apps de `/Applications` et `~/Applications`, avec leur bundle id |
| 🟢 **Icônes de statut** | Pour chaque app : supprimable, demande l'admin, protégée par macOS, ou en cours d'exécution |
| 🧹 **Onglet Nettoyage** | Scanne caches, journaux, corbeille et caches de développement (Xcode, npm, Cargo, Gradle), puis nettoie en un clic |
| 💻 **Onglet Système** | Modèle du Mac, version de macOS, stockage, RAM utilisée, cycles et santé de la batterie, actualisés en direct |
| 🔍 **Analyse intelligente** | Combine les emplacements usuels et un scan récursif façon `find ~/Library /Library -iname "*nom*"` |
| ☑️ **Contrôle total** | Chaque élément trouvé a sa case à cocher : vous gardez ce que vous voulez |
| 🛑 **Arrêt propre des services** | `launchctl unload -w` **avant** la suppression des `.plist`, pour ne pas laisser de service fantôme |
| 🔪 **Fin des process** | Arrête uniquement les processus lancés depuis le bundle `.app` (jamais par simple nom) |
| 📏 **Espace récupérable** | Taille de chaque élément, total à libérer et bilan réel après suppression |
| 🧾 **Oubli du reçu pkgutil** | Optionnel, désactivable |
| 🔒 **Droits administrateur** | Éléments système signalés, puis supprimés en **une seule** demande d'autorisation : fenêtre macOS depuis la GUI, `sudo` depuis un Terminal |
| 🛡️ **Chemins protégés** | `~/Library`, `/Library`, `/Applications`… ne sont jamais supprimés, seulement leur contenu ciblé |
| 🧪 **Mode simulation** | `--dry-run` en CLI pour tout voir sans rien supprimer |
| ⚡ **Interface fluide** | Scans et suppressions dans des threads séparés : la fenêtre ne se fige jamais |

Deux interfaces, **un seul moteur** (`src/lib.rs`) : le comportement est identique que vous utilisiez la fenêtre ou le terminal.

## 📋 Prérequis

- macOS (utilise `defaults`, `launchctl`, `pkgutil`, `pkill`)
- [Rust](https://rustup.rs) (édition 2021)

> 💡 Le projet compile aussi sous Linux pour la relecture et les tests, mais les fonctionnalités ne sont réellement opérationnelles que sur macOS.

## 📦 Installation

```bash
git clone https://github.com/<votre-utilisateur>/<votre-repo>.git
cd <votre-repo>
cargo build --release
```

Les binaires sont générés dans `target/release/` :

- `clean-uninstall` : la version ligne de commande
- `clean-uninstall-gui` : la version avec interface graphique

### 📱 Créer `CleanSpace.app` (avec l'icône)

```bash
./scripts/build-app.sh               # pour l'architecture de votre Mac
./scripts/build-app.sh --universal   # Apple Silicon + Intel
```

L'application est générée dans `target/CleanSpace.app` : glissez-la dans `/Applications`. La signature est « ad hoc » (usage local) : au premier lancement, faites clic droit > Ouvrir. Pensez à remplacer `CFBundleIdentifier` dans `assets/Info.plist.template`.

L'icône est générée par `python3 assets/make_icon.py` (nécessite Pillow).

Pour installer seulement les binaires :

```bash
cp target/release/clean-uninstall target/release/clean-uninstall-gui /usr/local/bin/
```

## 🖥️ Utilisation : interface graphique

```bash
./target/release/clean-uninstall-gui
```

L'application a trois onglets : **Désinstaller**, **Nettoyage** et **Système**.

### Onglet « Désinstaller »

1. **Choisissez une app** dans la liste de gauche (avec champ de filtre), ou tapez son nom en haut puis cliquez sur **Analyser**.
2. La fenêtre affiche le `.app` trouvé, son **bundle id** et tous les fichiers annexes détectés, triés par taille.
3. **Décochez** ce que vous souhaitez conserver (les éléments 🔒 nécessitent les droits administrateur).
4. Cochez **« Je confirme vouloir supprimer définitivement… »**.
5. Cliquez sur le bouton rouge **Supprimer**.
6. Suivez la progression en direct dans le **journal**, puis lisez le bilan d'espace libéré.

**Icônes de la liste des applications :**

| Icône | Signification |
|---|---|
| 🟢 coche verte | Supprimable sans droits particuliers |
| 🟠 cadenas orange | Supprimable, mais une autorisation administrateur sera demandée |
| 🔴 barre rouge | Protégée par macOS (SIP) : ne peut pas être supprimée |
| 🔵 point bleu | L'application est en cours d'exécution : elle sera fermée avant la suppression |

> 🔐 Pour les éléments système (🔒), une fenêtre d'authentification macOS s'affiche. Si vous avez lancé l'application depuis un Terminal, c'est le mot de passe `sudo` qui est demandé dans ce Terminal.

### Onglet « Nettoyage »

1. Cliquez sur **Scanner** : les catégories apparaissent au fur et à mesure.
2. Dépliez une catégorie pour voir ses éléments, avec leur taille. Cochez ou décochez par catégorie ou élément.
3. Cochez la confirmation, puis cliquez sur **Nettoyer**.

| Catégorie | Contenu |
|---|---|
| Caches des applications | `~/Library/Caches` (les caches `com.apple.*` sont décochés par défaut) |
| Journaux et rapports de plantage | `~/Library/Logs` |
| Corbeille | `~/.Trash` (nécessite l'**accès complet au disque** pour CleanSpace) |
| Xcode | DerivedData, iOS DeviceSupport, caches des simulateurs |
| Caches de développement | npm, Cargo (registre), Gradle |

Le nettoyage ne touche que les **éléments à l'intérieur** de ces dossiers, jamais les dossiers eux-mêmes, et uniquement dans votre dossier personnel : aucun droit administrateur n'est nécessaire.

### Onglet « Système »

Affiche, en lecture seule et actualisé toutes les 10 secondes :

| Bloc | Informations |
|---|---|
| Mac | Modèle, identifiant, puce, nombre de cœurs, version de macOS (nom commercial + build), durée depuis le démarrage |
| Stockage | Espace utilisé / disponible du disque, avec jauge colorée |
| Mémoire | RAM utilisée (app + système + compressée, comme le Moniteur d'activité) et swap |
| Batterie | Niveau, état de charge, **cycles de charge**, capacité maximale et état de santé (absent sur un Mac de bureau) |

## ⌨️ Utilisation : ligne de commande

```bash
# 1. Toujours commencer par une simulation
clean-uninstall "Zoom" --dry-run

# 2. Suppression réelle (confirmation demandée)
clean-uninstall "Zoom"

# 3. Sans confirmation interactive
clean-uninstall "Zoom" --yes

# 4. Avec le chemin complet du .app
clean-uninstall "/Applications/Zoom.app"
```

### Options

| Option | Description |
|---|---|
| `--dry-run` | Liste ce qui serait supprimé, sans rien toucher |
| `-y`, `--yes` | Pas de confirmation interactive |
| `--keep-receipts` | Ne fait pas `pkgutil --forget` |
| `--depth <N>` | Profondeur du scan générique dans `~/Library` et `/Library` (défaut : `4`) |

## ⚙️ Comment ça marche

```
┌──────────────┐   ┌────────────────┐   ┌─────────────────────┐
│ 1. Trouver   │ → │ 2. Lire le     │ → │ 3. Construire des   │
│    le .app   │   │    bundle id   │   │    mots-clés        │
└──────────────┘   └────────────────┘   └─────────────────────┘
                                                   │
┌──────────────┐   ┌────────────────┐   ┌──────────▼──────────┐
│ 6. Supprimer │ ← │ 5. Arrêter     │ ← │ 4. Scanner          │
│  + oubli du  │   │  services et   │   │  emplacements usuels│
│  reçu pkgutil│   │  process       │   │  + scan générique   │
└──────────────┘   └────────────────┘   └─────────────────────┘
```

1. Recherche du bundle `.app` dans `/Applications` et `~/Applications`.
2. Lecture du `CFBundleIdentifier` via `defaults read`.
3. Génération de mots-clés : nom de l'app et segments du bundle id (ex. `us.zoom.xos` → `zoom`).
4. Scan des emplacements usuels : Application Support, Preferences, Caches, Logs, Saved Application State, Containers, Group Containers, LaunchAgents/LaunchDaemons (utilisateur et système), WebKit, HTTPStorages, Cookies, `/var/db/receipts`. S'y ajoute un scan récursif de secours.
5. Déchargement des services `launchd`, puis arrêt des process lancés depuis le bundle `.app` (`pkill -f "<app>/Contents/"`).
6. Suppression des chemins sélectionnés (droits administrateur demandés une seule fois pour les emplacements système), puis oubli du reçu `pkgutil` s'il existe.

## ⚠️ Points d'attention

- **La suppression est irréversible** : `rm -rf`, pas de corbeille. Utilisez `--dry-run` en CLI, ou relisez bien la liste cochée dans la GUI.
- **Les noms trop génériques** (ex. « Mail ») peuvent produire des faux positifs : vérifiez toujours la liste avant de confirmer.
- **Droits administrateur** : les chemins système et les LaunchDaemons nécessitent un mot de passe. Si vous l'annulez, ces éléments restent en place et sont signalés dans le bilan.
- L'outil ne supprime que ce qui correspond au nom de l'app ou à son bundle id, mais **vous restez responsable de ce que vous cochez**.

## 🗂️ Structure du projet

```
.
├── Cargo.toml
├── src
│   ├── lib.rs          # Moteur partagé : recherche, scan, launchd, suppression, nettoyage
│   ├── sysinfo.rs      # Infos système : modèle, macOS, stockage, RAM, batterie
│   └── bin
│       ├── cli.rs      # Interface ligne de commande (clap)
│       └── gui.rs      # Interface graphique (eframe / egui)
├── assets
│   ├── make_icon.py    # Génère l'icône (PNG + .icns)
│   ├── icon.png · icon-64.png · CleanSpace.icns
│   └── Info.plist.template
├── scripts
│   └── build-app.sh    # Construit CleanSpace.app
└── README.md
```

**Dépendances principales :** [`clap`](https://crates.io/crates/clap) · [`libc`](https://crates.io/crates/libc) · [`walkdir`](https://crates.io/crates/walkdir) · [`eframe` / `egui`](https://github.com/emilk/egui)

## 🛠️ Développement sous Linux (relecture / tests)

La GUI nécessite quelques bibliothèques de développement :

```bash
sudo apt install libxkbcommon-dev libgl1-mesa-dev libxcursor-dev libxrandr-dev libxi-dev libx11-dev
```

## 🗺️ Idées d'évolution

- [x] Affichage de la **taille** de chaque élément et de l'espace total récupérable
- [ ] Envoi à la **Corbeille** plutôt que suppression définitive (option)
- [x] Onglet **Nettoyage** (caches, journaux, corbeille, caches de développement)
- [ ] Nettoyage des **caches système** (`/Library/Caches`, avec droits administrateur) et recherche des **gros fichiers**
- [x] **Icône** et application `.app` (signature ad hoc) ; [ ] notarisation
- [x] Onglet **Système** (modèle, macOS, stockage, RAM, batterie)
- [ ] Binaire universel Apple Silicon / Intel via GitHub Actions

## 🤝 Contribuer

Les contributions sont les bienvenues !

1. Forkez le dépôt
2. Créez votre branche : `git checkout -b feature/ma-fonctionnalite`
3. Commitez : `git commit -m "Ajoute ma fonctionnalité"`
4. Poussez : `git push origin feature/ma-fonctionnalite`
5. Ouvrez une Pull Request

## 📄 Licence

Distribué sous licence **MIT**. Voir le fichier `LICENSE` pour plus d'informations.

---

<div align="center">

Fait avec 🦀 Rust et ❤️ pour récupérer de l'espace sur votre Mac.

</div>
