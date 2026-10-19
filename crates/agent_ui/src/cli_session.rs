use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agent_client_protocol::schema::v2 as acp_v2;
use agent_settings::AgentSettings;
use anyhow::Result;
use collections::{HashMap, HashSet};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Entity, Global, Subscription, TaskExt as _, WeakEntity,
    Window, WindowHandle,
};
use project::AgentId;
use settings::{NotifyWhenAgentWaiting, Settings as _};
use task::{HideStrategy, RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::{self, TerminalPanel};
use ui::{AgentThreadStatus, IconName};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Pane, PathList, Workspace};

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths};
use crate::ui::{AgentNotification, AgentNotificationEvent};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const ENTER_DELAY: Duration = Duration::from_millis(500);
const CODEX_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TERMINAL_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Agents that ship a command line client which can run in the terminal panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliAgent {
    Claude,
    Codex,
}

impl CliAgent {
    pub fn name(self) -> &'static str {
        match self {
            CliAgent::Claude => "Claude",
            CliAgent::Codex => "Codex",
        }
    }

    pub fn icon(self) -> ui::IconName {
        match self {
            CliAgent::Claude => ui::IconName::AiClaude,
            CliAgent::Codex => ui::IconName::AiOpenAi,
        }
    }

    pub fn from_agent_id(agent_id: &AgentId) -> Option<Self> {
        match agent_id.0.as_ref() {
            agent_servers::CLAUDE_AGENT_ID => Some(CliAgent::Claude),
            agent_servers::CODEX_ID => Some(CliAgent::Codex),
            _ => None,
        }
    }

    fn agent_id(self) -> AgentId {
        match self {
            CliAgent::Claude => AgentId::new(agent_servers::CLAUDE_AGENT_ID),
            CliAgent::Codex => AgentId::new(agent_servers::CODEX_ID),
        }
    }

    fn command(self) -> &'static str {
        match self {
            CliAgent::Claude => "claude",
            CliAgent::Codex => "codex",
        }
    }

    fn resume_arguments(self, session_id: &acp_v2::SessionId) -> Vec<String> {
        match self {
            CliAgent::Claude => vec!["--resume".into(), session_id.to_string()],
            CliAgent::Codex => vec!["resume".into(), session_id.to_string()],
        }
    }

    /// Arguments that bring a session back. Claude only writes a session file
    /// once something was typed, and can't resume a session without one, so such
    /// a session is started again under the same id.
    fn start_arguments(
        self,
        session_id: &acp_v2::SessionId,
        working_directory: Option<&Path>,
    ) -> Vec<String> {
        let session_was_never_used = self == CliAgent::Claude
            && working_directory.is_some_and(|working_directory| {
                !self.title_file(session_id, working_directory).exists()
            });
        if session_was_never_used {
            vec!["--session-id".into(), session_id.to_string()]
        } else {
            self.resume_arguments(session_id)
        }
    }

    /// Appends a rename to the files the CLI reads titles from, the same way
    /// its own rename command does. Nothing is created: a session that was
    /// never used has no file yet.
    fn write_title(
        self,
        session_id: &acp_v2::SessionId,
        working_directory: &Path,
        title: &str,
    ) -> std::io::Result<()> {
        use std::io::Write as _;

        let lines = match self {
            CliAgent::Claude => vec![
                serde_json::json!({
                    "type": "custom-title",
                    "customTitle": title,
                    "sessionId": session_id.to_string(),
                }),
                serde_json::json!({
                    "type": "agent-name",
                    "agentName": title,
                    "sessionId": session_id.to_string(),
                }),
            ],
            CliAgent::Codex => vec![serde_json::json!({
                "id": session_id.to_string(),
                "thread_name": title,
                "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            })],
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.title_file(session_id, working_directory))?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    /// Label of the terminal task that resumes this session. It identifies the
    /// session's terminal tab.
    fn resume_label(self, session_id: &acp_v2::SessionId) -> String {
        format!(
            "{} {}",
            self.command(),
            self.resume_arguments(session_id).join(" ")
        )
    }

    /// File that changes whenever the session's title may have changed.
    fn title_file(self, session_id: &acp_v2::SessionId, working_directory: &Path) -> PathBuf {
        match self {
            // Claude keeps each session under `~/.claude/projects/<cwd>/`, where
            // `<cwd>` is the working directory with every non-alphanumeric
            // character replaced by `-`.
            CliAgent::Claude => {
                let encoded_directory: String = working_directory
                    .to_string_lossy()
                    .chars()
                    .map(|character| {
                        if character.is_ascii_alphanumeric() {
                            character
                        } else {
                            '-'
                        }
                    })
                    .collect();
                paths::home_dir()
                    .join(".claude")
                    .join("projects")
                    .join(encoded_directory)
                    .join(format!("{session_id}.jsonl"))
            }
            CliAgent::Codex => paths::home_dir().join(".codex").join("session_index.jsonl"),
        }
    }

    /// Arguments that make the session report its status through hooks, and
    /// starts polling that status. Only Claude has hooks for every state.
    fn status_arguments(
        self,
        session_id: &acp_v2::SessionId,
        terminal_labels: &[String],
        terminal_panel: &Entity<TerminalPanel>,
        window: &Window,
        cx: &mut App,
    ) -> Vec<String> {
        if self != CliAgent::Claude {
            return Vec::new();
        }
        let Some(settings_file) = write_claude_status_settings(session_id).log_err() else {
            return Vec::new();
        };
        watch_session_status(
            session_id.clone(),
            terminal_labels.to_vec(),
            terminal_panel.downgrade(),
            window.window_handle(),
            cx,
        );
        vec![
            "--settings".into(),
            settings_file.to_string_lossy().into_owned(),
        ]
    }

    fn read_title(self, title_file: &Path, session_id: &acp_v2::SessionId) -> Option<String> {
        match self {
            CliAgent::Claude => read_claude_title(title_file),
            CliAgent::Codex => read_codex_title(title_file, session_id),
        }
    }
}

/// Reads the latest title Claude wrote to a session file. A title set by the
/// user (`custom-title`) wins over the generated one (`ai-title`). Claude
/// writes `custom-title` once, near the start of the file, so the whole file
/// has to be scanned.
fn read_claude_title(session_file: &Path) -> Option<String> {
    use std::io::{BufRead as _, BufReader};

    let file = std::fs::File::open(session_file).log_err()?;
    let mut generated_title = None;
    let mut custom_title = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("-title\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let title_of = |key: &str| entry.get(key)?.as_str().map(str::to_string);
        match entry.get("type").and_then(|entry_type| entry_type.as_str()) {
            Some("ai-title") => generated_title = title_of("aiTitle").or(generated_title),
            Some("custom-title") => custom_title = title_of("customTitle").or(custom_title),
            _ => {}
        }
    }
    custom_title.or(generated_title)
}

/// Reads the latest `thread_name` Codex indexed for a session. The index is
/// shared by every session and only ever appended to.
fn read_codex_title(session_index: &Path, session_id: &acp_v2::SessionId) -> Option<String> {
    use std::io::{BufRead as _, BufReader};

    let file = std::fs::File::open(session_index).log_err()?;
    let session_id = session_id.to_string();
    let mut title = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains(&session_id) {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if entry.get("id").and_then(|id| id.as_str()) == Some(session_id.as_str()) {
            title = entry
                .get("thread_name")
                .and_then(|name| name.as_str())
                .map(str::to_string)
                .or(title);
        }
    }
    title
}

/// Book-keeping shared by every terminal session. Observers of this global are
/// notified when the status of a session changes.
#[derive(Default)]
pub struct CliSessions {
    /// Threads whose title is already being polled.
    watched_threads: HashSet<ThreadId>,
    /// Labels of terminals started before their session had an id. Such a
    /// terminal can't be labelled after its session's resume command.
    started_terminal_labels: HashMap<acp_v2::SessionId, String>,
    /// Sessions whose status file is being polled.
    watched_statuses: HashSet<acp_v2::SessionId>,
    /// Latest status reported by the hooks of each running session.
    statuses: HashMap<acp_v2::SessionId, AgentThreadStatus>,
    /// Pop-ups telling that a session waits for the user.
    notifications: HashMap<acp_v2::SessionId, SessionNotification>,
}

struct SessionNotification {
    windows: Vec<WindowHandle<AgentNotification>>,
    _subscriptions: Vec<Subscription>,
}

impl Global for CliSessions {}

/// Status of the session running in a terminal, as reported by its hooks.
pub fn session_status(session_id: &acp_v2::SessionId, cx: &App) -> Option<AgentThreadStatus> {
    cx.try_global::<CliSessions>()?
        .statuses
        .get(session_id)
        .copied()
}

/// Session run by the terminal shown in the active tab of `pane`.
pub fn session_id_for_pane(pane: &Entity<Pane>, cx: &App) -> Option<acp_v2::SessionId> {
    session_id_for_terminal_label(&terminal_panel::active_task_label(pane, cx)?, cx)
}

/// Maps the label of a terminal tab back to the session it runs.
pub fn session_id_for_terminal_label(label: &str, cx: &App) -> Option<acp_v2::SessionId> {
    for agent in [CliAgent::Claude, CliAgent::Codex] {
        // The resume label of an empty session id is the command followed by
        // its arguments and a trailing space.
        let prefix = agent.resume_label(&acp_v2::SessionId::new(""));
        if let Some(session_id) = label.strip_prefix(&prefix) {
            return Some(acp_v2::SessionId::new(session_id));
        }
    }
    cx.try_global::<CliSessions>()?
        .started_terminal_labels
        .iter()
        .find_map(|(session_id, started_label)| {
            (started_label == label).then(|| session_id.clone())
        })
}

/// Labels under which the terminal of a session may be found.
fn session_terminal_labels(
    agent: CliAgent,
    session_id: &acp_v2::SessionId,
    cx: &App,
) -> Vec<String> {
    let mut labels = vec![agent.resume_label(session_id)];
    if let Some(started_label) = cx
        .try_global::<CliSessions>()
        .and_then(|sessions| sessions.started_terminal_labels.get(session_id))
    {
        labels.push(started_label.clone());
    }
    labels
}

/// Renames the CLI session of a thread. The title shown by the sidebar and
/// the terminal tab follows through the title polling.
///
/// A running session is renamed with its own `/rename` command, since it
/// keeps its title in memory. Otherwise the name is appended to the files the
/// CLI reads titles from.
pub fn rename_session(
    agent_id: &AgentId,
    session_id: &acp_v2::SessionId,
    working_directory: Option<PathBuf>,
    title: String,
    terminal_panels: &[Entity<TerminalPanel>],
    cx: &mut App,
) {
    let Some(agent) = CliAgent::from_agent_id(agent_id) else {
        return;
    };
    let labels = session_terminal_labels(agent, session_id, cx);
    // Codex takes a fast burst of input for a paste and doesn't submit it, so
    // its Enter has to be a separate keypress.
    let (rename_command, enter_is_separate) = match agent {
        CliAgent::Claude => (format!("/rename {title}\r"), false),
        CliAgent::Codex => (format!("/rename {title}"), true),
    };
    let running_terminal = terminal_panels.iter().find_map(|terminal_panel| {
        let label = terminal_panel.update(cx, |terminal_panel, cx| {
            labels
                .iter()
                .find(|label| {
                    terminal_panel.send_input_to_task_terminal(label, &rename_command, cx)
                })
                .cloned()
        })?;
        Some((terminal_panel.downgrade(), label))
    });
    if let Some((terminal_panel, label)) = running_terminal {
        if enter_is_separate {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(ENTER_DELAY).await;
                terminal_panel
                    .update(cx, |terminal_panel, cx| {
                        terminal_panel.send_input_to_task_terminal(&label, "\r", cx);
                    })
                    .log_err();
            })
            .detach();
        }
        return;
    }

    let Some(working_directory) = working_directory else {
        return;
    };
    let session_id = session_id.clone();
    cx.background_executor()
        .spawn(async move {
            agent
                .write_title(&session_id, &working_directory, &title)
                .log_err();
        })
        .detach();
}

/// Opens an existing thread in the terminal panel instead of the agent panel.
/// Returns `false` when there is no terminal panel, so the caller can fall
/// back to the agent panel.
pub fn open_thread_in_terminal_panel(
    workspace: &Entity<Workspace>,
    agent_id: &AgentId,
    session_id: Option<&acp_v2::SessionId>,
    working_directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
        return false;
    };
    let session = session_id.zip(CliAgent::from_agent_id(agent_id));
    let Some((session_id, agent)) = session else {
        return spawn_in_terminal_panel(
            workspace,
            &terminal_panel,
            CliAgent::Claude.command(),
            Vec::new(),
            CliAgent::Claude.command().to_string(),
            None,
            working_directory,
            Vec::new(),
            window,
            cx,
        );
    };

    let reusable_labels = session_terminal_labels(agent, session_id, cx);
    if let Some(working_directory) = working_directory.clone() {
        let thread_id = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .map(|metadata| metadata.thread_id);
        if let Some(thread_id) = thread_id {
            watch_session_title(
                agent,
                thread_id,
                session_id.clone(),
                reusable_labels.clone(),
                terminal_panel.downgrade(),
                working_directory,
                cx,
            );
        }
    }
    let mut arguments = agent.start_arguments(session_id, working_directory.as_deref());
    // Writing the status settings resets the status of the session, so it is
    // skipped when its terminal is only focused again.
    let already_running = terminal_panel.update(cx, |terminal_panel, cx| {
        reusable_labels
            .iter()
            .any(|label| terminal_panel.has_running_task_terminal(label, cx))
    });
    if !already_running {
        arguments.extend(agent.status_arguments(
            session_id,
            &reusable_labels,
            &terminal_panel,
            window,
            cx,
        ));
    }
    spawn_in_terminal_panel(
        workspace,
        &terminal_panel,
        agent.command(),
        arguments,
        agent.resume_label(session_id),
        Some(session_id),
        working_directory,
        reusable_labels,
        window,
        cx,
    )
}

