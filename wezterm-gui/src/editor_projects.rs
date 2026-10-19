//! The folders VS Code and the editors built from it have opened, and the
//! ones coding agents -- Claude Code, Codex, Pi -- and cmux worked in, for
//! the Import page and first-run setup to offer as projects.
//!
//! Each of these editors keeps a directory per opened folder under
//! `<user data>/User/workspaceStorage`, holding a `workspace.json` that
//! names the folder by URI, and the directory's last change says when the
//! folder was last used. Whether a folder is still there is the caller's to
//! ask: on macOS, looking at one in Documents or on the Desktop can put a
//! permission prompt in front of someone who has not asked to import.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};
use crate::ui::draw::DrawContext;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use window::color::LinearRgba;

/// Editors known by their user-data directory, with what to call them.
/// Only these directories are looked in: on macOS, looking inside another
/// app's can ask the user for that app's data -- their contacts, say.
const KNOWN: &[(&str, &str, EditorKind)] = &[
    ("Code", "VS Code", EditorKind::VsCode),
    ("Code - Insiders", "VS Code Insiders", EditorKind::VsCode),
    ("Code - OSS", "Code - OSS", EditorKind::Other),
    ("VSCodium", "VSCodium", EditorKind::VsCodium),
    ("VSCodium - Insiders", "VSCodium Insiders", EditorKind::VsCodium),
    ("Cursor", "Cursor", EditorKind::Cursor),
    ("Windsurf", "Windsurf", EditorKind::Windsurf),
    ("Antigravity", "Antigravity", EditorKind::Other),
    ("Kiro", "Kiro", EditorKind::Other),
    ("Positron", "Positron", EditorKind::Other),
    ("Trae", "Trae", EditorKind::Other),
    ("Void", "Void", EditorKind::Other),
];

/// Opened folders read per editor, newest first: an install keeps one
/// directory per folder it ever opened, and only the recent ones matter.
const PER_EDITOR: usize = 400;

/// A `workspace.json` larger than this is not one; it is not read.
const MAX_WORKSPACE_JSON: u64 = 64 * 1024;

/// How recent a folder's last use has to be for its box to start ticked.
pub(crate) const RECENT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorKind {
    VsCode,
    VsCodium,
    Cursor,
    Windsurf,
    Other,
    Claude,
    Codex,
    Pi,
    Cmux,
}

impl EditorKind {
    /// Its own mark, where it has one in colour.
    fn brand(self) -> Option<BrandIcon> {
        match self {
            Self::Cursor => Some(BrandIcon::Cursor),
            Self::Windsurf => Some(BrandIcon::Windsurf),
            Self::VsCodium => Some(BrandIcon::VSCodium),
            Self::Claude => Some(BrandIcon::AgentClaude),
            _ => None,
        }
    }

    /// The colour its glyph is drawn in: white, but for cmux's blue
    /// chevron on its light tile.
    fn glyph_color(self) -> LinearRgba {
        match self {
            Self::Cmux => LinearRgba::with_srgba(0x3B, 0x7B, 0xF6, 0xFF),
            _ => LinearRgba::with_srgba(0xFF, 0xFF, 0xFF, 0xFF),
        }
    }

    /// The glyph drawn where it has no mark in colour, in `glyph_color`.
    fn glyph(self) -> SvgIcon {
        match self {
            Self::VsCode => SvgIcon::VsCode,
            Self::Codex => SvgIcon::AgentCodex,
            Self::Pi => SvgIcon::AgentPi,
            Self::Cmux => SvgIcon::Cmux,
            _ => SvgIcon::CodeXml,
        }
    }

    /// The colour of the tile its mark sits on.
    fn tile(self) -> LinearRgba {
        let (r, g, b) = match self {
            Self::VsCode => (0x00, 0x7A, 0xCC),
            Self::VsCodium => (0x1B, 0x2B, 0x44),
            Self::Cursor => (0x14, 0x14, 0x14),
            Self::Windsurf => (0x0B, 0x10, 0x0F),
            Self::Other => (0x48, 0x48, 0x4A),
            Self::Claude => (0x2B, 0x2B, 0x30),
            Self::Codex => (0x14, 0x14, 0x14),
            Self::Pi => (0x1F, 0x1F, 0x22),
            Self::Cmux => (0xEE, 0xEF, 0xF2),
        };
        LinearRgba::with_srgba(r, g, b, 0xFF)
    }
}

