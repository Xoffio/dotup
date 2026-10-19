use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Flex, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// How long a cached version check stays valid.
const CACHE_TTL_SECS: u64 = 3600;

#[derive(Debug, Deserialize)]
struct ToolsData {
    tools: BTreeMap<String, ToolEntry>,
}

#[derive(Debug, Deserialize)]
struct ToolEntry {
    version: String,
    repo: String,
    dotup: Option<DotupConfig>,
}

/// dotup-specific config embedded in tools.toml under [tools.<name>.dotup].
#[derive(Debug, Deserialize)]
struct DotupConfig {
    /// platform key (e.g. linux_amd64) -> regex matching exactly one asset
    assets: BTreeMap<String, String>,
}

/// Read-only view of .chezmoidata/hashes.toml: tool -> platform -> sha256.
#[derive(Debug, Deserialize, Default)]
struct HashesData {
    hashes: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Unchecked,
    Checking,
    UpToDate,
    UpdateAvailable,
    Error,
}

struct Tool {
    name: String,
    repo: String,
    current_version: String,
    latest_version: Option<String>,
    release_notes: Option<String>,
    /// sha256 digests of the latest release's assets: asset name -> hex digest
    assets: Vec<AssetHash>,
    /// platform -> regex from [tools.<name>.dotup.assets]; empty if the tool
    /// has no dotup section (version-check only)
    asset_patterns: BTreeMap<String, Regex>,
    status: Status,
}

/// A release asset and its sha256 digest, when GitHub provides one.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetHash {
    name: String,
    /// Hex-encoded sha256, or None when GitHub didn't publish a digest.
    sha256: Option<String>,
    /// Download URL, used as a fallback to compute the hash ourselves.
    #[serde(default)]
    url: String,
}

/// One platform's planned hash change in an update dry-run.
#[derive(Debug)]
struct HashPlan {
    platform: String,
    asset_name: String,
    old: Option<String>,
    new: Option<String>,
    /// True when GitHub published no digest and the asset must be downloaded
    /// to compute its hash.
    needs_download: bool,
}

/// The full update plan for one tool, built by `d` and executed by `u`.
#[derive(Debug)]
struct UpdatePlan {
    tool_index: usize,
    old_version: String,
    new_version: String,
    hashes: Vec<HashPlan>,
    /// platforms whose regex matched zero or several assets
    warnings: Vec<String>,
}

/// Messages from background threads into the app loop.
enum AppMsg {
    Check(CheckResult),
    UpdatePrepared {
        generation: u64,
        result: std::result::Result<UpdatePlan, String>,
    },
}

struct CheckResult {
    generation: u64,
    tool_index: usize,
    result: std::result::Result<ReleaseInfo, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseInfo {
    version: String,
    notes: String,
    assets: Vec<AssetHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tools,
    Details,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Cache {
    /// repo -> cached release info
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at: u64,
    release: ReleaseInfo,
}

impl CacheEntry {
    fn is_fresh(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.fetched_at) < CACHE_TTL_SECS
    }
}

fn cache_file_path() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".cache")
        });
    base.join("dotup").join("cache.json")
}

fn load_cache() -> Cache {
    let path = cache_file_path();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &Cache) {
    let path = cache_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, content);
    }
}

struct App {
    tools: Vec<Tool>,
    table_state: TableState,
    rx: Receiver<AppMsg>,
    generation: u64,
    tick: usize,
    details_scroll: u16,
    focus: Focus,
    cache: Cache,
    /// old hashes from hashes.toml (tool -> platform -> sha256), read-only
    hashes: BTreeMap<String, BTreeMap<String, String>>,
    /// pending update confirmation modal (from `d` dry-run or `u`)
    update_modal: Option<UpdateModal>,
    /// transient status line message shown in the footer
    notice: Option<(String, Color)>,
}

struct UpdateModal {
    plan: UpdatePlan,
    /// true: shown by `d`, view only. false: shown by `u`, confirm writes.
    dry_run: bool,
    /// true once the background hash preparation has finished
    ready: bool,
}