/// Starts a fresh session of the agent in a new terminal of the terminal panel
/// and registers it as a thread, so that it shows up in the sidebar and can be
/// resumed later. Returns `false` when there is no terminal panel, so the
/// caller can fall back to the agent panel.
pub fn open_new_thread_in_terminal_panel(
    workspace: &Entity<Workspace>,
    agent: CliAgent,
    folder_paths: PathList,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
        return false;
    };
    let working_directory = folder_paths.paths().first().cloned();

    match agent {
        CliAgent::Claude => {
            let session_id = acp_v2::SessionId::new(uuid::Uuid::new_v4().to_string());
            let resume_label = agent.resume_label(&session_id);
            let thread_id = register_thread(agent, &session_id, &folder_paths, cx);
            if let Some(working_directory) = working_directory.clone() {
                watch_session_title(
                    agent,
                    thread_id,
                    session_id.clone(),
                    vec![resume_label.clone()],
                    terminal_panel.downgrade(),
                    working_directory,
                    cx,
                );
            }
            let mut arguments = vec!["--session-id".into(), session_id.to_string()];
            arguments.extend(agent.status_arguments(
                &session_id,
                std::slice::from_ref(&resume_label),
                &terminal_panel,
                window,
                cx,
            ));
            spawn_in_terminal_panel(
                workspace,
                &terminal_panel,
                agent.command(),
                arguments,
                resume_label,
                Some(&session_id),
                working_directory,
                Vec::new(),
                window,
                cx,
            )
        }
        // Codex picks the session id itself, so the session is registered once
        // its rollout file shows up.
        CliAgent::Codex => {
            let started_label = format!("{} ({})", agent.command(), uuid::Uuid::new_v4());
            if let Some(working_directory) = working_directory.clone() {
                discover_codex_session(
                    started_label.clone(),
                    folder_paths,
                    working_directory,
                    terminal_panel.downgrade(),
                    cx,
                );
            }
            spawn_in_terminal_panel(
                workspace,
                &terminal_panel,
                agent.command(),
                Vec::new(),
                started_label,
                None,
                working_directory,
                Vec::new(),
                window,
                cx,
            )
        }
    }
}