/// How far each tile in [`draw_stack`] starts after the one before, as a
/// share of its side.
const STACK_STEP: f32 = 0.7;

/// The kinds a stack shows: each once, in the order given, at most three.
pub(crate) fn stack_kinds(kinds: impl IntoIterator<Item = EditorKind>) -> Vec<EditorKind> {
    let mut stack = Vec::new();
    for kind in kinds {
        if stack.len() == 3 {
            break;
        }
        if !stack.contains(&kind) {
            stack.push(kind);
        }
    }
    stack
}

/// How wide [`draw_stack`] draws `count` tiles of side `tile`.
pub(crate) fn stack_width(count: usize, tile: f32) -> f32 {
    let tile = tile.round();
    tile + (tile * STACK_STEP).round() * count.saturating_sub(1) as f32
}

/// Editors' marks on tiles of side `tile`, from `x` rightwards, the first
/// in front and each ringed with `ground` -- what they sit on -- so the
/// ones behind show where they are cut. The tiles are lit as Settings
/// lights the squares heading its rows; `dark` is the chrome's appearance.
/// All on the top layer, where the marks are, so a tile in front hides the
/// mark behind it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_stack(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    kinds: &[EditorKind],
    x: f32,
    y: f32,
    tile: f32,
    ground: LinearRgba,
    dark: bool,
) -> anyhow::Result<()> {
    // Whole pixels, so the marks come out sharp.
    let (x, y, tile) = (x.round(), y.round(), tile.round());
    let step = (tile * STACK_STEP).round();
    let ring = (tile * 0.05).round().max(1.0);
    let radius = tile * 0.27;
    let mark = (tile * 0.56).round();
    for (index, kind) in kinds.iter().enumerate().rev() {
        let tile_x = x + step * index as f32;
        ctx.draw_rounded_rect(
            layers,
            2,
            tile_x - ring,
            y - ring,
            tile + ring * 2.0,
            tile + ring * 2.0,
            ground,
            radius + ring,
        )?;
        crate::ui::tile::draw_tile(
            ctx,
            layers,
            2,
            tile_x,
            y,
            tile,
            radius,
            kind.tile(),
            dark,
            &crate::settings_window::ROW_TILE,
        )?;
        let mark_x = tile_x + (tile - mark) / 2.0;
        let mark_y = y + (tile - mark) / 2.0;
        match kind.brand() {
            Some(brand) => ctx.draw_brand_icon(layers, brand, mark_x, mark_y, mark)?,
            None => ctx.draw_svg_icon(
                layers,
                kind.glyph(),
                mark_x,
                mark_y,
                mark,
                kind.glyph_color(),
            )?,
        }
    }
    Ok(())
}

/// The most marks [`draw_ring`] sets round its circle.
pub(crate) const RING_MAX: usize = 6;

/// The kinds a ring shows: each once, in the order given, at most
/// [`RING_MAX`].
pub(crate) fn ring_kinds(kinds: impl IntoIterator<Item = EditorKind>) -> Vec<EditorKind> {
    let mut ring = Vec::new();
    for kind in kinds {
        if ring.len() == RING_MAX {
            break;
        }
        if !ring.contains(&kind) {
            ring.push(kind);
        }
    }
    ring
}

