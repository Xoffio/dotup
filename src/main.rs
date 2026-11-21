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
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use regex::{Regex, RegexBuilder};
use semver::Version;
use serde::{Deserialize, Serialize};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// How long a cached version check stays valid.
const CACHE_TTL_SECS: u64 = 3600;

/// Catppuccin Mocha palette (truecolor).
const BG: Color = Color::Rgb(0x1e, 0x1e, 0x2e); // base
const SURFACE: Color = Color::Rgb(0x31, 0x32, 0x44); // selection bg
const BORDER: Color = Color::Rgb(0x45, 0x47, 0x5a); // dim borders
const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
const MUTED: Color = Color::Rgb(0x6c, 0x70, 0x86);
const ACCENT: Color = Color::Rgb(0xb4, 0xbe, 0xfe); // lavender
const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
const TEAL: Color = Color::Rgb(0x89, 0xdc, 0xeb); // checking / section headers

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

impl Status {
    /// Plain-text label used in the table and matched by the search filter.
    fn label(self) -> &'static str {
        match self {
            Status::Unchecked => "-",
            Status::Checking => "checking",
            Status::UpToDate => "up to date",
            Status::UpdateAvailable => "update available",
            Status::Error => "error",
        }
    }
}

/// Which release dotup treats as the newest: the release GitHub flags as
/// "Latest" (`/releases/latest`), or the highest semver among published
/// non-prerelease releases. Maintainers can pin the GitHub flag to an
/// older line, so the two can differ. Toggled with `m`; defaults to
/// Latest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Latest,
    Highest,
}

impl Mode {
    fn toggle(self) -> Self {
        match self {
            Mode::Latest => Mode::Highest,
            Mode::Highest => Mode::Latest,
        }
    }
}

struct Tool {
    name: String,
    repo: String,
    current_version: String,
    /// Fetched release info for both modes, present once checked.
    release: Option<ReleaseInfo>,
    /// Human-readable reason for the last failed check, shown in details.
    check_error: Option<String>,
    /// platform -> regex from [tools.<name>.dotup.assets]; empty if the tool
    /// has no dotup section (version-check only)
    asset_patterns: BTreeMap<String, Regex>,
    status: Status,
}

impl Tool {
    /// The release the current mode considers newest. `Highest` falls back
    /// to the flagged release when the semver scan is unavailable.
    fn selected_release(&self, mode: Mode) -> Option<&Release> {
        self.release.as_ref().map(|info| match mode {
            Mode::Latest => &info.latest,
            Mode::Highest => info.highest.as_ref().unwrap_or(&info.latest),
        })
    }
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
    /// Result of the on-demand highest-semver scan triggered by switching
    /// to `Highest` mode for a tool whose scan hasn't run yet.
    HighestCheck {
        generation: u64,
        tool_index: usize,
        result: std::result::Result<Option<Release>, String>,
    },
    UpdatePrepared {
        generation: u64,
        result: std::result::Result<UpdatePlan, String>,
    },
}