fn register_thread(
    agent: CliAgent,
    session_id: &acp_v2::SessionId,
    folder_paths: &PathList,
    cx: &mut App,
) -> ThreadId {
    let now = chrono::Utc::now();
    let thread_id = ThreadId::new();
    ThreadMetadataStore::global(cx).update(cx, |store, cx| {
        store.save(
            ThreadMetadata {
                thread_id,
                session_id: Some(session_id.clone()),
                agent_id: agent.agent_id(),
                title: None,
                title_override: None,
                updated_at: now,
                created_at: Some(now),
                interacted_at: Some(now),
                worktree_paths: WorktreePaths::from_folder_paths(folder_paths),
                remote_connection: None,
                archived: false,
            },
            cx,
        );
    });
    thread_id
}

/// Keeps the thread title in sync with the title of the CLI session, until
/// the thread is deleted. The title file is polled rather than watched: the
/// native file watcher drops events when many directories are being watched.
fn watch_session_title(
    agent: CliAgent,
    thread_id: ThreadId,
    session_id: acp_v2::SessionId,
    terminal_labels: Vec<String>,
    terminal_panel: gpui::WeakEntity<TerminalPanel>,
    working_directory: PathBuf,
    cx: &mut App,
) {
    if !cx
        .default_global::<CliSessions>()
        .watched_threads
        .insert(thread_id)
    {
        return;
    }
    let title_file = agent.title_file(&session_id, &working_directory);
    cx.spawn(async move |cx| {
        let mut last_modified = None;
        let mut last_title = None;
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;

            let thread_still_exists = cx.update(|cx| {
                ThreadMetadataStore::global(cx)
                    .read(cx)
                    .entry(thread_id)
                    .is_some()
            });
            if !thread_still_exists {
                break;
            }

            let (modified, title) = cx
                .background_executor()
                .spawn({
                    let title_file = title_file.clone();
                    let session_id = session_id.clone();
                    async move {
                        let modified = std::fs::metadata(&title_file)
                            .and_then(|metadata| metadata.modified())
                            .ok();
                        let title = match modified {
                            Some(_) => agent.read_title(&title_file, &session_id),
                            None => None,
                        };
                        (modified, title)
                    }
                })
                .await;
            if modified.is_none() || modified == last_modified {
                continue;
            }
            last_modified = modified;
            let Some(title) = title else {
                continue;
            };
            if last_title.as_ref() == Some(&title) {
                continue;
            }
            last_title = Some(title.clone());
            cx.update(|cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.set_generated_title(thread_id, title.clone().into(), cx)
                });
                terminal_panel
                    .update(cx, |terminal_panel, cx| {
                        for label in &terminal_labels {
                            terminal_panel.set_task_terminal_title(label, &title, cx);
                        }
                    })
                    .ok();
            });
        }
        cx.update(|cx| {
            cx.default_global::<CliSessions>()
                .watched_threads
                .remove(&thread_id)
        });
    })
    .detach();
}

