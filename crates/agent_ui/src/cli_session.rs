use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agent_client_protocol::schema::v2 as acp_v2;
use collections::{HashMap, HashSet};
use gpui::{App, Entity, Global, TaskExt as _, Window};
use project::AgentId;
use task::{HideStrategy, RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use util::ResultExt as _;
use workspace::{PathList, Workspace};

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const ENTER_DELAY: Duration = Duration::from_millis(500);
const CODEX_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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

/// Book-keeping shared by every terminal session.
#[derive(Default)]
struct CliSessions {
    /// Threads whose title is already being polled.
    watched_threads: HashSet<ThreadId>,
    /// Labels of terminals started before their session had an id. Such a
    /// terminal can't be labelled after its session's resume command.
    started_terminal_labels: HashMap<acp_v2::SessionId, String>,
}

impl Global for CliSessions {}

/// Maps the label of a terminal tab back to the session it runs.
pub fn session_id_for_terminal_label(label: &str, cx: &App) -> Option<acp_v2::SessionId> {
    for agent in [CliAgent::Claude, CliAgent::Codex] {
        let prefix = format!("{} ", agent.resume_label(&acp_v2::SessionId::new("")));
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
    spawn_in_terminal_panel(
        workspace,
        &terminal_panel,
        agent.command(),
        agent.start_arguments(session_id, working_directory.as_deref()),
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
            spawn_in_terminal_panel(
                workspace,
                &terminal_panel,
                agent.command(),
                vec!["--session-id".into(), session_id.to_string()],
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