struct CheckResult {
    generation: u64,
    tool_index: usize,
    result: std::result::Result<Release, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Release {
    version: String,
    notes: String,
    assets: Vec<AssetHash>,
}

/// Both notions of "newest release" for one repo, cached together so the
/// mode toggle never triggers a refetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseInfo {
    /// The release GitHub flags as "Latest" (`/releases/latest`). Not
    /// necessarily the highest version — maintainers can pin the flag to
    /// an older line (opencode pins it to 1.x while 2.x releases exist).
    latest: Release,
    /// Highest semver among recent published, non-prerelease releases.
    /// `None` when that list could not be fetched; the UI then falls back
    /// to `latest`.
    #[serde(default)]
    highest: Option<Release>,
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
    /// indices into `tools` that pass the current filter, in display order
    visible: Vec<usize>,
    /// applied search pattern (empty = no filter)
    filter: String,
    /// pattern being typed after `/`; None when not in search mode
    search_input: Option<String>,
    /// true when the current pattern is not a valid regex
    filter_error: bool,
    /// which release counts as "newest" (pill box, top right)
    mode: Mode,
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
        let visible = (0..tools.len()).collect();
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
            visible,
            filter: String::new(),
            search_input: None,
            filter_error: false,
            mode: Mode::default(),
        }
    }

    fn next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % self.visible.len(),
            None => 0,
        };
        self.table_state.select(Some(i));
        self.details_scroll = 0;
    }

    fn previous(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) | None => self.visible.len() - 1,
            Some(i) => i - 1,
        };
        self.table_state.select(Some(i));
        self.details_scroll = 0;
    }

    /// Index into `tools` of the row currently highlighted in the table.
    fn selected_index(&self) -> Option<usize> {
        self.table_state
            .selected()
            .and_then(|i| self.visible.get(i).copied())
    }

    fn active_pattern(&self) -> &str {
        self.search_input.as_deref().unwrap_or(&self.filter)
    }

    /// Recompute which tools pass the active pattern, matched case-insensitively
    /// against name, repo, current/latest version and status. An invalid regex
    /// shows everything and flags the error in the search bar.
    fn rebuild_visible(&mut self) {
        let pattern = self.active_pattern().to_string();
        self.filter_error = false;
        self.visible = if pattern.is_empty() {
            (0..self.tools.len()).collect()
        } else {
            match RegexBuilder::new(&pattern).case_insensitive(true).build() {
                Ok(re) => self
                    .tools
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        re.is_match(&t.name)
                            || re.is_match(&t.repo)
                            || re.is_match(&t.current_version)
                            || t.selected_release(self.mode)
                                .is_some_and(|r| re.is_match(&r.version))
                            || re.is_match(t.status.label())
                    })
                    .map(|(i, _)| i)
                    .collect(),
                Err(_) => {
                    self.filter_error = true;
                    (0..self.tools.len()).collect()
                }
            }
        };
        let selection = match self.visible.len() {
            0 => None,
            n => Some(self.table_state.selected().unwrap_or(0).min(n - 1)),
        };
        self.table_state.select(selection);
        self.details_scroll = 0;
    }

    fn start_search(&mut self) {
        self.search_input = Some(self.filter.clone());
    }

    fn search_push(&mut self, c: char) {
        if let Some(input) = self.search_input.as_mut() {
            input.push(c);
        }
        self.rebuild_visible();
    }

    fn search_pop(&mut self) {
        if let Some(input) = self.search_input.as_mut() {
            input.pop();
        }
        self.rebuild_visible();
    }

    /// Enter: keep the typed pattern as the active filter.
    fn search_confirm(&mut self) {
        if let Some(input) = self.search_input.take() {
            self.filter = input;
        }
        self.rebuild_visible();
    }

    /// Esc: drop the pattern and show every tool again.
    fn clear_filter(&mut self) {
        self.search_input = None;
        self.filter.clear();
        self.rebuild_visible();
    }

    /// `m`: switch between the GitHub "Latest" flag and highest semver.
    /// Tools whose highest-semver scan hasn't run yet (it is fetched
    /// lazily to spare the GitHub rate limit) go to `checking` — but only
    /// the ones currently visible; hidden ones just get their status
    /// recomputed locally from already-fetched data, and are scanned the
    /// next time they are visible and `m` is pressed.
    fn toggle_mode(&mut self, tx: &mpsc::Sender<AppMsg>) {
        self.mode = self.mode.toggle();
        let mode = self.mode;
        let generation = self.generation;
        let visible: std::collections::HashSet<usize> =
            self.visible.iter().copied().collect();
        let mut scan_needed: Vec<(usize, String)> = Vec::new();
        for (index, tool) in self.tools.iter_mut().enumerate() {
            let needs_scan = mode == Mode::Highest
                && tool.release.as_ref().is_some_and(|info| info.highest.is_none());
            if needs_scan && visible.contains(&index) {
                tool.status = Status::Checking;
                tool.check_error = None;
                scan_needed.push((index, tool.repo.clone()));
            } else if let Some(rel) = tool.selected_release(mode) {
                tool.status = compare_versions(&tool.current_version, &rel.version);
            }
        }
        for (index, repo) in scan_needed {
            spawn_highest_check(tx.clone(), generation, index, repo);
        }
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

    /// Check tools. With `force = false` fresh cache entries are used
    /// instead of hitting the network; `force = true` refetches. When a
    /// filter is active only the visible tools are checked, to keep the
    /// request count down. The generation is bumped only on a full round
    /// (no filter): a partial round must not discard in-flight results of
    /// the tools it didn't touch, or those would spin at `checking`
    /// forever.
    fn refresh(&mut self, tx: mpsc::Sender<AppMsg>, force: bool) {
        let scope: Vec<usize> = self.visible.clone();
        let full = scope.len() == self.tools.len();
        if full {
            self.generation += 1;
        }
        let generation = self.generation;
        for index in scope {
            let tool = &mut self.tools[index];
            if !force
                && let Some(entry) = self.cache.entries.get(&tool.repo)
                && entry.is_fresh()
            {
                tool.release = Some(entry.release.clone());
                if let Some(rel) = tool.selected_release(self.mode) {
                    tool.status = compare_versions(&tool.current_version, &rel.version);
                }
                continue;
            }
            tool.status = Status::Checking;
            tool.release = None;
            tool.check_error = None;
            spawn_check(tx.clone(), generation, index, tool.repo.clone());
        }
    }

    /// Kick off update preparation (hash matching + downloads) for the
    /// selected tool in the background. `dry_run` only previews; `!dry_run`
    /// asks for confirmation before writing.
    fn begin_update(&mut self, tx: mpsc::Sender<AppMsg>, dry_run: bool) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let mode = self.mode;
        let tool = &self.tools[index];

        if tool.status != Status::UpdateAvailable {
            self.notice = Some((format!("{}: no update available", tool.name), MUTED));
            return;
        }
        if tool.asset_patterns.is_empty() {
            self.notice = Some((
                format!("{}: no dotup section in tools.toml — version-check only", tool.name),
                RED,
            ));
            return;
        }
        let Some(sel) = tool.selected_release(mode) else {
            return;
        };
        let latest = sel.version.clone();

        let generation = self.generation;
        let name = tool.name.clone();
        let current = tool.current_version.clone();
        let assets = sel.assets.clone();
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
                    GREEN,
                ));
                // Re-check so the table shows "up to date" from real data.
                self.refresh(tx, true);
            }
            Err(e) => {
                self.notice = Some((format!("failed to write update: {e:#}"), RED));
            }
        }
    }

    fn handle_results(&mut self, tx: &mpsc::Sender<AppMsg>) {
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
                            let mode = self.mode;
                            let generation = result.generation;
                            // The highest-semver scan is lazy: it runs on
                            // demand to spare the GitHub rate limit, so a
                            // fresh check starts without it.
                            tool.release = Some(ReleaseInfo {
                                latest: release.clone(),
                                highest: None,
                            });
                            tool.check_error = None;
                            if let Some(rel) = tool.selected_release(mode) {
                                tool.status =
                                    compare_versions(&tool.current_version, &rel.version);
                            }
                            // In Highest mode a fresh check without a scan
                            // would silently fall back to the flagged
                            // release; kick off the scan instead.
                            if mode == Mode::Highest
                                && tool.release.as_ref().is_some_and(|i| i.highest.is_none())
                            {
                                let index = result.tool_index;
                                let repo = tool.repo.clone();
                                spawn_highest_check(tx.clone(), generation, index, repo);
                            }
                            let fetched_at = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            self.cache.entries.insert(
                                tool.repo.clone(),
                                CacheEntry {
                                    fetched_at,
                                    release: ReleaseInfo {
                                        latest: release,
                                        highest: None,
                                    },
                                },
                            );
                            cache_dirty = true;
                        }
                        Err(e) => {
                            tool.status = Status::Error;
                            tool.release = None;
                            tool.check_error = Some(e);
                        }
                    }
                }
                Ok(AppMsg::HighestCheck {
                    generation,
                    tool_index,
                    result,
                }) => {
                    if generation != self.generation {
                        continue;
                    }
                    let Some(tool) = self.tools.get_mut(tool_index) else {
                        continue;
                    };
                    let mode = self.mode;
                    match result {
                        Ok(Some(release)) => {
                            if let Some(info) = &mut tool.release {
                                info.highest = Some(release.clone());
                            }
                            if let Some(entry) = self.cache.entries.get_mut(&tool.repo) {
                                entry.release.highest = Some(release);
                            }
                            tool.check_error = None;
                            if let Some(rel) = tool.selected_release(mode) {
                                tool.status =
                                    compare_versions(&tool.current_version, &rel.version);
                            }
                            cache_dirty = true;
                        }
                        // No semver-parseable tags at all: remember the
                        // flagged release as the highest so the scan is not
                        // retried on every mode toggle.
                        Ok(None) => {
                            let fallback = tool.release.as_ref().map(|i| i.latest.clone());
                            if let (Some(info), Some(fallback)) = (&mut tool.release, fallback) {
                                info.highest = Some(fallback);
                            }
                            if let Some(rel) = tool.selected_release(mode) {
                                tool.status =
                                    compare_versions(&tool.current_version, &rel.version);
                            }
                        }
                        Err(e) => {
                            tool.check_error = Some(e);
                            // Without a scan, Highest mode has nothing real
                            // to compare against; only fall back to the
                            // flagged release when that is what is shown.
                            if mode == Mode::Highest
                                && tool.release.as_ref().is_none_or(|i| i.highest.is_none())
                            {
                                tool.status = Status::Error;
                            } else if let Some(rel) = tool.selected_release(mode) {
                                tool.status =
                                    compare_versions(&tool.current_version, &rel.version);
                            }
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
                            self.notice = Some((format!("update prep failed: {e}"), RED));
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

/// GET a GitHub API endpoint and parse the JSON body.
fn github_get_json(url: &str) -> std::result::Result<serde_json::Value, String> {
    let mut request = ureq::get(url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    // Surface HTTP errors with GitHub's own message (e.g. rate limits)
    // instead of failing later on an unrelated JSON parse.
    if !response.status().is_success() {
        let status = response.status();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(120).collect());
        return Err(format!("HTTP {status}: {message}"));
    }
    serde_json::from_str(
        &response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("failed to read response: {e}"))?,
    )
    .map_err(|e| format!("invalid json: {e}"))
}

/// Collect a release's assets and their digests from the GitHub API.
fn parse_assets(assets: &[serde_json::Value]) -> Vec<AssetHash> {
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
}

/// Extract a release from a GitHub API release object; `None` when the
/// object has no tag_name.
fn release_from_json(body: &serde_json::Value) -> Option<Release> {
    let version = body
        .get("tag_name")?
        .as_str()?
        .trim_start_matches(['v', 'V'])
        .to_string();
    let notes = body
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("No release notes.")
        .to_string();
    let assets = body
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|a| parse_assets(a))
        .unwrap_or_default();
    Some(Release {
        version,
        notes,
        assets,
    })
}

/// Fetch the release GitHub flags as "Latest". The highest-semver scan is
/// deliberately NOT part of this: it costs a second API request per repo
/// and would double dotup's rate-limit usage, so it is fetched lazily when
/// the user switches to `Highest` mode.
/// Color for the new version in an "old → new" transition, tinted by the
/// size of the bump: major red, minor yellow, patch dim. Non-semver
/// versions fall back to a generic "update" yellow.
fn severity_color(current: &str, latest: &str) -> Color {
    match (
        Version::parse(version_part(current)),
        Version::parse(version_part(latest)),
    ) {
        (Ok(c), Ok(l)) if l.major > c.major => RED,
        (Ok(c), Ok(l)) if l.minor > c.minor => YELLOW,
        (Ok(_), Ok(_)) => MUTED,
        _ => YELLOW,
    }
}

fn fetch_flagged_latest(repo: &str) -> std::result::Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body = github_get_json(&url)?;
    release_from_json(&body).ok_or_else(|| "no tag_name in response".to_string())
}