/// Finds the session a freshly started `codex` terminal created, by looking
/// for a new rollout file in today's session directory that was started in the
/// same working directory. The session is then registered as a thread.
fn discover_codex_session(
    started_label: String,
    folder_paths: PathList,
    working_directory: PathBuf,
    terminal_panel: gpui::WeakEntity<TerminalPanel>,
    cx: &mut App,
) {
    let started_at = SystemTime::now() - Duration::from_secs(2);
    cx.spawn(async move |cx| {
        let deadline = SystemTime::now() + CODEX_DISCOVERY_TIMEOUT;
        while SystemTime::now() < deadline {
            cx.background_executor().timer(POLL_INTERVAL).await;

            let known_session_ids: HashSet<String> = cx.update(|cx| {
                ThreadMetadataStore::global(cx)
                    .read(cx)
                    .entries()
                    .filter_map(|metadata| metadata.session_id.as_ref().map(ToString::to_string))
                    .collect()
            });
            let session_id = cx
                .background_executor()
                .spawn({
                    let working_directory = working_directory.clone();
                    async move {
                        find_new_codex_session(&working_directory, started_at, &known_session_ids)
                    }
                })
                .await;
            let Some(session_id) = session_id else {
                continue;
            };

            cx.update(|cx| {
                let session_id = acp_v2::SessionId::new(session_id);
                let thread_id = register_thread(CliAgent::Codex, &session_id, &folder_paths, cx);
                cx.default_global::<CliSessions>()
                    .started_terminal_labels
                    .insert(session_id.clone(), started_label.clone());
                watch_session_title(
                    CliAgent::Codex,
                    thread_id,
                    session_id,
                    vec![started_label],
                    terminal_panel,
                    working_directory,
                    cx,
                );
            });
            return;
        }
    })
    .detach();
}