/// Editors' marks on round tiles set round a circle in the square of side
/// `side` at `x`, `y`: the first at the top, the rest clockwise, each over
/// the one after it and ringed with `ground` -- what they sit on -- so the
/// edge it covers shows. A single kind is a plain square tile filling the
/// square. `dark` is the chrome's appearance.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_ring(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    kinds: &[EditorKind],
    x: f32,
    y: f32,
    side: f32,
    ground: LinearRgba,
    dark: bool,
) -> anyhow::Result<()> {
    if let [only] = kinds {
        return draw_stack(ctx, layers, &[*only], x, y, side, ground, dark);
    }
    let count = kinds.len() as f32;
    // Fewer marks have room to be larger; each still just overlaps the
    // next round the circle.
    let tile = (side
        * match kinds.len() {
            2 => 0.52,
            3 => 0.5,
            4 => 0.47,
            5 => 0.43,
            _ => 0.39,
        })
    .round();
    let orbit = (side - tile) / 2.0;
    let ring = (tile * 0.08).round().max(1.0);
    let hair = ctx.px(1.0).round().max(1.0);
    let mark = (tile * 0.6).round();
    let (cx, cy) = (x + side / 2.0, y + side / 2.0);
    for (index, kind) in kinds.iter().enumerate().rev() {
        let angle = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * index as f32 / count;
        let tile_x = (cx + orbit * angle.cos() - tile / 2.0).round();
        let tile_y = (cy + orbit * angle.sin() - tile / 2.0).round();
        ctx.draw_rounded_rect(
            layers,
            2,
            tile_x - ring,
            tile_y - ring,
            tile + ring * 2.0,
            tile + ring * 2.0,
            ground,
            tile / 2.0 + ring,
        )?;
        // A faint edge, so a dark tile still reads as round on a dark card.
        ctx.draw_rounded_rect(
            layers,
            2,
            tile_x,
            tile_y,
            tile,
            tile,
            LinearRgba::with_components(1.0, 1.0, 1.0, 0.16),
            tile / 2.0,
        )?;
        let inner = tile - hair * 2.0;
        crate::ui::tile::draw_tile(
            ctx,
            layers,
            2,
            tile_x + hair,
            tile_y + hair,
            inner,
            inner / 2.0,
            kind.tile(),
            dark,
            &crate::settings_window::ROW_TILE,
        )?;
        let mark_x = tile_x + (tile - mark) / 2.0;
        let mark_y = tile_y + (tile - mark) / 2.0;
        match kind.brand() {
            Some(brand) => ctx.draw_brand_icon(layers, brand, mark_x, mark_y, mark)?,
            None => ctx.draw_svg_icon(
                layers,
                kind.glyph(),
                mark_x,
                mark_y,
                mark,
                kind.glyph_color(),
            )?,
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Editor {
    pub name: String,
    pub kind: EditorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Folder {
    pub path: PathBuf,
    pub name: String,
    /// Every editor that opened it, by index into `Scan::editors`, the most
    /// recent first.
    pub editors: Vec<usize>,
    pub last_used: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Scan {
    /// The editors with a folder here to offer: the ones used this week
    /// first, then the ones in the most folders, then the latest used.
    pub editors: Vec<Editor>,
    /// One per folder, the most recently used first.
    pub folders: Vec<Folder>,
    /// Folders the editors opened on another machine -- over SSH, in WSL or
    /// a container -- which are not offered yet.
    pub remote: usize,
}

impl Scan {
    pub fn folders_in(&self, editor: Option<usize>) -> impl Iterator<Item = (usize, &Folder)> {
        self.folders
            .iter()
            .enumerate()
            .filter(move |(_, folder)| editor.map_or(true, |e| folder.editors.contains(&e)))
    }
}

/// The kinds of editor this machine has, each once, looked up the first
/// time they are asked for: the Import page offers its card only when
/// there is one. Nothing an editor opened is read for this.
pub(crate) fn installed() -> &'static [EditorKind] {
    static INSTALLED: std::sync::OnceLock<Vec<EditorKind>> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let mut kinds = Vec::new();
        if let Some(root) = user_data_root() {
            for (_, kind, _) in editor_storages(&root) {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        kinds.extend(
            AGENTS
                .iter()
                .filter(|agent| (agent.present)(&config::HOME_DIR))
                .map(|agent| agent.kind),
        );
        kinds
    })
}

/// What this machine's editors have opened, each folder as `resolve` names
/// it: two it names alike are one folder, and one it names None -- no
/// longer there, say -- is left out.
pub(crate) fn scan(resolve: &dyn Fn(&Path) -> Option<PathBuf>) -> Scan {
    scan_roots(user_data_root().as_deref(), &config::HOME_DIR, resolve)
}

/// Where the editors keep their user data.
fn user_data_root() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(config::HOME_DIR.join("Library").join("Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| Some(config::HOME_DIR.join(".config")))
    }
}

/// One opened folder as an editor recorded it.
struct Opened {
    editor: usize,
    last_used: SystemTime,
    uri: String,
}

/// The editors under `root`, the agents under `home`, and what they
/// opened, as for [`scan`].
fn scan_roots(
    root: Option<&Path>,
    home: &Path,
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Scan {
    let mut editors = Vec::new();
    let mut opened = Vec::new();
    for (name, kind, storage) in root.map(editor_storages).unwrap_or_default() {
        let index = editors.len();
        editors.push(Editor { name, kind });
        for (workspace_json, last_used) in newest_entries(&storage) {
            if let Some(uri) = read_folder_uri(&workspace_json) {
                opened.push(Opened {
                    editor: index,
                    last_used,
                    uri,
                });
            }
        }
    }
    for agent in AGENTS {
        let worked_in: Vec<_> = (agent.read)(home)
            .into_iter()
            .filter(|(path, _)| !is_scratch(path))
            .collect();
        if worked_in.is_empty() {
            continue;
        }
        let index = editors.len();
        editors.push(Editor {
            name: agent.name.to_string(),
            kind: agent.kind,
        });
        for (path, last_used) in worked_in {
            if let Ok(uri) = url::Url::from_directory_path(&path) {
                opened.push(Opened {
                    editor: index,
                    last_used,
                    uri: uri.to_string(),
                });
            }
        }
    }
    merge(editors, opened, resolve)
}

/// A coding agent, or a terminal kept for them: what to call it, whether
/// this machine has used it, and the folders it worked in with when each
/// was last used. Only where each keeps its record of those folders is read
/// -- never a conversation.
struct Agent {
    name: &'static str,
    kind: EditorKind,
    present: fn(&Path) -> bool,
    read: fn(&Path) -> Vec<(PathBuf, SystemTime)>,
}

const AGENTS: &[Agent] = &[
    Agent {
        name: "Claude Code",
        kind: EditorKind::Claude,
        present: |home| home.join(".claude.json").is_file(),
        read: claude_folders,
    },
    Agent {
        name: "Codex",
        kind: EditorKind::Codex,
        present: |home| home.join(".codex").join("sessions").is_dir(),
        read: codex_folders,
    },
    Agent {
        name: "Pi",
        kind: EditorKind::Pi,
        present: |home| pi_sessions(home).is_dir(),
        read: pi_folders,
    },
    Agent {
        name: "cmux",
        kind: EditorKind::Cmux,
        present: |home| cmux_session(home).is_file(),
        read: cmux_folders,
    },
];

/// A file read whole for a list of folders is not one past this.
const MAX_AGENT_FILE: u64 = 64 * 1024 * 1024;
/// How much of a session's first line is read for its folder: Codex puts
/// its whole system prompt on that line, after the folder.
const SESSION_HEAD: usize = 64 * 1024;
/// How much of it is read first: the folder comes early on the line.
const SESSION_PEEK: usize = 4 * 1024;

fn modified(path: &Path) -> Option<SystemTime> {
    path.metadata().and_then(|meta| meta.modified()).ok()
}

/// Claude Code lists every folder it was started in under `projects` in
/// `~/.claude.json`, and keeps a directory of sessions per folder under
/// `~/.claude/projects`, named for the path with every other character
/// than a letter or a digit made `-`. Its newest session is the folder's
/// last use; a folder whose sessions are gone is not offered. (The
/// directory's own time is not it: clearing old sessions changes it, and
/// adding to a session does not.)
fn claude_folders(home: &Path) -> Vec<(PathBuf, SystemTime)> {
    /// Only the folders' names are wanted, not what is kept for each.
    #[derive(serde::Deserialize)]
    struct Config {
        #[serde(default)]
        projects: HashMap<String, serde::de::IgnoredAny>,
    }
    let config = home.join(".claude.json");
    if config.metadata().map_or(true, |meta| meta.len() > MAX_AGENT_FILE) {
        return Vec::new();
    }
    let Some(config) = std::fs::read(&config)
        .ok()
        .and_then(|text| serde_json::from_slice::<Config>(&text).ok())
    else {
        return Vec::new();
    };
    let sessions = home.join(".claude").join("projects");
    config
        .projects
        .into_keys()
        .filter_map(|path| {
            let encoded: String = path
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let (last_used, _) = newest_session(&sessions.join(encoded))?;
            Some((PathBuf::from(path), last_used))
        })
        .collect()
}

/// The newest `.jsonl` session directly in `dir`, and when it last changed.
fn newest_session(dir: &Path) -> Option<(SystemTime, PathBuf)> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|session| session.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| Some((modified(&path)?, path)))
        .max_by_key(|(when, _)| *when)
}

/// Codex writes a session per run under `~/.codex/sessions/<y>/<m>/<d>`,
/// its first line naming the folder (`cwd`). The newest are read.
fn codex_folders(home: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut files = Vec::new();
    let mut dirs = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(when) = modified(&path) {
                    files.push((path, when));
                }
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.truncate(PER_EDITOR);
    files
        .into_iter()
        .filter_map(|(file, when)| Some((PathBuf::from(session_cwd(&file)?), when)))
        .collect()
}

fn pi_sessions(home: &Path) -> PathBuf {
    home.join(".pi").join("agent").join("sessions")
}

/// Pi keeps a directory of sessions per folder under
/// `~/.pi/agent/sessions`, each session's first line naming the folder
/// (`cwd`). The newest session of each directory is read.
fn pi_folders(home: &Path) -> Vec<(PathBuf, SystemTime)> {
    std::fs::read_dir(pi_sessions(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let (last_used, newest) = newest_session(&entry.path())?;
            Some((PathBuf::from(session_cwd(&newest)?), last_used))
        })
        .collect()
}

fn cmux_session(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("cmux")
        .join("session-com.cmuxterm.app.json")
}

/// cmux saves its windows to one file to bring back on relaunch: each
/// workspace's folder, and each of its terminals'. The file's last save is
/// when they were last used.
fn cmux_folders(home: &Path) -> Vec<(PathBuf, SystemTime)> {
    #[derive(serde::Deserialize)]
    struct Session {
        #[serde(default)]
        windows: Vec<Window>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Window {
        tab_manager: Option<TabManager>,
    }
    #[derive(serde::Deserialize)]
    struct TabManager {
        #[serde(default)]
        workspaces: Vec<Workspace>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Workspace {
        current_directory: Option<String>,
        #[serde(default)]
        panels: Vec<Panel>,
    }
    #[derive(serde::Deserialize)]
    struct Panel {
        directory: Option<String>,
    }
    let path = cmux_session(home);
    let Some(saved) = path
        .metadata()
        .ok()
        .filter(|meta| meta.len() <= MAX_AGENT_FILE)
        .and_then(|meta| meta.modified().ok())
    else {
        return Vec::new();
    };
    let Some(session) = std::fs::read(&path)
        .ok()
        .and_then(|text| serde_json::from_slice::<Session>(&text).ok())
    else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = Vec::new();
    for workspace in session
        .windows
        .into_iter()
        .filter_map(|window| window.tab_manager)
        .flat_map(|tabs| tabs.workspaces)
    {
        let dirs = workspace
            .current_directory
            .into_iter()
            .chain(workspace.panels.into_iter().filter_map(|panel| panel.directory));
        for dir in dirs.filter(|dir| !dir.is_empty()).map(PathBuf::from) {
            if !folders.contains(&dir) {
                folders.push(dir);
            }
        }
    }
    folders.into_iter().map(|dir| (dir, saved)).collect()
}

/// The folder a session file's first line names, as `"cwd": "..."`, read
/// from at most the start of the file. It is near the start, so a little
/// is read first and more only when that falls short.
fn session_cwd(file: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    let mut file = std::fs::File::open(file).ok()?;
    for limit in [SESSION_PEEK, SESSION_HEAD] {
        let want = (limit - head.len()) as u64;
        let read = (&mut file).take(want).read_to_end(&mut head).ok()?;
        let line = head.split(|&b| b == b'\n').next()?;
        if let Some(cwd) = json_string_after(&String::from_utf8_lossy(line), "\"cwd\"") {
            return Some(cwd);
        }
        // The first line ended, or the file did, without one.
        if line.len() < head.len() || (read as u64) < want {
            return None;
        }
    }
    None
}

/// The JSON string that follows `key` and a colon in `text`.
fn json_string_after(text: &str, key: &str) -> Option<String> {
    let rest = text[text.find(key)? + key.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    // The string ends at the first quote not escaped by a backslash.
    let mut escaped = false;
    for (index, c) in rest.char_indices().skip(1) {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return serde_json::from_str(&rest[..=index]).ok(),
            _ => escaped = false,
        }
    }
    None
}

/// Each known editor's `workspaceStorage` under `root`, where it has one.
fn editor_storages(root: &Path) -> Vec<(String, EditorKind, PathBuf)> {
    KNOWN
        .iter()
        .map(|(dir, name, kind)| {
            let storage = root.join(dir).join("User").join("workspaceStorage");
            (name.to_string(), *kind, storage)
        })
        .filter(|(.., storage)| storage.is_dir())
        .collect()
}

/// The `workspace.json` of the most recently used entries in `storage`,
/// with when each was last used: the later of its directory's change and
/// its state database's, which the editor writes as the folder is used.
fn newest_entries(storage: &Path) -> Vec<(PathBuf, SystemTime)> {
    let modified = |path: &Path| path.metadata().and_then(|m| m.modified()).ok();
    let mut entries: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(storage)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let last_used = modified(&dir)?.max(
                modified(&dir.join("state.vscdb")).unwrap_or(SystemTime::UNIX_EPOCH),
            );
            Some((dir.join("workspace.json"), last_used))
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(PER_EDITOR);
    entries
}

/// The folder a `workspace.json` names, as the URI it gives. A multi-root
/// workspace names a `.code-workspace` file instead and is skipped.
fn read_folder_uri(workspace_json: &Path) -> Option<String> {
    if workspace_json.metadata().ok()?.len() > MAX_WORKSPACE_JSON {
        return None;
    }
    let text = std::fs::read(workspace_json).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&text).ok()?;
    value.get("folder")?.as_str().map(str::to_string)
}

/// One folder per path `resolve` names, the most recent use deciding its
/// time and the order its editors are listed in. Editors with nothing left
/// to offer are dropped, and the indexes renumbered.
fn merge(
    editors: Vec<Editor>,
    mut opened: Vec<Opened>,
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Scan {
    opened.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    let mut remote = HashSet::new();
    let mut folders: Vec<Folder> = Vec::new();
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    // Each path resolved once, however many editors opened it.
    let mut resolved: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    // Each app's own last use of a folder it is offered for: a folder's
    // time is its latest use by any of them.
    let mut app_last: HashMap<usize, SystemTime> = HashMap::new();
    for item in opened {
        let Some(local) = local_path(&item.uri) else {
            // Opened on another machine: an SSH host, WSL, a container.
            if item.uri.contains("://") {
                remote.insert(item.uri);
            }
            continue;
        };
        let Some(path) = resolved
            .entry(local)
            .or_insert_with_key(|local| resolve(local))
            .clone()
        else {
            continue;
        };
        let last = app_last.entry(item.editor).or_insert(item.last_used);
        *last = (*last).max(item.last_used);
        if let Some(&index) = by_path.get(&path) {
            let folder = &mut folders[index];
            if !folder.editors.contains(&item.editor) {
                folder.editors.push(item.editor);
            }
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        by_path.insert(path.clone(), folders.len());
        folders.push(Folder {
            path,
            name,
            editors: vec![item.editor],
            last_used: item.last_used,
        });
    }

    // The apps in the order they are shown -- in the stack of marks, the
    // filters and each folder's marks: those used in the last week first,
    // by how many folders they were used in, then by their last use.
    let now = SystemTime::now();
    let mut order: Vec<(usize, bool, usize, SystemTime)> = (0..editors.len())
        .filter_map(|index| {
            let last = *app_last.get(&index)?;
            let used = folders
                .iter()
                .filter(|folder| folder.editors.contains(&index))
                .count();
            let this_week = now.duration_since(last).map_or(true, |age| age <= THIS_WEEK);
            Some((index, this_week, used, last))
        })
        .collect();
    order.sort_by(|a, b| (b.1, b.2, b.3).cmp(&(a.1, a.2, a.3)));
    let mut renumber = HashMap::new();
    let mut slots: Vec<Option<Editor>> = editors.into_iter().map(Some).collect();
    let mut kept = Vec::new();
    for (index, ..) in order {
        renumber.insert(index, kept.len());
        kept.extend(slots[index].take());
    }
    for folder in &mut folders {
        for editor in &mut folder.editors {
            *editor = renumber[editor];
        }
    }
    Scan {
        editors: kept,
        folders,
        remote: remote.len(),
    }
}

/// How recent an app's last use has to be to come before the ones used in
/// more folders but not lately.
const THIS_WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A folder an agent worked in that no one means to keep working in: the
/// root, the home folder itself or its Desktop, Documents and Downloads --
/// though not the folders in them -- or one under a temporary directory,
/// where agents run for tests and scratch work.
fn is_scratch(path: &Path) -> bool {
    let temp = std::env::temp_dir();
    let home: &Path = &config::HOME_DIR;
    path.parent().is_none()
        || path == home
        || ["Desktop", "Documents", "Downloads"]
            .iter()
            .any(|dir| path == home.join(dir))
        || path.starts_with(&temp)
        || ["/tmp", "/private/tmp", "/private/var/folders", "/var/folders"]
            .iter()
            .any(|dir| path.starts_with(dir))
}

/// A `file:` URI as the path it names on this machine. One naming another
/// machine -- `file://wsl.localhost/...` -- is not this machine's.
fn local_path(uri: &str) -> Option<PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" || url.host_str().is_some_and(|host| !host.is_empty()) {
        return None;
    }
    url.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days_ago: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days_ago * 24 * 60 * 60)
    }

    fn opened(editor: usize, days_ago: u64, path: &Path) -> Opened {
        Opened {
            editor,
            last_used: at(days_ago),
            uri: url::Url::from_directory_path(path).unwrap().to_string(),
        }
    }

    fn editor(name: &str) -> Editor {
        Editor {
            name: name.to_string(),
            kind: EditorKind::Other,
        }
    }

    fn as_given(path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    #[test]
    fn a_folder_opened_in_two_editors_is_offered_once_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let api = root.path().join("api");
        let web = root.path().join("web");
        let scan = merge(
            vec![editor("VS Code"), editor("Cursor")],
            vec![
                opened(0, 9, &api),
                opened(1, 2, &api),
                opened(0, 1, &web),
            ],
            &as_given,
        );
        let paths: Vec<_> = scan.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, [web, api]);
        // Cursor opened it last, so it is named first.
        assert_eq!(scan.folders[1].editors, [1, 0]);
        assert_eq!(scan.folders[1].name, "api");
        assert_eq!(scan.folders_in(Some(1)).count(), 1);
        assert_eq!(scan.folders_in(None).count(), 2);
    }

    #[test]
    fn removed_and_remote_folders_are_not_offered() {
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("gone");
        let here = root.path().join("here");
        let scan = merge(
            vec![editor("VS Code"), editor("Windsurf")],
            vec![
                opened(0, 1, &gone),
                opened(0, 2, &here),
                Opened {
                    editor: 1,
                    last_used: at(3),
                    uri: "vscode-remote://ssh-remote%2Bserver-a/home/user/app".into(),
                },
                Opened {
                    editor: 1,
                    last_used: at(4),
                    uri: "vscode-remote://ssh-remote%2Bserver-a/home/user/app".into(),
                },
            ],
            &|path| (path != gone).then(|| path.to_path_buf()),
        );
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.remote, 1, "the same remote folder counts once");
        // Windsurf had only the remote folder: no pill for it.
        assert_eq!(scan.editors, [editor("VS Code")]);
        assert_eq!(scan.folders[0].editors, [0]);
    }

    #[test]
    fn editors_are_found_by_their_storage_and_read_from_workspace_json() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let write = |dir: &str, entry: &str, json: String| {
            let storage = root.path().join(dir).join("User").join("workspaceStorage").join(entry);
            std::fs::create_dir_all(&storage).unwrap();
            std::fs::write(storage.join("workspace.json"), json).unwrap();
        };
        let uri = url::Url::from_directory_path(project.path()).unwrap();
        write("Cursor", "a", format!(r#"{{"folder":"{uri}"}}"#));
        write("Trae", "b", format!(r#"{{"folder":"{uri}"}}"#));
        write("Code", "c", r#"{"workspace":"file:///code/team.code-workspace"}"#.into());
        // Laid out like an editor but not a known one: not looked in.
        write("Some Fork", "d", format!(r#"{{"folder":"{uri}"}}"#));

        let home = tempfile::tempdir().unwrap();
        let scan = scan_roots(Some(root.path()), home.path(), &|path: &Path| {
            path.is_dir().then(|| path.to_path_buf())
        });
        let mut names: Vec<_> = scan.editors.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        // VS Code opened only a multi-root workspace, which is skipped.
        assert_eq!(names, ["Cursor", "Trae"]);
        let cursor = scan.editors.iter().find(|e| e.name == "Cursor").unwrap();
        assert_eq!(cursor.kind, EditorKind::Cursor);
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.folders[0].editors.len(), 2);
    }

    #[test]
    fn two_spellings_of_one_folder_are_offered_once() {
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("alias");
        let canonical = root.path().join("canonical");
        let resolve = |path: &Path| {
            Some(match path.strip_prefix(&alias) {
                Ok(rest) => canonical.join(rest),
                Err(_) => path.to_path_buf(),
            })
        };
        let scan = merge(
            vec![editor("VS Code"), editor("Cursor")],
            vec![
                opened(0, 1, &alias.join("demo")),
                opened(1, 2, &canonical.join("demo")),
            ],
            &resolve,
        );
        assert_eq!(scan.folders.len(), 1);
        assert_eq!(scan.folders[0].path, canonical.join("demo"));
        assert_eq!(scan.folders[0].editors, [0, 1]);
    }

    #[test]
    fn a_file_uri_naming_another_machine_is_not_a_local_folder() {
        let scan = merge(
            vec![editor("VS Code")],
            vec![Opened {
                editor: 0,
                last_used: at(1),
                uri: "file://wsl.localhost/Alpine/home/user/app".into(),
            }],
            &as_given,
        );
        assert!(scan.folders.is_empty());
        assert_eq!(scan.remote, 1);
    }

    #[test]
    fn a_sessions_folder_is_read_from_the_start_of_its_first_line() {
        let line = r#"{"type":"session_meta","payload":{"id":"x","cwd":"/home/user/a \"b\"","base":"long"}}"#;
        assert_eq!(
            json_string_after(line, "\"cwd\"").as_deref(),
            Some("/home/user/a \"b\"")
        );
        assert_eq!(json_string_after(r#"{"cwd": 3}"#, "\"cwd\""), None);
        assert_eq!(json_string_after(r#"{"other":"x"}"#, "\"cwd\""), None);
    }

    #[test]
    fn the_agents_folders_are_read_where_each_keeps_them() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        // Claude Code: listed, and with sessions; listed without is old.
        write(
            home.join(".claude.json"),
            r#"{"projects":{"/home/user/api":{},"/home/user/old":{}}}"#,
        );
        write(home.join(".claude/projects/-home-user-api/a.jsonl"), "{}");
        // A directory with no session left is not a sign of use.
        std::fs::create_dir_all(home.join(".claude/projects/-home-user-old")).unwrap();
        let claude: Vec<_> = claude_folders(home).into_iter().map(|(p, _)| p).collect();
        assert_eq!(claude, [PathBuf::from("/home/user/api")]);
        // Codex: the folder on each session's first line.
        write(
            home.join(".codex/sessions/2026/01/02/rollout-a.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/home/user/web\"}}\n{}\n",
        );
        let codex: Vec<_> = codex_folders(home).into_iter().map(|(p, _)| p).collect();
        assert_eq!(codex, [PathBuf::from("/home/user/web")]);
        // Pi: the newest session of each folder's directory.
        write(
            home.join(".pi/agent/sessions/--home-user-cli--/1.jsonl"),
            "{\"type\":\"session\",\"cwd\":\"/home/user/cli\"}\n",
        );
        let pi: Vec<_> = pi_folders(home).into_iter().map(|(p, _)| p).collect();
        assert_eq!(pi, [PathBuf::from("/home/user/cli")]);
        // cmux: each workspace's folder and its terminals', once each.
        write(
            home.join("Library/Application Support/cmux/session-com.cmuxterm.app.json"),
            r#"{"version":1,"windows":[{"tabManager":{"workspaces":[
                {"currentDirectory":"/home/user/site","panels":[
                    {"directory":"/home/user/site"},{"directory":"/home/user/site/docs"}]},
                {"currentDirectory":"","panels":[{"type":"browser"}]}]}}]}"#,
        );
        let cmux: Vec<_> = cmux_folders(home).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            cmux,
            [PathBuf::from("/home/user/site"), PathBuf::from("/home/user/site/docs")]
        );
    }

    #[test]
    fn scratch_folders_are_not_offered() {
        assert!(is_scratch(Path::new("/")));
        assert!(is_scratch(&config::HOME_DIR));
        assert!(is_scratch(&std::env::temp_dir().join("work")));
        assert!(is_scratch(Path::new("/private/tmp/agent-test")));
        assert!(!is_scratch(&config::HOME_DIR.join("code").join("app")));
        assert!(is_scratch(&config::HOME_DIR.join("Desktop")));
        assert!(!is_scratch(&config::HOME_DIR.join("Documents").join("app")));
    }

    #[test]
    fn apps_used_this_week_come_first_then_by_how_much() {
        let root = tempfile::tempdir().unwrap();
        let (a, b, c) = (root.path().join("a"), root.path().join("b"), root.path().join("c"));
        let scan = merge(
            vec![editor("Old"), editor("Busy"), editor("Fresh")],
            vec![
                // Old: three folders, none this week.
                opened(0, 20, &a),
                opened(0, 21, &b),
                opened(0, 22, &c),
                // Busy: two this week; Fresh: one, but the latest.
                opened(1, 2, &a),
                opened(1, 3, &b),
                opened(2, 1, &c),
            ],
            &as_given,
        );
        let names: Vec<_> = scan.editors.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Busy", "Fresh", "Old"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_drive_is_read_with_its_colon_escaped() {
        // VS Code writes the drive's colon as %3A.
        assert_eq!(
            local_path("file:///c%3A/Users/user/app"),
            Some(PathBuf::from(r"c:\Users\user\app"))
        );
    }
}