/// Highest semver among the most recent published, non-prerelease
/// releases. `None` when no tag parses as semver.
fn fetch_highest_semver(repo: &str) -> std::result::Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let body = github_get_json(&url)?;
    let releases = body
        .as_array()
        .ok_or_else(|| "releases: expected an array".to_string())?;
    Ok(best_semver_release(releases))
}

/// Pick the highest-semver release from GitHub API release objects,
/// skipping drafts, prereleases and tags that don't parse as semver.
fn best_semver_release(releases: &[serde_json::Value]) -> Option<Release> {
    releases
        .iter()
        .filter(|rel| {
            !rel.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
                && !rel
                    .get("prerelease")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
        })
        .filter_map(|rel| {
            let release = release_from_json(rel)?;
            let version = Version::parse(version_part(&release.version)).ok()?;
            Some((version, release))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, release)| release)
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
        let result = fetch_flagged_latest(&repo);
        // If the receiver is gone the app is exiting; ignore the error.
        let _ = tx.send(AppMsg::Check(CheckResult {
            generation,
            tool_index,
            result,
        }));
    });
}

/// Fetch the highest-semver release for one tool in the background
/// (triggered by switching to `Highest` mode).
fn spawn_highest_check(
    tx: mpsc::Sender<AppMsg>,
    generation: u64,
    tool_index: usize,
    repo: String,
) {
    thread::spawn(move || {
        let result = fetch_highest_semver(&repo);
        let _ = tx.send(AppMsg::HighestCheck {
            generation,
            tool_index,
            result,
        });
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
                release: None,
                check_error: None,
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

/// Dim hint shown under the status line when the two notions of "newest"
/// differ, pointing at the other mode's version.
fn other_version_hint(tool: &Tool, mode: Mode) -> Option<Line<'static>> {
    let info = tool.release.as_ref()?;
    let (selected, other, label) = match mode {
        Mode::Latest => (&info.latest, info.highest.as_ref()?, "highest semver"),
        Mode::Highest => (info.highest.as_ref()?, &info.latest, "github \"latest\""),
    };
    if version_part(&selected.version) == version_part(&other.version) {
        return None;
    }
    Some(Line::from(Span::styled(
        format!(" {label}: {}  (m: switch)", other.version),
        Style::default().fg(MUTED),
    )))
}

/// The latest/highest selector rendered as a segmented pill box, top
/// right: the active segment is accent-filled, the inactive one muted.
fn mode_pill(mode: Mode) -> Line<'static> {
    let on = Style::default()
        .bg(ACCENT)
        .fg(BG)
        .add_modifier(Modifier::BOLD);
    let off = Style::default().fg(MUTED);
    let (latest, highest) = match mode {
        Mode::Latest => (on, off),
        Mode::Highest => (off, on),
    };
    Line::from(vec![
        Span::styled(" latest ", latest),
        Span::styled(" · ", Style::default().fg(MUTED)),
        Span::styled(" highest ", highest),
    ])
    .right_aligned()
}

/// A footer key hint: the key as a small pill (ACCENT on SURFACE) with a
/// muted label next to it.
fn key_chip(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .bg(SURFACE)
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label} "), Style::default().fg(MUTED)),
    ]
}