fn find_new_codex_session(
    working_directory: &Path,
    started_at: SystemTime,
    known_session_ids: &HashSet<String>,
) -> Option<String> {
    use std::io::{BufRead as _, BufReader};

    let today = chrono::Local::now();
    let session_directory = paths::home_dir()
        .join(".codex")
        .join("sessions")
        .join(today.format("%Y").to_string())
        .join(today.format("%m").to_string())
        .join(today.format("%d").to_string());
    for entry in std::fs::read_dir(session_directory).ok()?.flatten() {
        let created_recently = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified >= started_at);
        if !created_recently {
            continue;
        }
        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let mut first_line = String::new();
        if BufReader::new(file).read_line(&mut first_line).is_err() {
            continue;
        }
        let Ok(session_meta) = serde_json::from_str::<serde_json::Value>(&first_line) else {
            continue;
        };
        let payload = session_meta.get("payload")?;
        let id = payload.get("id").and_then(|id| id.as_str());
        let cwd = payload.get("cwd").and_then(|cwd| cwd.as_str());
        if let (Some(id), Some(cwd)) = (id, cwd)
            && Path::new(cwd) == working_directory
            && !known_session_ids.contains(id)
        {
            return Some(id.to_string());
        }
    }
    None
}

/// Directory of the files through which sessions report their status. The
/// path of the settings file is passed unquoted to the shell, so it must not
/// contain spaces, which rules out the application support directory.
fn status_directory() -> PathBuf {
    std::env::temp_dir().join("zed-agent-cli")
}

fn status_file(session_id: &acp_v2::SessionId) -> PathBuf {
    status_directory().join(format!("{session_id}.status.json"))
}