impl App {
    fn new(
        tools: Vec<Tool>,
        rx: Receiver<AppMsg>,
        cache: Cache,
        hashes: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Self {
        let mut table_state = TableState::default();
        if !tools.is_empty() {
            table_state.select(Some(0));
        }
        Self {
            tools,
            table_state,
            rx,
            generation: 0,
            tick: 0,
            details_scroll: 0,
            focus: Focus::Tools,
            cache,
            hashes,
            update_modal: None,
            notice: None,
        }
    }

    fn next(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % self.tools.len(),
            None => 0,
        };
        self.table_state.select(Some(i));
        self.details_scroll = 0;
    }

    fn previous(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) | None => self.tools.len() - 1,
            Some(i) => i - 1,
        };
        self.table_state.select(Some(i));
        self.details_scroll = 0;
    }

    fn scroll_details_down(&mut self) {
        self.details_scroll = self.details_scroll.saturating_add(1);
    }

    fn scroll_details_up(&mut self) {
        self.details_scroll = self.details_scroll.saturating_sub(1);
    }

    fn focus_details(&mut self) {
        self.focus = Focus::Details;
    }

    fn focus_tools(&mut self) {
        self.focus = Focus::Tools;
    }

    /// Check all tools. With `force = false` fresh cache entries are used
    /// instead of hitting the network; `force = true` refetches everything.
    fn refresh(&mut self, tx: mpsc::Sender<AppMsg>, force: bool) {
        self.generation += 1;
        let generation = self.generation;
        for (index, tool) in self.tools.iter_mut().enumerate() {
            if !force
                && let Some(entry) = self.cache.entries.get(&tool.repo)
                && entry.is_fresh()
            {
                tool.status = compare_versions(&tool.current_version, &entry.release.version);
                tool.latest_version = Some(entry.release.version.clone());
                tool.release_notes = Some(entry.release.notes.clone());
                tool.assets = entry.release.assets.clone();
                continue;
            }
            tool.status = Status::Checking;
            tool.latest_version = None;
            tool.release_notes = None;
            tool.assets = Vec::new();
            spawn_check(tx.clone(), generation, index, tool.repo.clone());
        }
    }

    /// Kick off update preparation (hash matching + downloads) for the
    /// selected tool in the background. `dry_run` only previews; `!dry_run`
    /// asks for confirmation before writing.
    fn begin_update(&mut self, tx: mpsc::Sender<AppMsg>, dry_run: bool) {
        let Some(index) = self.table_state.selected() else {
            return;
        };
        let tool = &self.tools[index];

        if tool.status != Status::UpdateAvailable {
            self.notice = Some((
                format!("{}: no update available", tool.name),
                Color::DarkGray,
            ));
            return;
        }
        if tool.asset_patterns.is_empty() {
            self.notice = Some((
                format!("{}: no dotup section in tools.toml — version-check only", tool.name),
                Color::Red,
            ));
            return;
        }
        let Some(latest) = tool.latest_version.clone() else {
            return;
        };

        let generation = self.generation;
        let name = tool.name.clone();
        let current = tool.current_version.clone();
        let assets = tool.assets.clone();
        let patterns: Vec<(String, Regex)> = tool
            .asset_patterns
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let old_hashes = self.hashes.get(&tool.name).cloned().unwrap_or_default();

        self.update_modal = Some(UpdateModal {
            plan: UpdatePlan {
                tool_index: index,
                old_version: current.clone(),
                new_version: latest.clone(),
                hashes: Vec::new(),
                warnings: Vec::new(),
            },
            dry_run,
            ready: false,
        });

        thread::spawn(move || {
            let result = prepare_update(index, name, current, latest, &patterns, &old_hashes, &assets);
            let _ = tx.send(AppMsg::UpdatePrepared { generation, result });
        });
    }

    /// Write the confirmed update to tools.toml and hashes.toml, then refresh.
    fn apply_update(&mut self, tx: mpsc::Sender<AppMsg>) {
        let Some(modal) = self.update_modal.take() else {
            return;
        };
        let plan = modal.plan;
        let tool_name = self.tools[plan.tool_index].name.clone();

        match write_update(&tool_name, &plan) {
            Ok(()) => {
                // Reflect the change in memory immediately.
                let tool = &mut self.tools[plan.tool_index];
                tool.current_version = plan.new_version.clone();
                for h in &plan.hashes {
                    if let Some(new) = &h.new {
                        self.hashes
                            .entry(tool_name.clone())
                            .or_default()
                            .insert(h.platform.clone(), new.clone());
                    }
                }
                self.notice = Some((
                    format!("{} updated to {}", tool_name, plan.new_version),
                    Color::Green,
                ));
                // Re-check so the table shows "up to date" from real data.
                self.refresh(tx, true);
            }
            Err(e) => {
                self.notice = Some((format!("failed to write update: {e:#}"), Color::Red));
            }
        }
    }

    fn handle_results(&mut self) {
        let mut cache_dirty = false;
        loop {
            match self.rx.try_recv() {
                Ok(AppMsg::Check(result)) => {
                    // Discard results from a previous refresh round.
                    if result.generation != self.generation {
                        continue;
                    }
                    let Some(tool) = self.tools.get_mut(result.tool_index) else {
                        continue;
                    };
                    match result.result {
                        Ok(release) => {
                            tool.status = compare_versions(&tool.current_version, &release.version);
                            tool.latest_version = Some(release.version.clone());
                            tool.release_notes = Some(release.notes.clone());
                            tool.assets = release.assets.clone();
                            let fetched_at = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            self.cache.entries.insert(
                                tool.repo.clone(),
                                CacheEntry {
                                    fetched_at,
                                    release,
                                },
                            );
                            cache_dirty = true;
                        }
                        Err(_) => {
                            tool.status = Status::Error;
                            tool.latest_version = Some("-".to_string());
                            tool.release_notes = None;
                            tool.assets = Vec::new();
                        }
                    }
                }
                Ok(AppMsg::UpdatePrepared { generation, result }) => {
                    if generation != self.generation {
                        continue;
                    }
                    match result {
                        Ok(plan) => {
                            if let Some(modal) = &mut self.update_modal
                                && modal.plan.tool_index == plan.tool_index
                            {
                                modal.plan = plan;
                                modal.ready = true;
                            }
                        }
                        Err(e) => {
                            self.update_modal = None;
                            self.notice = Some((format!("update prep failed: {e}"), Color::Red));
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if cache_dirty {
            save_cache(&self.cache);
        }
    }
}

/// Extract a semver-ish substring from a version or tag string.
/// Handles tags like "v1.2.3", "jq-1.8.2", "release-2.0".
fn version_part(s: &str) -> &str {
    let start = s
        .bytes()
        .position(|b| b.is_ascii_digit())
        .unwrap_or(s.len());
    &s[start..]
}

fn compare_versions(current: &str, latest: &str) -> Status {
    let current = version_part(current);
    let latest = version_part(latest);
    match (Version::parse(current), Version::parse(latest)) {
        (Ok(current), Ok(latest)) => {
            if latest > current {
                Status::UpdateAvailable
            } else {
                Status::UpToDate
            }
        }
        _ => {
            if current == latest {
                Status::UpToDate
            } else {
                Status::UpdateAvailable
            }
        }
    }
}

fn fetch_latest_version(repo: &str) -> std::result::Result<ReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut request = ureq::get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let body: serde_json::Value = serde_json::from_str(
        &response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("failed to read response: {e}"))?,
    )
    .map_err(|e| format!("invalid json: {e}"))?;

    let version = body
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .map(|tag| tag.trim_start_matches(['v', 'V']).to_string())
        .ok_or_else(|| "no tag_name in response".to_string())?;

    let notes = body
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("No release notes.")
        .to_string();

    let assets = body
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    let name = asset.get("name")?.as_str()?.to_string();
                    // GitHub exposes digests as "sha256:<hex>" when available.
                    let sha256 = asset
                        .get("digest")
                        .and_then(|d| d.as_str())
                        .and_then(|d| d.strip_prefix("sha256:"))
                        .map(|d| d.to_string());
                    let url = asset
                        .get("browser_download_url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some(AssetHash { name, sha256, url })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ReleaseInfo {
        version,
        notes,
        assets,
    })
}

/// Build an UpdatePlan for a tool: match each platform regex against the
/// latest release's assets, and download assets whose digest GitHub did not
/// publish to compute their sha256 ourselves.
fn prepare_update(
    tool_index: usize,
    tool_name: String,
    current: String,
    latest: String,
    patterns: &[(String, Regex)],
    old_hashes: &BTreeMap<String, String>,
    assets: &[AssetHash],
) -> std::result::Result<UpdatePlan, String> {
    let mut plan = UpdatePlan {
        tool_index,
        old_version: current,
        new_version: latest,
        hashes: Vec::new(),
        warnings: Vec::new(),
    };

    for (platform, pattern) in patterns {
        let matches: Vec<&AssetHash> = assets
            .iter()
            .filter(|a| pattern.is_match(&a.name))
            .collect();
        match matches.as_slice() {
            [asset] => {
                let new = match &asset.sha256 {
                    Some(h) => Some(h.clone()),
                    None => Some(download_and_hash(&asset.url).map_err(|e| {
                        format!("{tool_name}/{platform}: failed to hash {}: {e}", asset.name)
                    })?),
                };
                plan.hashes.push(HashPlan {
                    platform: platform.clone(),
                    asset_name: asset.name.clone(),
                    old: old_hashes.get(platform).cloned(),
                    new,
                    needs_download: asset.sha256.is_none(),
                });
            }
            [] => plan
                .warnings
                .push(format!("{platform}: no asset matches {}", pattern.as_str())),
            many => plan.warnings.push(format!(
                "{platform}: {} assets match {} ({})",
                many.len(),
                pattern.as_str(),
                many.iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    plan.hashes.sort_by(|a, b| a.platform.cmp(&b.platform));
    Ok(plan)
}

/// Download an asset and compute its sha256 (fallback for repos that don't
/// publish digests).
fn download_and_hash(url: &str) -> std::result::Result<String, String> {
    use sha2::Digest;
    let mut response = ureq::get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut reader = response.body_mut().as_reader();
    std::io::copy(&mut reader, &mut hasher).map_err(|e| format!("read failed: {e}"))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Persist an update: version in tools.toml, hashes in hashes.toml, with
/// formatting and comments preserved.
fn write_update(tool_name: &str, plan: &UpdatePlan) -> Result<()> {
    let dir = chezmoi_data_dir()?;

    // tools.toml: bump the version string in place.
    let tools_path = dir.join("tools.toml");
    let content = std::fs::read_to_string(&tools_path)
        .with_context(|| format!("Failed to read {}", tools_path.display()))?;
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("Failed to parse {}", tools_path.display()))?;
    doc["tools"][tool_name]["version"] = toml_edit::value(plan.new_version.clone());
    std::fs::write(&tools_path, doc.to_string())
        .with_context(|| format!("Failed to write {}", tools_path.display()))?;

    // hashes.toml: upsert each platform hash.
    let hashes_path = dir.join("hashes.toml");
    let content = std::fs::read_to_string(&hashes_path)
        .with_context(|| format!("Failed to read {}", hashes_path.display()))?;
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("Failed to parse {}", hashes_path.display()))?;
    for h in &plan.hashes {
        let Some(new) = &h.new else { continue };
        if doc["hashes"][tool_name].is_none() {
            doc["hashes"][tool_name] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        doc["hashes"][tool_name][h.platform.as_str()] = toml_edit::value(new.clone());
    }
    std::fs::write(&hashes_path, doc.to_string())
        .with_context(|| format!("Failed to write {}", hashes_path.display()))?;

    Ok(())
}

fn spawn_check(
    tx: mpsc::Sender<AppMsg>,
    generation: u64,
    tool_index: usize,
    repo: String,
) {
    thread::spawn(move || {
        let result = fetch_latest_version(&repo);
        // If the receiver is gone the app is exiting; ignore the error.
        let _ = tx.send(AppMsg::Check(CheckResult {
            generation,
            tool_index,
            result,
        }));
    });
}

fn chezmoi_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".local/share/chezmoi/.chezmoidata"))
}

fn tools_file_path() -> Result<PathBuf> {
    Ok(chezmoi_data_dir()?.join("tools.toml"))
}

fn hashes_file_path() -> Result<PathBuf> {
    Ok(chezmoi_data_dir()?.join("hashes.toml"))
}

fn load_tools() -> Result<Vec<Tool>> {
    let path = tools_file_path()?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let data: ToolsData = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    data.tools
        .into_iter()
        .map(|(name, entry)| {
            let mut asset_patterns = BTreeMap::new();
            if let Some(dotup) = entry.dotup {
                for (platform, pattern) in dotup.assets {
                    let regex = Regex::new(&pattern).with_context(|| {
                        format!("tools.toml: invalid regex for {name}/{platform}: {pattern}")
                    })?;
                    asset_patterns.insert(platform, regex);
                }
            }
            Ok(Tool {
                name,
                repo: entry.repo,
                current_version: entry.version,
                latest_version: None,
                release_notes: None,
                assets: Vec::new(),
                asset_patterns,
                status: Status::Unchecked,
            })
        })
        .collect()
}

fn load_hashes() -> BTreeMap<String, BTreeMap<String, String>> {
    let Ok(path) = hashes_file_path() else {
        return BTreeMap::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| toml::from_str::<HashesData>(&content).ok())
        .map(|d| d.hashes)
        .unwrap_or_default()
}

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[0]);

    // Upper section: tools table
    let header = Row::new(vec!["Tool", "Current", "Latest", "Status"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let spinner = SPINNER[app.tick % SPINNER.len()];

    let rows: Vec<Row> = app
        .tools
        .iter()
        .map(|tool| {
            let latest = tool.latest_version.as_deref().unwrap_or("-");
            let (status_text, status_style) = match tool.status {
                Status::Unchecked => ("-".to_string(), Style::default().fg(Color::DarkGray)),
                Status::Checking => (
                    format!("{spinner} checking…"),
                    Style::default().fg(Color::Cyan),
                ),
                Status::UpToDate => (
                    "up to date".to_string(),
                    Style::default().fg(Color::Green),
                ),
                Status::UpdateAvailable => (
                    "update available".to_string(),
                    Style::default().fg(Color::Yellow),
                ),
                Status::Error => ("error".to_string(), Style::default().fg(Color::Red)),
            };
            Row::new(vec![
                Cell::from(tool.name.clone()),
                Cell::from(tool.current_version.clone()),
                Cell::from(latest.to_string()),
                Cell::from(status_text).style(status_style),
            ])
        })
        .collect();

    let tools_border = if app.focus == Focus::Tools {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Tools ")
            .borders(Borders::ALL)
            .border_style(tools_border),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(">> ");

    frame.render_stateful_widget(table, body[0], &mut app.table_state);

    // Lower section: details of the selected tool
    let selected = app.table_state.selected().and_then(|i| app.tools.get(i));

    let (details_title, details_lines) = match selected {
        Some(tool) => {
            let title = format!(" {} {} ", tool.name, tool.current_version);

            // Asset hashes section, shared by the update-available and
            // up-to-date views (whenever we have fetched release data).
            // Shown before the release notes since it is the actionable data;
            // long notes then follow below.
            let asset_lines = |lines: &mut Vec<Line>| {
                // Skip checksum sidecar files; they add noise without value here.
                let assets: Vec<&AssetHash> = tool
                    .assets
                    .iter()
                    .filter(|a| {
                        !a.name.ends_with(".sha256")
                            && !a.name.ends_with(".sig")
                            && !a.name.ends_with(".asc")
                    })
                    .collect();
                if assets.is_empty() {
                    return;
                }
                lines.push(Line::from(Span::styled(
                    "Assets (latest release):",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                for asset in assets {
                    match &asset.sha256 {
                        Some(hash) => lines.push(Line::from(vec![
                            Span::raw(format!("  {}", asset.name)),
                            Span::styled(
                                format!("  sha256:{hash}"),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ])),
                        None => lines.push(Line::from(vec![
                            Span::raw(format!("  {}", asset.name)),
                            Span::styled(
                                "  (no digest published)",
                                Style::default().fg(Color::DarkGray),
                            ),
                        ])),
                    }
                }
                lines.push(Line::from(""));
            };

            match tool.status {
                Status::UpdateAvailable => {
                    let mut lines = vec![
                        Line::from(vec![
                            Span::styled("Update available: ", Style::default().fg(Color::Yellow)),
                            Span::styled(
                                format!(
                                    "{} → {}",
                                    tool.current_version,
                                    tool.latest_version.as_deref().unwrap_or("?")
                                ),
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(""),
                    ];
                    asset_lines(&mut lines);
                    match &tool.release_notes {
                        Some(notes) if !notes.trim().is_empty() => {
                            lines.push(Line::from(Span::styled(
                                "Release notes:",
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            )));
                            lines.extend(notes.lines().map(|l| Line::from(l.to_string())));
                        }
                        _ => lines.push(Line::from(Span::styled(
                            "No release notes.",
                            Style::default().fg(Color::DarkGray),
                        ))),
                    }
                    (title, lines)
                }
                Status::UpToDate => {
                    let mut lines = vec![
                        Line::from(Span::styled(
                            "Up to date.",
                            Style::default().fg(Color::Green),
                        )),
                        Line::from(""),
                    ];
                    asset_lines(&mut lines);
                    (title, lines)
                }
                Status::Checking => (
                    title,
                    vec![Line::from(Span::styled(
                        "Checking for updates…",
                        Style::default().fg(Color::Cyan),
                    ))],
                ),
                Status::Error => (
                    title,
                    vec![Line::from(Span::styled(
                        "Failed to check for updates.",
                        Style::default().fg(Color::Red),
                    ))],
                ),
                Status::Unchecked => (title, vec![Line::from("")]),
            }
        }
        None => (" Details ".to_string(), vec![Line::from("")]),
    };

    let details_border = if app.focus == Focus::Details {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let scroll_indicator = if app.details_scroll > 0 {
        format!(" [scroll: {}] ", app.details_scroll)
    } else {
        String::new()
    };

    let details = Paragraph::new(details_lines)
        .block(
            Block::default()
                .title(details_title)
                .title_bottom(scroll_indicator)
                .borders(Borders::ALL)
                .border_style(details_border),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.details_scroll, 0));

    frame.render_widget(details, body[1]);

    // Footer: key hints, or a transient notice if one is active
    let footer = if let Some((msg, color)) = &app.notice {
        Line::from(Span::styled(format!(" {msg}"), Style::default().fg(*color)))
    } else {
        Line::from(vec![
            Span::styled(" q", Style::default().fg(Color::Yellow)),
            Span::raw(": quit  "),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::raw(": refresh  "),
            Span::styled("d", Style::default().fg(Color::Yellow)),
            Span::raw(": dry-run  "),
            Span::styled("u", Style::default().fg(Color::Yellow)),
            Span::raw(": update  "),
            Span::styled("←/→", Style::default().fg(Color::Yellow)),
            Span::raw(": pane  "),
            Span::styled("↑/k ↓/j", Style::default().fg(Color::Yellow)),
            Span::raw(": nav/scroll"),
        ])
    };
    frame.render_widget(Paragraph::new(footer), chunks[1]);

    // Update confirmation / dry-run modal
    if let Some(modal) = &app.update_modal {
        render_update_modal(frame, modal);
    }
}

fn render_update_modal(frame: &mut Frame, modal: &UpdateModal) {
    let plan = &modal.plan;
    let spinner = SPINNER[0]; // static fallback; spinner animates via tick elsewhere

    let mut lines: Vec<Line> = Vec::new();

    if modal.ready {
        lines.push(Line::from(vec![
            Span::styled("Version: ", Style::default().fg(Color::Gray)),
            Span::raw(&plan.old_version),
            Span::styled("  →  ", Style::default().fg(Color::Yellow)),
            Span::styled(
                &plan.new_version,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        if plan.hashes.is_empty() {
            lines.push(Line::from(Span::styled(
                "No platform hashes to update.",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Hashes:",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            for h in &plan.hashes {
                let old = h.old.as_deref().unwrap_or("∅ (not in hashes.toml)");
                let new = h.new.as_deref().unwrap_or("?");
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", h.platform),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ({})", h.asset_name),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    old: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(short_hash(old), Style::default().fg(Color::Red)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    new: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(short_hash(new), Style::default().fg(Color::Green)),
                    Span::styled(
                        if h.needs_download { "  (downloaded)" } else { "" },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }

        for warning in &plan.warnings {
            lines.push(Line::from(Span::styled(
                format!("⚠ {warning}"),
                Style::default().fg(Color::Yellow),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            format!("{spinner} Preparing update…"),
            Style::default().fg(Color::Cyan),
        )));
    }

    lines.push(Line::from(""));
    let hint = if !modal.ready {
        Line::from(Span::styled(
            "working… (Esc cancels)".to_string(),
            Style::default().fg(Color::DarkGray),
        ))
    } else if modal.dry_run {
        Line::from(Span::styled(
            "dry-run — no files were changed.  Esc: close",
            Style::default().fg(Color::Gray),
        ))
    } else if plan.hashes.is_empty() {
        Line::from(Span::styled(
            "nothing to write.  Esc: cancel",
            Style::default().fg(Color::Gray),
        ))
    } else {
        Line::from(vec![
            Span::styled("Enter/y", Style::default().fg(Color::Yellow)),
            Span::raw(": apply to tools.toml + hashes.toml  "),
            Span::styled("Esc/n", Style::default().fg(Color::Yellow)),
            Span::raw(": cancel"),
        ])
    };
    lines.push(hint);

    let title = if modal.dry_run {
        " Update dry-run "
    } else {
        " Confirm update "
    };

    // Center the modal.
    let area = frame.area();
    let width = area.width.clamp(30, 78);
    let content_lines = lines.len() as u16 + 2; // + borders
    let height = content_lines.min(area.height.saturating_sub(2)).max(8);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(width),
            Constraint::Fill(1),
        ])
        .flex(Flex::Center)
        .split(vertical[1]);
    let modal_area = horizontal[1];

    frame.render_widget(Clear, modal_area);
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, modal_area);
}

fn short_hash(h: &str) -> String {
    const MAX: usize = 48;
    if h.chars().count() > MAX {
        format!("{}…", h.chars().take(MAX - 1).collect::<String>())
    } else {
        h.to_string()
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tx: mpsc::Sender<AppMsg>,
) -> Result<()> {
    loop {
        app.handle_results();

        terminal.draw(|frame| ui(frame, app))?;

        // Poll with a short timeout so the UI keeps animating and
        // background results are applied promptly without blocking.
        if !event::poll(Duration::from_millis(100))? {
            app.tick = app.tick.wrapping_add(1);
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // The update modal captures all keys while open.
            if app.update_modal.is_some() {
                let modal = app.update_modal.as_ref().unwrap();
                let ready = modal.ready;
                let dry_run = modal.dry_run;
                match key.code {
                    KeyCode::Esc => app.update_modal = None,
                    KeyCode::Char('y') | KeyCode::Enter if ready && !dry_run => {
                        app.apply_update(tx.clone());
                    }
                    _ => {}
                }
                continue;
            }

            app.notice = None;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => {
                    let tx = tx.clone();
                    app.refresh(tx, true);
                }
                KeyCode::Char('d') => app.begin_update(tx.clone(), true),
                KeyCode::Char('u') => app.begin_update(tx.clone(), false),
                KeyCode::Right => app.focus_details(),
                KeyCode::Left => app.focus_tools(),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => match app.focus {
                    Focus::Tools => app.next(),
                    Focus::Details => app.scroll_details_down(),
                },
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => match app.focus {
                    Focus::Tools => app.previous(),
                    Focus::Details => app.scroll_details_up(),
                },
                _ => {}
            }
        }
    }
}

fn main() -> Result<()> {
    let tools = load_tools()?;
    let hashes = load_hashes();

    let (tx, rx) = mpsc::channel::<AppMsg>();
    let cache = load_cache();
    let mut app = App::new(tools, rx, cache, hashes);

    // Check for updates on startup; fresh cache entries are used
    // instead of hitting the network.
    app.refresh(tx.clone(), false);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &mut app, tx);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