fn ui(frame: &mut Frame, app: &mut App) {
    // Canvas: fill the whole screen with the palette base.
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        frame.area(),
    );

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

    // Upper section: tools table. The header carries a thin separator line
    // as its second row (clipped to each column's width).
    let header_style = Style::default()
        .fg(MUTED)
        .add_modifier(Modifier::BOLD);
    let separator =
        Line::from(Span::styled("─".repeat(64), Style::default().fg(BORDER)));
    let header = Row::new(vec![
        Cell::from(Text::from(vec![
            Line::from(Span::styled("TOOL", header_style)),
            separator.clone(),
        ])),
        Cell::from(Text::from(vec![
            Line::from(Span::styled("VERSION", header_style)),
            separator.clone(),
        ])),
        Cell::from(Text::from(vec![
            Line::from(Span::styled("STATUS", header_style)),
            separator,
        ])),
    ]);

    let spinner = SPINNER[app.tick % SPINNER.len()];

    let rows: Vec<Row> = app
        .visible
        .iter()
        .map(|&i| &app.tools[i])
        .map(|tool| {
            let sel = tool.selected_release(app.mode);
            // Attention rows keep full-color text; everything else is muted.
            let (name_style, version_line, status_line) = match (tool.status, sel) {
                (Status::UpdateAvailable, Some(rel)) => {
                    let sev = severity_color(&tool.current_version, &rel.version);
                    (
                        TEXT,
                        Line::from(vec![
                            Span::styled(
                                tool.current_version.clone(),
                                Style::default().fg(TEXT),
                            ),
                            Span::styled(" → ", Style::default().fg(MUTED)),
                            Span::styled(
                                rel.version.clone(),
                                Style::default()
                                    .fg(sev)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                "↑ ",
                                Style::default()
                                    .fg(YELLOW)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled("update available", Style::default().fg(YELLOW)),
                        ]),
                    )
                }
                (Status::UpToDate, Some(_)) => (
                    MUTED,
                    Line::from(Span::styled(
                        tool.current_version.clone(),
                        Style::default().fg(MUTED),
                    )),
                    Line::from(vec![
                        Span::styled(
                            "✔ ",
                            Style::default().fg(GREEN).add_modifier(Modifier::DIM),
                        ),
                        Span::styled("up to date", Style::default().fg(MUTED)),
                    ]),
                ),
                (Status::Checking, _) => (
                    TEXT,
                    Line::from(Span::styled(
                        tool.current_version.clone(),
                        Style::default().fg(MUTED),
                    )),
                    Line::from(Span::styled(
                        format!("{spinner} checking…"),
                        Style::default().fg(TEAL),
                    )),
                ),
                (Status::Error, _) => (
                    TEXT,
                    Line::from(Span::styled(
                        tool.current_version.clone(),
                        Style::default().fg(MUTED),
                    )),
                    Line::from(vec![
                        Span::styled("✖ ", Style::default().fg(RED)),
                        Span::styled("error", Style::default().fg(RED)),
                    ]),
                ),
                // Defensive: a status derived from a release that has since
                // gone missing, or not yet checked.
                (
                    Status::Unchecked | Status::UpdateAvailable | Status::UpToDate,
                    _,
                ) => (
                    MUTED,
                    Line::from(Span::styled(
                        tool.current_version.clone(),
                        Style::default().fg(MUTED),
                    )),
                    Line::from(Span::styled("-", Style::default().fg(MUTED))),
                ),
            };
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    tool.name.clone(),
                    Style::default().fg(name_style),
                ))),
                Cell::from(version_line),
                Cell::from(status_line),
            ])
        })
        .collect();

    // Tools title: pane name (accent when focused), filter state, and a
    // colored summary of what currently needs attention.
    let focused = app.focus == Focus::Tools;
    let title_style = Style::default()
        .fg(if focused { ACCENT } else { MUTED })
        .add_modifier(Modifier::BOLD);
    let mut title_spans = vec![Span::styled(" Tools ", title_style)];
    if !app.filter.is_empty() || app.search_input.is_some() {
        title_spans.push(Span::styled(
            format!(
                "/{}/ ({}/{}) ",
                app.active_pattern(),
                app.visible.len(),
                app.tools.len()
            ),
            Style::default().fg(MUTED),
        ));
    }
    let mut updates = 0usize;
    let mut errors = 0usize;
    let mut current = 0usize;
    for &i in &app.visible {
        match app.tools[i].status {
            Status::UpdateAvailable => updates += 1,
            Status::Error => errors += 1,
            Status::UpToDate => current += 1,
            _ => {}
        }
    }
    let summary: Vec<(usize, &str, Color)> = [
        (updates, "updates", YELLOW),
        (errors, "errors", RED),
        (current, "current", GREEN),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .collect();
    if !summary.is_empty() {
        title_spans.push(Span::raw(" "));
        for (n, (count, label, color)) in summary.iter().enumerate() {
            if n > 0 {
                title_spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
            }
            title_spans.push(Span::styled(
                format!("{count} {label}"),
                Style::default().fg(*color),
            ));
        }
    }

    let tools_border = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(BORDER)
    };

    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(34),
            Constraint::Length(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(Line::from(title_spans))
            .title(mode_pill(app.mode))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(tools_border),
    )
    .row_highlight_style(Style::default().bg(SURFACE).fg(ACCENT))
    .highlight_symbol("▌ ");

    frame.render_stateful_widget(table, body[0], &mut app.table_state);

    // Lower section: details of the selected tool
    let selected = app.selected_index().and_then(|i| app.tools.get(i));
    let details_focused = app.focus == Focus::Details;
    let details_title_style = Style::default()
        .fg(if details_focused { ACCENT } else { MUTED })
        .add_modifier(Modifier::BOLD);

    let (details_title, details_lines) = match selected {
        Some(tool) => {
            let title = Line::from(Span::styled(
                format!(" {} {} ", tool.name, tool.current_version),
                details_title_style,
            ));
            let mode = app.mode;
            let sel = tool.selected_release(mode);

            // Asset hashes section, shared by the update-available and
            // up-to-date views (whenever we have fetched release data).
            // Shown before the release notes since it is the actionable data;
            // long notes then follow below.
            let asset_lines = |assets: &[AssetHash], lines: &mut Vec<Line>| {
                // Skip checksum sidecar files; they add noise without value here.
                let assets: Vec<&AssetHash> = assets
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
                    "Assets (newest release):",
                    Style::default()
                        .fg(TEAL)
                        .add_modifier(Modifier::BOLD),
                )));
                for asset in assets {
                    match &asset.sha256 {
                        Some(hash) => lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {}", asset.name),
                                Style::default().fg(TEXT),
                            ),
                            Span::styled(
                                format!("  sha256:{hash}"),
                                Style::default().fg(MUTED),
                            ),
                        ])),
                        None => lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {}", asset.name),
                                Style::default().fg(TEXT),
                            ),
                            Span::styled(
                                "  (no digest published)",
                                Style::default().fg(MUTED),
                            ),
                        ])),
                    }
                }
                lines.push(Line::from(""));
            };

            match (tool.status, sel) {
                (Status::UpdateAvailable, Some(sel)) => {
                    let sev = severity_color(&tool.current_version, &sel.version);
                    let mut lines = vec![Line::from(vec![
                        Span::styled("Update available: ", Style::default().fg(YELLOW)),
                        Span::styled(
                            tool.current_version.clone(),
                            Style::default().fg(TEXT),
                        ),
                        Span::styled(" → ", Style::default().fg(MUTED)),
                        Span::styled(
                            sel.version.clone(),
                            Style::default().fg(sev).add_modifier(Modifier::BOLD),
                        ),
                    ])];
                    if let Some(hint) = other_version_hint(tool, mode) {
                        lines.push(hint);
                    }
                    lines.push(Line::from(""));
                    asset_lines(&sel.assets, &mut lines);
                    if sel.notes.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            "No release notes.",
                            Style::default().fg(MUTED),
                        )));
                    } else {
                        lines.push(Line::from(Span::styled(
                            "Release notes:",
                            Style::default()
                                .fg(TEAL)
                                .add_modifier(Modifier::BOLD),
                        )));
                        lines.extend(sel.notes.lines().map(|l| Line::from(l.to_string())));
                    }
                    (title, lines)
                }
                (Status::UpToDate, Some(sel)) => {
                    let mut lines = vec![Line::from(Span::styled(
                        "Up to date.",
                        Style::default().fg(GREEN),
                    ))];
                    if let Some(hint) = other_version_hint(tool, mode) {
                        lines.push(hint);
                    }
                    lines.push(Line::from(""));
                    asset_lines(&sel.assets, &mut lines);
                    (title, lines)
                }
                (Status::Checking, _) => (
                    title,
                    vec![Line::from(Span::styled(
                        "Checking for updates…",
                        Style::default().fg(TEAL),
                    ))],
                ),
                (Status::Error, _) => {
                    let mut lines = vec![Line::from(Span::styled(
                        "Failed to check for updates.",
                        Style::default().fg(RED),
                    ))];
                    if let Some(err) = &tool.check_error {
                        lines.push(Line::from(Span::styled(
                            err.clone(),
                            Style::default().fg(MUTED),
                        )));
                    }
                    (title, lines)
                }
                (Status::Unchecked, _) => (title, vec![Line::from("")]),
                // Defensive: a status derived from a release that has since
                // gone missing. Should not happen.
                (Status::UpdateAvailable | Status::UpToDate, None) => (title, vec![Line::from("")]),
            }
        }
        None => (
            Line::from(Span::styled(" Details ", details_title_style)),
            vec![Line::from("")],
        ),
    };

    let details_border = if details_focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(BORDER)
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
                .title_bottom(Line::from(Span::styled(
                    scroll_indicator,
                    Style::default().fg(MUTED),
                )))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(details_border),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.details_scroll, 0));

    frame.render_widget(details, body[1]);

    // Footer: key hints, or a transient notice if one is active
    let footer = if let Some(input) = &app.search_input {
        let mut spans = vec![
            Span::styled(
                format!(" /{input}_"),
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Enter: keep  Esc: clear", Style::default().fg(MUTED)),
        ];
        if app.filter_error {
            spans.push(Span::styled("  invalid regex", Style::default().fg(RED)));
        }
        Line::from(spans)
    } else if let Some((msg, color)) = &app.notice {
        Line::from(Span::styled(format!(" {msg}"), Style::default().fg(*color)))
    } else {
        let mut spans = vec![Span::raw(" ")];
        for (key, label) in [
            ("q", "quit"),
            ("r", "refresh"),
            ("m", "mode"),
            ("/", "search"),
            ("d", "dry-run"),
            ("u", "update"),
        ] {
            spans.extend(key_chip(key, label));
        }
        spans.push(Span::styled(
            "  ↑/↓ nav · ←/→ pane",
            Style::default().fg(MUTED),
        ));
        Line::from(spans)
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
            Span::styled("Version: ", Style::default().fg(MUTED)),
            Span::styled(plan.old_version.clone(), Style::default().fg(TEXT)),
            Span::styled("  →  ", Style::default().fg(MUTED)),
            Span::styled(
                plan.new_version.clone(),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        if plan.hashes.is_empty() {
            lines.push(Line::from(Span::styled(
                "No platform hashes to update.",
                Style::default().fg(MUTED),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Hashes:",
                Style::default()
                    .fg(TEAL)
                    .add_modifier(Modifier::BOLD),
            )));
            for h in &plan.hashes {
                let old = h.old.as_deref().unwrap_or("∅ (not in hashes.toml)");
                let new = h.new.as_deref().unwrap_or("?");
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", h.platform),
                        Style::default()
                            .fg(TEXT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ({})", h.asset_name),
                        Style::default().fg(MUTED),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    old: ", Style::default().fg(MUTED)),
                    Span::styled(short_hash(old), Style::default().fg(RED)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("    new: ", Style::default().fg(MUTED)),
                    Span::styled(short_hash(new), Style::default().fg(GREEN)),
                    Span::styled(
                        if h.needs_download { "  (downloaded)" } else { "" },
                        Style::default().fg(MUTED),
                    ),
                ]));
            }
        }

        for warning in &plan.warnings {
            lines.push(Line::from(Span::styled(
                format!("⚠ {warning}"),
                Style::default().fg(YELLOW),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            format!("{spinner} Preparing update…"),
            Style::default().fg(TEAL),
        )));
    }

    lines.push(Line::from(""));
    let hint = if !modal.ready {
        Line::from(Span::styled(
            "working… (Esc cancels)".to_string(),
            Style::default().fg(MUTED),
        ))
    } else if modal.dry_run {
        Line::from(Span::styled(
            "dry-run — no files were changed.  Esc: close",
            Style::default().fg(TEXT),
        ))
    } else if plan.hashes.is_empty() {
        Line::from(Span::styled(
            "nothing to write.  Esc: cancel",
            Style::default().fg(TEXT),
        ))
    } else {
        let mut spans = Vec::new();
        spans.extend(key_chip("Enter/y", "apply"));
        spans.push(Span::styled(
            " to tools.toml + hashes.toml  ",
            Style::default().fg(MUTED),
        ));
        spans.extend(key_chip("Esc/n", "cancel"));
        Line::from(spans)
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
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(TEXT)
                        .add_modifier(Modifier::BOLD),
                )))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(SURFACE)),
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
        app.handle_results(&tx);

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

            // While typing a search pattern, keys edit the pattern.
            if app.search_input.is_some() {
                match key.code {
                    KeyCode::Char(c) => app.search_push(c),
                    KeyCode::Backspace => app.search_pop(),
                    KeyCode::Enter => app.search_confirm(),
                    KeyCode::Esc => app.clear_filter(),
                    _ => {}
                }
                continue;
            }

            app.notice = None;
            match key.code {
                KeyCode::Char('/') if app.focus == Focus::Tools => app.start_search(),
                KeyCode::Esc if !app.filter.is_empty() => app.clear_filter(),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => {
                    let tx = tx.clone();
                    app.refresh(tx, true);
                }
                KeyCode::Char('m') => app.toggle_mode(&tx),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str, prerelease: bool, draft: bool) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "body": "notes",
            "prerelease": prerelease,
            "draft": draft,
        })
    }

    #[test]
    fn best_semver_release_prefers_highest_over_flagged_latest() {
        // Mirrors anomalyco/opencode: GitHub's "Latest" flag points at the
        // 1.x line while newer 2.x releases exist.
        let releases = [
            release_json("v2.3.2", false, false),
            release_json("v1.19.6", false, false),
            release_json("v2.4.0-rc1", true, false),
            release_json("v3.0.0", false, true),
            release_json("process-daemon-v0.6.0", false, false),
        ];
        let best = best_semver_release(&releases).expect("a best release");
        assert_eq!(best.version, "2.3.2");
    }

    #[test]
    fn best_semver_release_skips_non_semver_tags() {
        let releases = [release_json("release-2026", false, false)];
        assert!(best_semver_release(&releases).is_none());
    }

    #[test]
    fn highest_semver_is_an_update_where_flagged_latest_is_not() {
        // dotup at 2.2.6: the GitHub "Latest" release (1.19.6) is not an
        // update, but the highest semver (2.3.2) is.
        assert_eq!(compare_versions("2.2.6", "1.19.6"), Status::UpToDate);
        assert_eq!(compare_versions("2.2.6", "2.3.2"), Status::UpdateAvailable);
    }

    #[test]
    fn severity_color_tints_by_bump_size() {
        assert_eq!(severity_color("2.2.6", "3.0.0"), RED); // major
        assert_eq!(severity_color("2.2.6", "2.3.2"), YELLOW); // minor
        assert_eq!(severity_color("2.2.6", "2.2.7"), MUTED); // patch: dim
        assert_eq!(severity_color("jq-1.7", "jq-1.8"), YELLOW); // non-semver
    }
}