/// Writes Claude settings whose hooks copy each hook input to the session's
/// status file, and returns their path. A stale status file is removed, so
/// the new session starts out as idle.
fn write_claude_status_settings(session_id: &acp_v2::SessionId) -> Result<PathBuf> {
    let directory = status_directory();
    std::fs::create_dir_all(&directory)?;
    let status_file = status_file(session_id);
    if let Err(error) = std::fs::remove_file(&status_file)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    let quoted_status_file = format!("'{}'", status_file.to_string_lossy().replace('\'', "'\\''"));
    let hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{ "type": "command", "command": format!("cat > {quoted_status_file}") }],
    }]);
    let settings = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": hook,
            "PreToolUse": hook,
            "PostToolUse": hook,
            "Notification": hook,
            "Stop": hook,
        }
    });
    let settings_file = directory.join(format!("{session_id}.settings.json"));
    std::fs::write(&settings_file, serde_json::to_vec(&settings)?)?;
    Ok(settings_file)
}

/// Maps the input of the last hook Claude ran to the session's status. `None`
/// means the hook says nothing about it, like the reminder sent when Claude
/// has been idle for a while.
fn status_from_hook_input(hook_input: &serde_json::Value) -> Option<AgentThreadStatus> {
    let field = |key: &str| hook_input.get(key).and_then(|value| value.as_str());
    match field("hook_event_name")? {
        "UserPromptSubmit" | "PostToolUse" => Some(AgentThreadStatus::Running),
        "PreToolUse" => match field("tool_name") {
            Some("AskUserQuestion" | "ExitPlanMode") => {
                Some(AgentThreadStatus::WaitingForConfirmation)
            }
            _ => Some(AgentThreadStatus::Running),
        },
        "Notification" => {
            let asks_the_user = match field("notification_type") {
                Some(notification_type) => {
                    matches!(
                        notification_type,
                        "permission_prompt" | "elicitation_dialog"
                    )
                }
                // Older versions don't send a type, only the message.
                None => field("message").is_some_and(|message| message.contains("permission")),
            };
            asks_the_user.then_some(AgentThreadStatus::WaitingForConfirmation)
        }
        "Stop" => Some(AgentThreadStatus::Completed),
        _ => None,
    }
}

/// Polls the status file of a session until its terminal stops running.
fn watch_session_status(
    session_id: acp_v2::SessionId,
    terminal_labels: Vec<String>,
    terminal_panel: gpui::WeakEntity<TerminalPanel>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    if !cx
        .default_global::<CliSessions>()
        .watched_statuses
        .insert(session_id.clone())
    {
        return;
    }
    let status_file = status_file(&session_id);
    let started_at = SystemTime::now();
    cx.spawn(async move |cx| {
        let mut terminal_was_running = false;
        let mut last_modified = None;
        loop {
            cx.background_executor().timer(STATUS_POLL_INTERVAL).await;

            let terminal_is_running = cx.update(|cx| {
                terminal_panel
                    .update(cx, |terminal_panel, cx| {
                        terminal_labels
                            .iter()
                            .any(|label| terminal_panel.has_running_task_terminal(label, cx))
                    })
                    .unwrap_or(false)
            });
            if terminal_is_running {
                terminal_was_running = true;
            } else if terminal_was_running
                || started_at.elapsed().unwrap_or_default() > TERMINAL_START_TIMEOUT
            {
                break;
            }

            let (modified, hook_input) = cx
                .background_executor()
                .spawn({
                    let status_file = status_file.clone();
                    async move {
                        let modified = std::fs::metadata(&status_file)
                            .and_then(|metadata| metadata.modified())
                            .ok();
                        let hook_input = modified
                            .and_then(|_| std::fs::read(&status_file).ok())
                            .and_then(|contents| {
                                serde_json::from_slice::<serde_json::Value>(&contents).ok()
                            });
                        (modified, hook_input)
                    }
                })
                .await;
            if modified.is_none() || modified == last_modified {
                continue;
            }
            // A hook may still be writing the file. It is read again on the
            // next poll, even if its modification time stays the same.
            let Some(hook_input) = hook_input else {
                continue;
            };
            last_modified = modified;
            let Some(status) = status_from_hook_input(&hook_input) else {
                continue;
            };
            cx.update(|cx| {
                let previous_status = session_status(&session_id, cx);
                if previous_status != Some(status) {
                    cx.default_global::<CliSessions>()
                        .statuses
                        .insert(session_id.clone(), status);
                    notify_status_change(
                        &session_id,
                        previous_status,
                        status,
                        &terminal_labels,
                        &terminal_panel,
                        window,
                        cx,
                    );
                }
            });
        }
        cx.update(|cx| {
            let sessions = cx.default_global::<CliSessions>();
            sessions.watched_statuses.remove(&session_id);
            sessions.statuses.remove(&session_id);
            dismiss_session_notification(&session_id, cx);
        });
    })
    .detach();
}

/// Tells the user that a session waits for them, the way the agent panel does
/// for its threads: with a sound and a pop-up, unless they are looking at the
/// session's terminal.
fn notify_status_change(
    session_id: &acp_v2::SessionId,
    previous_status: Option<AgentThreadStatus>,
    status: AgentThreadStatus,
    terminal_labels: &[String],
    terminal_panel: &WeakEntity<TerminalPanel>,
    window_handle: AnyWindowHandle,
    cx: &mut App,
) {
    dismiss_session_notification(session_id, cx);
    let Some((caption, icon)) = status_change_notification(previous_status, status) else {
        return;
    };
    let Some(terminal_panel) = terminal_panel.upgrade() else {
        return;
    };
    let metadata = ThreadMetadataStore::global(cx)
        .read(cx)
        .entry_by_session(session_id)
        .cloned();
    let title = metadata
        .as_ref()
        .map(|metadata| metadata.display_title())
        .unwrap_or_else(|| CliAgent::Claude.name().into());
    let project_name = metadata.as_ref().and_then(|metadata| {
        let path = metadata.folder_paths().paths().first()?.clone();
        Some(path.file_name()?.to_string_lossy().into_owned())
    });

    window_handle
        .update(cx, |_, window, cx| {
            let visible = window.is_window_active()
                && terminal_labels.iter().any(|label| {
                    terminal_panel.update(cx, |terminal_panel, cx| {
                        terminal_panel.is_task_terminal_focused(label, window, cx)
                    })
                });
            #[cfg(feature = "audio")]
            if AgentSettings::get_global(cx)
                .play_sound_when_agent_done
                .should_play(visible)
            {
                audio::Audio::play_sound(audio::Sound::AgentDone, cx);
            }
            if visible {
                return;
            }
            let screens = match AgentSettings::get_global(cx).notify_when_agent_waiting {
                NotifyWhenAgentWaiting::PrimaryScreen => cx.primary_display().into_iter().collect(),
                NotifyWhenAgentWaiting::AllScreens => cx.displays(),
                NotifyWhenAgentWaiting::Never => Vec::new(),
            };
            if screens.is_empty() {
                return;
            }
            window.request_attention();

            let mut windows = Vec::new();
            let mut subscriptions = Vec::new();
            for screen in screens {
                let Some(pop_up_window) = cx
                    .open_window(AgentNotification::window_options(screen, cx), |_, cx| {
                        cx.new(|_| {
                            AgentNotification::new(
                                title.clone(),
                                Some(caption.into()),
                                icon,
                                project_name.clone(),
                            )
                        })
                    })
                    .log_err()
                else {
                    continue;
                };
                let Some(pop_up) = pop_up_window.entity(cx).log_err() else {
                    continue;
                };
                subscriptions.push(cx.subscribe(&pop_up, {
                    let session_id = session_id.clone();
                    let terminal_labels = terminal_labels.to_vec();
                    let terminal_panel = terminal_panel.downgrade();
                    move |_, event: &AgentNotificationEvent, cx| {
                        if let AgentNotificationEvent::Accepted = event {
                            show_session_terminal(
                                &terminal_labels,
                                &terminal_panel,
                                window_handle,
                                cx,
                            );
                        }
                        dismiss_session_notification(&session_id, cx);
                    }
                }));
                windows.push(pop_up_window);
            }
            cx.default_global::<CliSessions>().notifications.insert(
                session_id.clone(),
                SessionNotification {
                    windows,
                    _subscriptions: subscriptions,
                },
            );
        })
        .log_err();
}

/// Caption and icon of the notification for a status change, if it calls
/// for one: the session finished working, or waits for an answer.
fn status_change_notification(
    previous_status: Option<AgentThreadStatus>,
    status: AgentThreadStatus,
) -> Option<(&'static str, IconName)> {
    match status {
        AgentThreadStatus::Completed if previous_status == Some(AgentThreadStatus::Running) => {
            Some(("Finished", IconName::ZedAssistant))
        }
        AgentThreadStatus::WaitingForConfirmation => Some(("Waiting for input", IconName::Info)),
        _ => None,
    }
}

/// Brings the window, the workspace and the terminal of a session to the front.
fn show_session_terminal(
    terminal_labels: &[String],
    terminal_panel: &WeakEntity<TerminalPanel>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let Some(terminal_panel) = terminal_panel.upgrade() else {
        return;
    };
    cx.activate(true);
    let Some(window) = window.downcast::<MultiWorkspace>() else {
        return;
    };
    window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            let workspace = multi_workspace
                .workspaces()
                .find(|workspace| {
                    workspace.read(cx).panel::<TerminalPanel>(cx).as_ref() == Some(&terminal_panel)
                })
                .cloned();
            if let Some(workspace) = workspace {
                multi_workspace.activate(workspace, None, window, cx);
            }
            terminal_panel.update(cx, |terminal_panel, cx| {
                terminal_labels
                    .iter()
                    .any(|label| terminal_panel.activate_running_task_terminal(label, window, cx))
            });
        })
        .log_err();
}

fn dismiss_session_notification(session_id: &acp_v2::SessionId, cx: &mut App) {
    let Some(notification) = cx
        .default_global::<CliSessions>()
        .notifications
        .remove(session_id)
    else {
        return;
    };
    for window in notification.windows {
        window
            .update(cx, |_, window, _| window.remove_window())
            .log_err();
    }
}

fn spawn_in_terminal_panel(
    workspace: &Entity<Workspace>,
    terminal_panel: &Entity<TerminalPanel>,
    command: &'static str,
    args: Vec<String>,
    command_label: String,
    session_id: Option<&acp_v2::SessionId>,
    working_directory: Option<PathBuf>,
    reusable_labels: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let already_running = terminal_panel.update(cx, |terminal_panel, cx| {
        reusable_labels
            .iter()
            .any(|label| terminal_panel.activate_running_task_terminal(label, window, cx))
    });
    if already_running {
        workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<TerminalPanel>(window, cx);
        });
        return true;
    }

    // The task's `RevealStrategy::Always` focuses the panel once its terminal
    // exists. Focusing it earlier makes the still empty panel open a shell.
    let task = SpawnInTerminal {
        id: TaskId(format!(
            "resume-agent-session-{}",
            session_id.map(ToString::to_string).unwrap_or_default()
        )),
        full_label: command_label.clone(),
        label: command_label.clone(),
        command: Some(command.to_string()),
        args,
        command_label,
        cwd: working_directory,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Dock,
        hide: HideStrategy::Never,
        shell: Shell::System,
        show_summary: false,
        show_command: false,
        show_rerun: false,
        ..Default::default()
    };

    terminal_panel
        .update(cx, |terminal_panel, cx| {
            terminal_panel.spawn_task(&task, window, cx)
        })
        .detach_and_log_err(cx);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_session_id_for_terminal_label(cx: &mut TestAppContext) {
        let session_id = acp_v2::SessionId::new("0b5f6c1e-7d3a-4c39-9a7e-2f1d8e6b4a90");
        cx.update(|cx| {
            for agent in [CliAgent::Claude, CliAgent::Codex] {
                assert_eq!(
                    session_id_for_terminal_label(&agent.resume_label(&session_id), cx),
                    Some(session_id.clone())
                );
            }
            assert_eq!(session_id_for_terminal_label("claude", cx), None);
        });
    }

    #[test]
    fn test_status_change_notification() {
        let caption = |previous_status, status| {
            status_change_notification(previous_status, status).map(|(caption, _)| caption)
        };
        assert_eq!(
            caption(
                Some(AgentThreadStatus::Running),
                AgentThreadStatus::Completed
            ),
            Some("Finished")
        );
        assert_eq!(
            caption(
                Some(AgentThreadStatus::Running),
                AgentThreadStatus::WaitingForConfirmation
            ),
            Some("Waiting for input")
        );
        assert_eq!(
            caption(
                Some(AgentThreadStatus::WaitingForConfirmation),
                AgentThreadStatus::Running
            ),
            None
        );
        assert_eq!(caption(None, AgentThreadStatus::Completed), None);
    }

    #[test]
    fn test_status_from_hook_input() {
        let status = |hook_input: serde_json::Value| status_from_hook_input(&hook_input);
        assert_eq!(
            status(serde_json::json!({ "hook_event_name": "UserPromptSubmit" })),
            Some(AgentThreadStatus::Running)
        );
        assert_eq!(
            status(serde_json::json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" })),
            Some(AgentThreadStatus::Running)
        );
        assert_eq!(
            status(serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "AskUserQuestion",
            })),
            Some(AgentThreadStatus::WaitingForConfirmation)
        );
        assert_eq!(
            status(serde_json::json!({
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            })),
            Some(AgentThreadStatus::WaitingForConfirmation)
        );
        assert_eq!(
            status(serde_json::json!({
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
            })),
            None
        );
        assert_eq!(
            status(serde_json::json!({
                "hook_event_name": "Notification",
                "message": "Claude needs your permission to use Bash",
            })),
            Some(AgentThreadStatus::WaitingForConfirmation)
        );
        assert_eq!(
            status(serde_json::json!({ "hook_event_name": "Stop" })),
            Some(AgentThreadStatus::Completed)
        );
    }
}
