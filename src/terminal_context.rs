//! Privacy-conscious terminal and project context extraction.
//!
//! This module deliberately handles process structure and executable basenames only. It never
//! asks tmux for pane content, history, a pane title, or command arguments, and it never invokes a
//! shell. Paths leave the resolver only through [`PrivatePath`].

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

const CLIENT_FORMAT: &str = "#{client_pid}\t#{session_name}\t#{client_activity}";
const PANE_FORMAT: &str = "#{session_name}\t#{window_name}\t#{window_index}\t#{pane_id}\t#{pane_index}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}";
const MAX_PROCESS_COUNT: usize = 32_768;
const MAX_PROCESS_DEPTH: usize = 64;
const MAX_TOOL_CANDIDATES: usize = 32;
const MAX_TMUX_NAME_CHARS: usize = 120;
const MAX_EXECUTABLE_CHARS: usize = 120;
const MAX_GIT_FILE_BYTES: usize = 4_096;

/// The small, privacy-safe subset of focused-window state needed by terminal extractors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusedWindow {
    pub(crate) app_id: String,
    pub(crate) pid: Option<i32>,
}

impl FocusedWindow {
    pub(crate) fn new(app_id: impl Into<String>, pid: Option<i32>) -> Self {
        Self {
            app_id: app_id.into(),
            pid: pid.filter(|pid| *pid > 0),
        }
    }
}

/// Generic terminal integration contract. Implementations must return normal source absence and
/// ambiguity as typed outcomes, not as fatal collector errors.
pub(crate) trait TerminalContextExtractor {
    fn supports(&self, focused: &FocusedWindow) -> bool;

    fn extract(
        &mut self,
        focused: &FocusedWindow,
        processes: &ProcessSnapshot,
    ) -> Result<TerminalExtraction, TerminalExtractError>;
}

#[derive(Debug)]
pub(crate) struct TerminalExtractError {
    category: &'static str,
}

impl TerminalExtractError {
    fn invariant() -> Self {
        Self {
            category: "internal_invariant",
        }
    }

    pub(crate) fn category(&self) -> &'static str {
        self.category
    }
}

impl fmt::Display for TerminalExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.category)
    }
}

impl std::error::Error for TerminalExtractError {}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TerminalExtraction {
    NotApplicable(NotApplicableReason),
    Unresolved(TerminalAmbiguity),
    Resolved(TerminalContext),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotApplicableReason {
    UnsupportedApplication,
    MissingFocusedPid,
    NoTmuxDescendant,
    MissingTmuxServer,
    NoAttachedClient,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalAmbiguity {
    pub(crate) reason: AmbiguityReason,
    pub(crate) candidate_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AmbiguityReason {
    MultipleEligibleClients,
    StaleClients,
    TmuxTimeout,
    TmuxOutputLimit,
    MalformedTmuxOutput,
    PaneUnavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TerminalContext {
    pub(crate) terminal_kind: TerminalKind,
    pub(crate) match_method: TerminalMatchMethod,
    pub(crate) match_confidence: f64,
    pub(crate) candidate_count: usize,
    pub(crate) tmux_client_pid: i32,
    pub(crate) tmux_session: String,
    pub(crate) tmux_window: String,
    pub(crate) tmux_window_index: u32,
    pub(crate) tmux_pane: String,
    pub(crate) tmux_pane_index: u32,
    pub(crate) tmux_pane_pid: i32,
    pub(crate) cwd: PrivatePath,
    pub(crate) foreground: ForegroundSelection,
    pub(crate) project: ProjectResolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Tmux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalMatchMethod {
    ExactDescendantClient,
    UniqueRecentDescendantClient,
}

/// A process record intentionally containing no argument vector or command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pid: i32,
    parent_pid: i32,
    executable: String,
    process_group: Option<i32>,
    foreground_process_group: Option<i32>,
}

impl ProcessInfo {
    pub(crate) fn new(pid: i32, parent_pid: i32, executable: impl AsRef<OsStr>) -> Self {
        Self {
            pid,
            parent_pid,
            executable: executable_basename(executable.as_ref()).unwrap_or_default(),
            process_group: None,
            foreground_process_group: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn executable(&self) -> &str {
        &self.executable
    }

    #[cfg(test)]
    pub(crate) fn with_process_groups(
        mut self,
        process_group: Option<i32>,
        foreground_process_group: Option<i32>,
    ) -> Self {
        self.process_group = process_group.filter(|group| *group > 0);
        self.foreground_process_group = foreground_process_group.filter(|group| *group > 0);
        self
    }
}

/// Bounded process snapshot with cycle-safe ancestry operations.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessSnapshot {
    by_pid: HashMap<i32, ProcessInfo>,
}

impl ProcessSnapshot {
    pub(crate) fn new(processes: impl IntoIterator<Item = ProcessInfo>) -> Self {
        let mut by_pid = HashMap::new();
        for process in processes.into_iter().take(MAX_PROCESS_COUNT) {
            if process.pid > 0 && !process.executable.is_empty() {
                by_pid.entry(process.pid).or_insert(process);
            }
        }
        Self { by_pid }
    }

    pub(crate) fn get(&self, pid: i32) -> Option<&ProcessInfo> {
        self.by_pid.get(&pid)
    }

    #[cfg(test)]
    pub(crate) fn is_descendant_of(&self, pid: i32, ancestor: i32) -> bool {
        if pid <= 0 || ancestor <= 0 || pid == ancestor {
            return false;
        }
        let mut current = pid;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PROCESS_DEPTH {
            if !seen.insert(current) {
                return false;
            }
            let Some(process) = self.by_pid.get(&current) else {
                return false;
            };
            if process.parent_pid == ancestor {
                return true;
            }
            if process.parent_pid <= 0 || process.parent_pid == current {
                return false;
            }
            current = process.parent_pid;
        }
        false
    }

    fn descendants(&self, root: i32) -> Vec<(&ProcessInfo, usize)> {
        let mut result = Vec::new();
        for process in self.by_pid.values() {
            if process.pid == root {
                continue;
            }
            if let Some(depth) = self.depth_below(process.pid, root) {
                result.push((process, depth));
            }
        }
        result.sort_by_key(|(process, depth)| (*depth, process.pid));
        result
    }

    fn depth_below(&self, pid: i32, root: i32) -> Option<usize> {
        let mut current = pid;
        let mut seen = HashSet::new();
        for depth in 1..=MAX_PROCESS_DEPTH {
            if !seen.insert(current) {
                return None;
            }
            let process = self.by_pid.get(&current)?;
            if process.parent_pid == root {
                return Some(depth);
            }
            if process.parent_pid <= 0 || process.parent_pid == current {
                return None;
            }
            current = process.parent_pid;
        }
        None
    }

    fn tmux_descendant_pids(&self, root: i32) -> HashSet<i32> {
        self.descendants(root)
            .into_iter()
            .filter(|(process, _)| process.executable.eq_ignore_ascii_case("tmux"))
            .map(|(process, _)| process.pid)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolDefinition {
    pub(crate) canonical_name: String,
    pub(crate) executable_names: Vec<String>,
    pub(crate) kind: ToolKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Agent,
    Editor,
    Shell,
}

/// Adapter/config supplied registry. Matching never examines command arguments.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolRegistry {
    by_executable: HashMap<String, (String, ToolKind)>,
}

impl ToolRegistry {
    pub(crate) fn new(definitions: impl IntoIterator<Item = ToolDefinition>) -> Self {
        let mut by_executable = HashMap::new();
        for definition in definitions {
            let canonical = sanitize_executable(&definition.canonical_name);
            if canonical.is_empty() {
                continue;
            }
            for executable in definition.executable_names {
                let executable = sanitize_executable(&executable).to_ascii_lowercase();
                if !executable.is_empty() {
                    by_executable
                        .entry(executable)
                        .or_insert((canonical.clone(), definition.kind));
                }
            }
        }
        Self { by_executable }
    }

    pub(crate) fn common_defaults() -> Self {
        Self::new([
            tool("pi", &["pi"], ToolKind::Agent),
            tool("claude", &["claude"], ToolKind::Agent),
            tool("codex", &["codex"], ToolKind::Agent),
            tool("aider", &["aider"], ToolKind::Agent),
            tool("nvim", &["nvim"], ToolKind::Editor),
            tool("vim", &["vim", "vi"], ToolKind::Editor),
            tool(
                "shell",
                &["bash", "zsh", "fish", "sh", "dash", "nu"],
                ToolKind::Shell,
            ),
        ])
    }

    fn lookup(&self, executable: &str) -> Option<(&str, ToolKind)> {
        self.by_executable
            .get(&executable.to_ascii_lowercase())
            .map(|(name, kind)| (name.as_str(), *kind))
    }
}

fn tool(name: &str, executables: &[&str], kind: ToolKind) -> ToolDefinition {
    ToolDefinition {
        canonical_name: name.to_string(),
        executable_names: executables
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        kind,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolCandidate {
    pub(crate) pid: i32,
    pub(crate) executable: String,
    pub(crate) tool: String,
    pub(crate) kind: ToolKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForegroundSelection {
    pub(crate) executable: Option<String>,
    pub(crate) recognized_tool: Option<String>,
    pub(crate) candidates: Vec<ToolCandidate>,
    pub(crate) ambiguous: bool,
    pub(crate) method: ForegroundMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForegroundMethod {
    TerminalForegroundProcessGroup,
    UniqueAgentDescendant,
    DeepestRecognizedDescendant,
    TmuxCurrentExecutable,
    Unresolved,
}

fn select_foreground(
    pane_pid: i32,
    tmux_current_executable: &str,
    processes: &ProcessSnapshot,
    registry: &ToolRegistry,
) -> ForegroundSelection {
    let descendants = processes.descendants(pane_pid);
    let foreground_group = processes
        .get(pane_pid)
        .and_then(|process| process.foreground_process_group)
        .or_else(|| {
            descendants
                .iter()
                .find_map(|(process, _)| process.foreground_process_group)
        });

    let mut candidates = descendants
        .iter()
        .filter_map(|(process, depth)| {
            let (tool, kind) = registry.lookup(&process.executable)?;
            Some((
                ToolCandidate {
                    pid: process.pid,
                    executable: process.executable.clone(),
                    tool: tool.to_string(),
                    kind,
                },
                *depth,
                process.process_group,
            ))
        })
        .take(MAX_TOOL_CANDIDATES)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(candidate, depth, _)| (*depth, candidate.pid));
    let public_candidates = candidates
        .iter()
        .map(|(candidate, _, _)| candidate.clone())
        .collect::<Vec<_>>();

    if let Some(group) = foreground_group {
        let in_foreground = candidates
            .iter()
            .filter(|(_, _, process_group)| *process_group == Some(group))
            .collect::<Vec<_>>();
        if in_foreground.len() == 1 {
            let candidate = &in_foreground[0].0;
            return ForegroundSelection {
                executable: Some(candidate.executable.clone()),
                recognized_tool: Some(candidate.tool.clone()),
                candidates: public_candidates,
                ambiguous: false,
                method: ForegroundMethod::TerminalForegroundProcessGroup,
            };
        }
        if in_foreground.len() > 1 {
            return ForegroundSelection {
                executable: None,
                recognized_tool: None,
                candidates: public_candidates,
                ambiguous: true,
                method: ForegroundMethod::TerminalForegroundProcessGroup,
            };
        }
    }

    let agents = candidates
        .iter()
        .filter(|(candidate, _, _)| candidate.kind == ToolKind::Agent)
        .collect::<Vec<_>>();
    if agents.len() == 1 {
        let candidate = &agents[0].0;
        return ForegroundSelection {
            executable: Some(candidate.executable.clone()),
            recognized_tool: Some(candidate.tool.clone()),
            candidates: public_candidates,
            ambiguous: false,
            method: ForegroundMethod::UniqueAgentDescendant,
        };
    }
    if agents.len() > 1 {
        return ForegroundSelection {
            executable: None,
            recognized_tool: None,
            candidates: public_candidates,
            ambiguous: true,
            method: ForegroundMethod::Unresolved,
        };
    }

    if let Some(max_depth) = candidates.iter().map(|(_, depth, _)| *depth).max() {
        let deepest = candidates
            .iter()
            .filter(|(_, depth, _)| *depth == max_depth)
            .collect::<Vec<_>>();
        if deepest.len() == 1 {
            let candidate = &deepest[0].0;
            return ForegroundSelection {
                executable: Some(candidate.executable.clone()),
                recognized_tool: Some(candidate.tool.clone()),
                candidates: public_candidates,
                ambiguous: false,
                method: ForegroundMethod::DeepestRecognizedDescendant,
            };
        }
        if deepest.len() > 1 {
            return ForegroundSelection {
                executable: None,
                recognized_tool: None,
                candidates: public_candidates,
                ambiguous: true,
                method: ForegroundMethod::Unresolved,
            };
        }
    }

    let executable = sanitize_executable(tmux_current_executable);
    let recognized_tool = registry
        .lookup(&executable)
        .map(|(tool, _)| tool.to_string());
    ForegroundSelection {
        executable: (!executable.is_empty()).then_some(executable),
        recognized_tool,
        candidates: public_candidates,
        ambiguous: false,
        method: if tmux_current_executable.is_empty() {
            ForegroundMethod::Unresolved
        } else {
            ForegroundMethod::TmuxCurrentExecutable
        },
    }
}

fn executable_basename(value: &OsStr) -> Option<String> {
    let basename = Path::new(value).file_name().unwrap_or(value);
    let value = basename.to_string_lossy();
    let sanitized = sanitize_executable(&value);
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitize_executable(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_EXECUTABLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Strict subprocess limits. `max_output_bytes` is shared across stdout and stderr.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandLimits {
    pub(crate) timeout: Duration,
    pub(crate) max_output_bytes: usize,
    pub(crate) max_args: usize,
    pub(crate) max_arg_bytes: usize,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(150),
            max_output_bytes: 64 * 1_024,
            max_args: 32,
            max_arg_bytes: 4 * 1_024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
}

pub(crate) trait CommandRunner {
    fn run(
        &self,
        program: &OsStr,
        args: &[OsString],
        limits: CommandLimits,
    ) -> Result<CommandOutput, CommandRunError>;
}

#[derive(Debug)]
pub(crate) enum CommandRunError {
    InvalidInvocation,
    Spawn(io::Error),
    Wait(io::Error),
    Timeout,
    OutputLimitExceeded,
    PipeUnavailable,
}

impl CommandRunError {
    fn category(&self) -> &'static str {
        match self {
            Self::InvalidInvocation => "invalid_invocation",
            Self::Spawn(error) if error.kind() == io::ErrorKind::NotFound => "not_found",
            Self::Spawn(_) => "spawn_failed",
            Self::Wait(_) => "wait_failed",
            Self::Timeout => "timeout",
            Self::OutputLimitExceeded => "output_limit",
            Self::PipeUnavailable => "pipe_unavailable",
        }
    }
}

impl fmt::Display for CommandRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.category())
    }
}

impl std::error::Error for CommandRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Wait(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BoundedCommandRunner;

impl CommandRunner for BoundedCommandRunner {
    fn run(
        &self,
        program: &OsStr,
        args: &[OsString],
        limits: CommandLimits,
    ) -> Result<CommandOutput, CommandRunError> {
        if program.is_empty()
            || limits.timeout.is_zero()
            || args.len() > limits.max_args
            || os_bytes(program).len() > limits.max_arg_bytes
            || args
                .iter()
                .any(|argument| os_bytes(argument.as_os_str()).len() > limits.max_arg_bytes)
        {
            return Err(CommandRunError::InvalidInvocation);
        }

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CommandRunError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(CommandRunError::PipeUnavailable)?;
        let stderr = child
            .stderr
            .take()
            .ok_or(CommandRunError::PipeUnavailable)?;
        let used = Arc::new(AtomicUsize::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let stdout_rx = spawn_bounded_reader(
            stdout,
            limits.max_output_bytes,
            Arc::clone(&used),
            Arc::clone(&overflow),
        );
        let stderr_rx = spawn_bounded_reader(
            stderr,
            limits.max_output_bytes,
            Arc::clone(&used),
            Arc::clone(&overflow),
        );
        let started = Instant::now();
        let deadline = started
            .checked_add(limits.timeout)
            .ok_or(CommandRunError::InvalidInvocation)?;

        let status = loop {
            if overflow.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandRunError::OutputLimitExceeded);
            }
            if let Some(status) = child.try_wait().map_err(CommandRunError::Wait)? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandRunError::Timeout);
            }
            thread::sleep(Duration::from_millis(2));
        };

        let stdout = receive_pipe(stdout_rx, deadline)?;
        let _stderr = receive_pipe(stderr_rx, deadline)?;
        if overflow.load(Ordering::Acquire) {
            return Err(CommandRunError::OutputLimitExceeded);
        }
        Ok(CommandOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout,
        })
    }
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    maximum: usize,
    used: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
) -> mpsc::Receiver<Result<Vec<u8>, io::Error>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4_096];
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let previous = used
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some(value.saturating_add(count))
                })
                .unwrap_or(usize::MAX);
            let keep = maximum.saturating_sub(previous).min(count);
            output.extend_from_slice(&buffer[..keep]);
            if keep < count {
                overflow.store(true, Ordering::Release);
            }
        }
        let _ = sender.send(Ok(output));
    });
    receiver
}

fn receive_pipe(
    receiver: mpsc::Receiver<Result<Vec<u8>, io::Error>>,
    deadline: Instant,
) -> Result<Vec<u8>, CommandRunError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(CommandRunError::Timeout);
    }
    receiver
        .recv_timeout(remaining)
        .map_err(|_| CommandRunError::Timeout)?
        .map_err(CommandRunError::Wait)
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or("").as_bytes()
}

pub(crate) trait Clock: Clone {
    fn unix_millis(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn unix_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TmuxExtractorConfig {
    pub(crate) tmux_executable: PathBuf,
    /// Empty means the current user's default tmux socket. Overrides are queried with `-S`.
    pub(crate) socket_overrides: Vec<PathBuf>,
    pub(crate) command_limits: CommandLimits,
    pub(crate) recent_client_age: Duration,
    pub(crate) max_clients: usize,
}

impl Default for TmuxExtractorConfig {
    fn default() -> Self {
        Self {
            tmux_executable: PathBuf::from("tmux"),
            socket_overrides: Vec::new(),
            command_limits: CommandLimits::default(),
            recent_client_age: Duration::from_secs(5),
            max_clients: 64,
        }
    }
}

pub(crate) struct TmuxExtractor<R, C> {
    runner: R,
    clock: C,
    config: TmuxExtractorConfig,
    tools: ToolRegistry,
    projects: ProjectResolver<C>,
}

impl<R: CommandRunner, C: Clock> TmuxExtractor<R, C> {
    pub(crate) fn new(
        runner: R,
        clock: C,
        config: TmuxExtractorConfig,
        tools: ToolRegistry,
        project_config: ProjectResolverConfig,
    ) -> Self {
        Self {
            runner,
            clock: clock.clone(),
            config,
            tools,
            projects: ProjectResolver::new(project_config, clock),
        }
    }

    fn server_targets(&self) -> Vec<ServerTarget> {
        if self.config.socket_overrides.is_empty() {
            vec![ServerTarget::Default]
        } else {
            self.config
                .socket_overrides
                .iter()
                .cloned()
                .map(ServerTarget::Socket)
                .collect()
        }
    }

    fn command_args(
        &self,
        server: &ServerTarget,
        command: &str,
        trailing: impl IntoIterator<Item = OsString>,
    ) -> Vec<OsString> {
        let mut args = Vec::new();
        if let ServerTarget::Socket(socket) = server {
            args.push(OsString::from("-S"));
            args.push(socket.as_os_str().to_owned());
        }
        args.push(OsString::from(command));
        args.extend(trailing);
        args
    }

    fn query_clients(&self) -> Result<Vec<TmuxClient>, QueryIssue> {
        let mut clients = Vec::new();
        let mut successful_server = false;
        let mut most_severe_issue = QueryIssue::Missing;
        for server in self.server_targets() {
            if !server.is_current_user_eligible() {
                continue;
            }
            let args = self.command_args(
                &server,
                "list-clients",
                [OsString::from("-F"), OsString::from(CLIENT_FORMAT)],
            );
            match self.runner.run(
                self.config.tmux_executable.as_os_str(),
                &args,
                self.config.command_limits,
            ) {
                Ok(output) if output.success => {
                    successful_server = true;
                    let parsed = parse_clients(&output.stdout, &server, self.config.max_clients)?;
                    clients.extend(parsed);
                }
                Ok(_) => {}
                Err(CommandRunError::Spawn(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Err(CommandRunError::Timeout) => {
                    most_severe_issue = most_severe_issue.combine(QueryIssue::Timeout)
                }
                Err(CommandRunError::OutputLimitExceeded) => {
                    most_severe_issue = most_severe_issue.combine(QueryIssue::OutputLimit)
                }
                Err(_) => most_severe_issue = most_severe_issue.combine(QueryIssue::Malformed),
            }
        }
        if !successful_server {
            return Err(most_severe_issue);
        }
        let mut seen = HashSet::new();
        clients.retain(|client| seen.insert(client.pid));
        Ok(clients)
    }

    fn query_pane(&self, client: &TmuxClient) -> Result<CurrentPane, QueryIssue> {
        let args = self.command_args(
            &client.server,
            "display-message",
            [
                OsString::from("-p"),
                OsString::from("-t"),
                OsString::from(&client.session),
                OsString::from("-F"),
                OsString::from(PANE_FORMAT),
            ],
        );
        match self.runner.run(
            self.config.tmux_executable.as_os_str(),
            &args,
            self.config.command_limits,
        ) {
            Ok(output) if output.success => parse_pane(&output.stdout),
            Ok(_) => Err(QueryIssue::Missing),
            Err(CommandRunError::Timeout) => Err(QueryIssue::Timeout),
            Err(CommandRunError::OutputLimitExceeded) => Err(QueryIssue::OutputLimit),
            Err(_) => Err(QueryIssue::Malformed),
        }
    }
}

impl<R: CommandRunner, C: Clock> TerminalContextExtractor for TmuxExtractor<R, C> {
    fn supports(&self, focused: &FocusedWindow) -> bool {
        focused.app_id.to_ascii_lowercase().contains("ghostty")
    }

    fn extract(
        &mut self,
        focused: &FocusedWindow,
        processes: &ProcessSnapshot,
    ) -> Result<TerminalExtraction, TerminalExtractError> {
        if !self.supports(focused) {
            return Ok(TerminalExtraction::NotApplicable(
                NotApplicableReason::UnsupportedApplication,
            ));
        }
        let Some(focused_pid) = focused.pid else {
            return Ok(TerminalExtraction::NotApplicable(
                NotApplicableReason::MissingFocusedPid,
            ));
        };
        let descendant_pids = processes.tmux_descendant_pids(focused_pid);
        if descendant_pids.is_empty() {
            return Ok(TerminalExtraction::NotApplicable(
                NotApplicableReason::NoTmuxDescendant,
            ));
        }
        let clients = match self.query_clients() {
            Ok(clients) => clients,
            Err(QueryIssue::Missing) => {
                return Ok(TerminalExtraction::NotApplicable(
                    NotApplicableReason::MissingTmuxServer,
                ))
            }
            Err(issue) => return Ok(TerminalExtraction::Unresolved(issue.ambiguity(0))),
        };
        let eligible = clients
            .into_iter()
            .filter(|client| descendant_pids.contains(&client.pid))
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return Ok(TerminalExtraction::NotApplicable(
                NotApplicableReason::NoAttachedClient,
            ));
        }
        let candidate_count = eligible.len();
        let (client, match_method, confidence) = if eligible.len() == 1 {
            (
                eligible
                    .into_iter()
                    .next()
                    .ok_or_else(TerminalExtractError::invariant)?,
                TerminalMatchMethod::ExactDescendantClient,
                1.0,
            )
        } else {
            let now_seconds = self.clock.unix_millis() / 1_000;
            let recent = eligible
                .iter()
                .filter(|client| {
                    client.activity_seconds > 0
                        && client.activity_seconds <= now_seconds.saturating_add(5)
                        && now_seconds.saturating_sub(client.activity_seconds)
                            <= self.config.recent_client_age.as_secs().min(i64::MAX as u64) as i64
                })
                .cloned()
                .collect::<Vec<_>>();
            if recent.len() != 1 {
                let reason = if recent.is_empty() {
                    AmbiguityReason::StaleClients
                } else {
                    AmbiguityReason::MultipleEligibleClients
                };
                return Ok(TerminalExtraction::Unresolved(TerminalAmbiguity {
                    reason,
                    candidate_count,
                }));
            }
            (
                recent
                    .into_iter()
                    .next()
                    .ok_or_else(TerminalExtractError::invariant)?,
                TerminalMatchMethod::UniqueRecentDescendantClient,
                0.8,
            )
        };

        let pane = match self.query_pane(&client) {
            Ok(pane) if pane.session == client.session => pane,
            Ok(_) => {
                return Ok(TerminalExtraction::Unresolved(TerminalAmbiguity {
                    reason: AmbiguityReason::MalformedTmuxOutput,
                    candidate_count,
                }))
            }
            Err(issue) => {
                return Ok(TerminalExtraction::Unresolved(
                    issue.ambiguity(candidate_count),
                ))
            }
        };
        let foreground = select_foreground(
            pane.pane_pid,
            &pane.current_executable,
            processes,
            &self.tools,
        );
        let project = self.projects.resolve(&pane.cwd);
        Ok(TerminalExtraction::Resolved(TerminalContext {
            terminal_kind: TerminalKind::Tmux,
            match_method,
            match_confidence: confidence,
            candidate_count,
            tmux_client_pid: client.pid,
            tmux_session: pane.session,
            tmux_window: pane.window,
            tmux_window_index: pane.window_index,
            tmux_pane: pane.pane,
            tmux_pane_index: pane.pane_index,
            tmux_pane_pid: pane.pane_pid,
            cwd: project.cwd.clone(),
            foreground,
            project,
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ServerTarget {
    Default,
    Socket(PathBuf),
}

impl ServerTarget {
    fn is_current_user_eligible(&self) -> bool {
        match self {
            Self::Default => true,
            Self::Socket(path) => is_current_user_socket(path),
        }
    }
}

#[cfg(unix)]
fn is_current_user_socket(path: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let Some(uid) = current_uid() else {
        return false;
    };
    fs::metadata(path)
        .map(|metadata| metadata.file_type().is_socket() && metadata.uid() == uid)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_current_user_socket(_path: &Path) -> bool {
    false
}

fn current_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("Uid:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[derive(Clone, Debug)]
struct TmuxClient {
    pid: i32,
    session: String,
    activity_seconds: i64,
    server: ServerTarget,
}

#[derive(Debug)]
struct CurrentPane {
    session: String,
    window: String,
    window_index: u32,
    pane: String,
    pane_index: u32,
    pane_pid: i32,
    cwd: PathBuf,
    current_executable: String,
}

#[derive(Clone, Copy, Debug)]
enum QueryIssue {
    Missing,
    Timeout,
    OutputLimit,
    Malformed,
}

impl QueryIssue {
    fn combine(self, other: Self) -> Self {
        fn rank(issue: QueryIssue) -> u8 {
            match issue {
                QueryIssue::Missing => 0,
                QueryIssue::Malformed => 1,
                QueryIssue::Timeout => 2,
                QueryIssue::OutputLimit => 3,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }

    fn ambiguity(self, candidate_count: usize) -> TerminalAmbiguity {
        TerminalAmbiguity {
            reason: match self {
                Self::Missing => AmbiguityReason::PaneUnavailable,
                Self::Timeout => AmbiguityReason::TmuxTimeout,
                Self::OutputLimit => AmbiguityReason::TmuxOutputLimit,
                Self::Malformed => AmbiguityReason::MalformedTmuxOutput,
            },
            candidate_count,
        }
    }
}

fn parse_clients(
    bytes: &[u8],
    server: &ServerTarget,
    maximum: usize,
) -> Result<Vec<TmuxClient>, QueryIssue> {
    let text = std::str::from_utf8(bytes).map_err(|_| QueryIssue::Malformed)?;
    let mut clients = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if clients.len() >= maximum {
            return Err(QueryIssue::OutputLimit);
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(QueryIssue::Malformed);
        }
        let pid = fields[0]
            .parse::<i32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or(QueryIssue::Malformed)?;
        let session = sanitize_tmux_name(fields[1]).ok_or(QueryIssue::Malformed)?;
        let activity_seconds = fields[2]
            .parse::<i64>()
            .ok()
            .filter(|activity| *activity >= 0)
            .ok_or(QueryIssue::Malformed)?;
        clients.push(TmuxClient {
            pid,
            session,
            activity_seconds,
            server: server.clone(),
        });
    }
    Ok(clients)
}

fn parse_pane(bytes: &[u8]) -> Result<CurrentPane, QueryIssue> {
    let text = std::str::from_utf8(bytes).map_err(|_| QueryIssue::Malformed)?;
    let line = text.trim_end_matches(['\r', '\n']);
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != 8 {
        return Err(QueryIssue::Malformed);
    }
    let session = sanitize_tmux_name(fields[0]).ok_or(QueryIssue::Malformed)?;
    let window = sanitize_tmux_name(fields[1]).ok_or(QueryIssue::Malformed)?;
    let window_index = parse_u32(fields[2])?;
    let pane = fields[3];
    if !pane
        .strip_prefix('%')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(QueryIssue::Malformed);
    }
    let pane_index = parse_u32(fields[4])?;
    let pane_pid = fields[5]
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or(QueryIssue::Malformed)?;
    if fields[6].is_empty() || fields[6].chars().any(char::is_control) {
        return Err(QueryIssue::Malformed);
    }
    let cwd = PathBuf::from(fields[6]);
    let current_executable = sanitize_executable(fields[7]);
    Ok(CurrentPane {
        session,
        window,
        window_index,
        pane: pane.to_string(),
        pane_index,
        pane_pid,
        cwd,
        current_executable,
    })
}

fn parse_u32(value: &str) -> Result<u32, QueryIssue> {
    value.parse::<u32>().map_err(|_| QueryIssue::Malformed)
}

fn sanitize_tmux_name(value: &str) -> Option<String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let value = value
        .chars()
        .take(MAX_TMUX_NAME_CHARS + 1)
        .collect::<String>();
    (value.chars().count() <= MAX_TMUX_NAME_CHARS).then_some(value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathPrivacyMode {
    Full,
    Basename,
    Hash,
}

/// A typed path representation that cannot accidentally expose a full path in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PrivatePath {
    mode: PathPrivacyMode,
    display: Option<String>,
    stable_hash: [u8; 32],
}

impl PrivatePath {
    fn from_canonical(path: &Path, mode: PathPrivacyMode) -> Self {
        let basename = path
            .file_name()
            .map(|name| sanitize_component(&name.to_string_lossy()))
            .unwrap_or_default();
        Self::from_canonical_named(path, mode, &basename)
    }

    fn from_canonical_named(path: &Path, mode: PathPrivacyMode, basename: &str) -> Self {
        let stable_hash = typed_hash(b"path", b"canonical", &path_bytes(path));
        let display = match mode {
            PathPrivacyMode::Full => Some(sanitize_path_display(path)),
            PathPrivacyMode::Basename => {
                let basename = sanitize_component(basename);
                (!basename.is_empty()).then_some(basename)
            }
            PathPrivacyMode::Hash => None,
        };
        Self {
            mode,
            display,
            stable_hash,
        }
    }

    pub(crate) fn display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    pub(crate) fn stable_hash(&self) -> &[u8; 32] {
        &self.stable_hash
    }
}

impl fmt::Debug for PrivatePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivatePath")
            .field("mode", &self.mode)
            .field("display", &self.display.as_ref().map(|_| "<redacted>"))
            .field("stable_hash_prefix", &&self.stable_hash[..4])
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EntityKind {
    Repository,
    Project,
}

impl EntityKind {
    fn key_bytes(self) -> &'static [u8] {
        match self {
            Self::Repository => b"repository",
            Self::Project => b"project",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntityIdentity {
    pub(crate) kind: EntityKind,
    pub(crate) canonical_key: [u8; 32],
    pub(crate) display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectResolution {
    pub(crate) status: ProjectResolutionStatus,
    pub(crate) cwd: PrivatePath,
    pub(crate) worktree_root: Option<PrivatePath>,
    pub(crate) repository_location: Option<PrivatePath>,
    pub(crate) repository: Option<EntityIdentity>,
    pub(crate) project: Option<EntityIdentity>,
    pub(crate) confidence_millis: u16,
    pub(crate) cache_hit: bool,
}

impl ProjectResolution {
    #[cfg(test)]
    pub(crate) fn confidence(&self) -> f64 {
        f64::from(self.confidence_millis) / 1_000.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectResolutionStatus {
    Git,
    WorkspaceRoot,
    Marker,
    Unresolved(ProjectUnresolvedReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectUnresolvedReason {
    CwdUnavailable,
    PathTooLong,
    GitMetadataTooLarge,
    MalformedGitMetadata,
    OutsideConfiguredWorkspace,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectResolverConfig {
    pub(crate) privacy_mode: PathPrivacyMode,
    pub(crate) workspace_roots: Vec<PathBuf>,
    pub(crate) marker_files: Vec<OsString>,
    pub(crate) max_ancestors: usize,
    pub(crate) max_path_bytes: usize,
    pub(crate) cache_ttl: Duration,
    pub(crate) max_cache_entries: usize,
}

impl Default for ProjectResolverConfig {
    fn default() -> Self {
        Self {
            privacy_mode: PathPrivacyMode::Basename,
            workspace_roots: Vec::new(),
            marker_files: Vec::new(),
            max_ancestors: 64,
            max_path_bytes: 4 * 1_024,
            cache_ttl: Duration::from_secs(300),
            max_cache_entries: 1_024,
        }
    }
}

pub(crate) struct ProjectResolver<C> {
    config: ProjectResolverConfig,
    clock: C,
    cache: HashMap<PathBuf, CacheEntry>,
}

#[derive(Clone)]
struct CacheEntry {
    expires_at_ms: i64,
    inserted_at_ms: i64,
    watches: Vec<(PathBuf, Option<MetadataFingerprint>)>,
    resolution: ProjectResolution,
}

impl<C: Clock> ProjectResolver<C> {
    pub(crate) fn new(config: ProjectResolverConfig, clock: C) -> Self {
        Self {
            config,
            clock,
            cache: HashMap::new(),
        }
    }

    pub(crate) fn resolve(&mut self, cwd: &Path) -> ProjectResolution {
        if path_bytes(cwd).len() > self.config.max_path_bytes {
            return self.unresolved(cwd, ProjectUnresolvedReason::PathTooLong);
        }
        let canonical = match fs::canonicalize(cwd) {
            Ok(path) if path_bytes(&path).len() <= self.config.max_path_bytes => path,
            Ok(_) => return self.unresolved(cwd, ProjectUnresolvedReason::PathTooLong),
            Err(_) => return self.unresolved(cwd, ProjectUnresolvedReason::CwdUnavailable),
        };
        let now = self.clock.unix_millis();
        if let Some(entry) = self.cache.get(&canonical) {
            if now <= entry.expires_at_ms && watches_unchanged(&entry.watches) {
                let mut resolution = entry.resolution.clone();
                resolution.cache_hit = true;
                return resolution;
            }
        }
        self.cache.remove(&canonical);

        let (mut resolution, mut watches) = match self.resolve_uncached(&canonical) {
            Ok(value) => value,
            Err(reason) => (self.unresolved(&canonical, reason), Vec::new()),
        };
        watches.extend(self.discovery_watches(&canonical));
        watches.sort_by(|left, right| left.0.cmp(&right.0));
        watches.dedup_by(|left, right| left.0 == right.0);
        resolution.cache_hit = false;
        self.insert_cache(canonical, resolution.clone(), watches, now);
        resolution
    }

    fn discovery_watches(
        &self,
        canonical_cwd: &Path,
    ) -> Vec<(PathBuf, Option<MetadataFingerprint>)> {
        let mut watches = Vec::new();
        for ancestor in canonical_cwd
            .ancestors()
            .take(self.config.max_ancestors.max(1))
        {
            let git_marker = ancestor.join(".git");
            watches.push((git_marker.clone(), metadata_fingerprint(&git_marker)));
            for marker_name in &self.config.marker_files {
                if !marker_name.is_empty() && Path::new(marker_name).components().count() == 1 {
                    let marker = ancestor.join(marker_name);
                    watches.push((marker.clone(), metadata_fingerprint(&marker)));
                }
            }
        }
        for root in &self.config.workspace_roots {
            let Ok(root) = fs::canonicalize(root) else {
                continue;
            };
            let Ok(relative) = canonical_cwd.strip_prefix(&root) else {
                continue;
            };
            if let Some(first) = relative.components().next() {
                let child = root.join(first.as_os_str());
                watches.push((child.clone(), metadata_fingerprint(&child)));
            }
        }
        watches
    }

    fn resolve_uncached(
        &self,
        canonical_cwd: &Path,
    ) -> Result<
        (
            ProjectResolution,
            Vec<(PathBuf, Option<MetadataFingerprint>)>,
        ),
        ProjectUnresolvedReason,
    > {
        for ancestor in canonical_cwd
            .ancestors()
            .take(self.config.max_ancestors.max(1))
        {
            let marker = ancestor.join(".git");
            let metadata = match fs::symlink_metadata(&marker) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            let mut watches = vec![(
                marker.clone(),
                Some(MetadataFingerprint::from_metadata(&metadata)),
            )];
            let git_dir = if metadata.is_dir() {
                fs::canonicalize(&marker)
                    .map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)?
            } else if metadata.is_file() {
                let value = read_small_text(&marker, MAX_GIT_FILE_BYTES)?;
                let target = value
                    .trim()
                    .strip_prefix("gitdir:")
                    .map(str::trim)
                    .filter(|target| !target.is_empty() && !target.chars().any(char::is_control))
                    .ok_or(ProjectUnresolvedReason::MalformedGitMetadata)?;
                let target = PathBuf::from(target);
                let target = if target.is_absolute() {
                    target
                } else {
                    ancestor.join(target)
                };
                fs::canonicalize(target)
                    .map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)?
            } else {
                continue;
            };
            if path_bytes(&git_dir).len() > self.config.max_path_bytes {
                return Err(ProjectUnresolvedReason::PathTooLong);
            }
            if let Ok(metadata) = fs::symlink_metadata(&git_dir) {
                watches.push((
                    git_dir.clone(),
                    Some(MetadataFingerprint::from_metadata(&metadata)),
                ));
            }
            let commondir_file = git_dir.join("commondir");
            let common_dir = match fs::symlink_metadata(&commondir_file) {
                Ok(metadata) if metadata.is_file() => {
                    watches.push((
                        commondir_file.clone(),
                        Some(MetadataFingerprint::from_metadata(&metadata)),
                    ));
                    let value = read_small_text(&commondir_file, MAX_GIT_FILE_BYTES)?;
                    let value = value.trim();
                    if value.is_empty() || value.chars().any(char::is_control) {
                        return Err(ProjectUnresolvedReason::MalformedGitMetadata);
                    }
                    let value = PathBuf::from(value);
                    let value = if value.is_absolute() {
                        value
                    } else {
                        git_dir.join(value)
                    };
                    fs::canonicalize(value)
                        .map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)?
                }
                Ok(_) => return Err(ProjectUnresolvedReason::MalformedGitMetadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    watches.push((commondir_file.clone(), None));
                    git_dir.clone()
                }
                Err(_) => return Err(ProjectUnresolvedReason::MalformedGitMetadata),
            };
            if path_bytes(&common_dir).len() > self.config.max_path_bytes {
                return Err(ProjectUnresolvedReason::PathTooLong);
            }
            if common_dir != git_dir {
                if let Ok(metadata) = fs::symlink_metadata(&common_dir) {
                    watches.push((
                        common_dir.clone(),
                        Some(MetadataFingerprint::from_metadata(&metadata)),
                    ));
                }
            }
            let repository_root = if common_dir.file_name() == Some(OsStr::new(".git")) {
                common_dir.parent().unwrap_or(&common_dir)
            } else {
                &common_dir
            };
            let display_name = path_display_name(repository_root, "repository");
            let repository_key = typed_hash(
                EntityKind::Repository.key_bytes(),
                b"git-common-dir",
                &path_bytes(&common_dir),
            );
            let repository = EntityIdentity {
                kind: EntityKind::Repository,
                canonical_key: repository_key,
                display_name: display_name.clone(),
            };
            let project = EntityIdentity {
                kind: EntityKind::Project,
                canonical_key: typed_hash(
                    EntityKind::Project.key_bytes(),
                    b"repository-key",
                    &repository_key,
                ),
                display_name,
            };
            return Ok((
                ProjectResolution {
                    status: ProjectResolutionStatus::Git,
                    cwd: PrivatePath::from_canonical_named(
                        canonical_cwd,
                        self.config.privacy_mode,
                        &repository.display_name,
                    ),
                    worktree_root: Some(PrivatePath::from_canonical(
                        ancestor,
                        self.config.privacy_mode,
                    )),
                    repository_location: Some(PrivatePath::from_canonical(
                        repository_root,
                        self.config.privacy_mode,
                    )),
                    repository: Some(repository),
                    project: Some(project),
                    confidence_millis: 1_000,
                    cache_hit: false,
                },
                watches,
            ));
        }

        for root in &self.config.workspace_roots {
            let Ok(root) = fs::canonicalize(root) else {
                continue;
            };
            if path_bytes(&root).len() > self.config.max_path_bytes {
                continue;
            }
            let Ok(relative) = canonical_cwd.strip_prefix(&root) else {
                continue;
            };
            let Some(first) = relative.components().next() else {
                continue;
            };
            let project_root = root.join(first.as_os_str());
            let display_name = path_display_name(&project_root, "workspace");
            let project = EntityIdentity {
                kind: EntityKind::Project,
                canonical_key: typed_hash(
                    EntityKind::Project.key_bytes(),
                    b"workspace-root-child",
                    &path_bytes(&project_root),
                ),
                display_name,
            };
            let watches = vec![(project_root.clone(), metadata_fingerprint(&project_root))];
            return Ok((
                ProjectResolution {
                    status: ProjectResolutionStatus::WorkspaceRoot,
                    cwd: PrivatePath::from_canonical_named(
                        canonical_cwd,
                        self.config.privacy_mode,
                        &project.display_name,
                    ),
                    worktree_root: Some(PrivatePath::from_canonical(
                        &project_root,
                        self.config.privacy_mode,
                    )),
                    repository_location: None,
                    repository: None,
                    project: Some(project),
                    confidence_millis: 700,
                    cache_hit: false,
                },
                watches,
            ));
        }

        for ancestor in canonical_cwd
            .ancestors()
            .take(self.config.max_ancestors.max(1))
        {
            for marker_name in &self.config.marker_files {
                if marker_name.is_empty() || Path::new(marker_name).components().count() != 1 {
                    continue;
                }
                let marker = ancestor.join(marker_name);
                let Ok(metadata) = fs::symlink_metadata(&marker) else {
                    continue;
                };
                let display_name = path_display_name(ancestor, "project");
                let project = EntityIdentity {
                    kind: EntityKind::Project,
                    canonical_key: typed_hash(
                        EntityKind::Project.key_bytes(),
                        b"marker-root",
                        &path_bytes(ancestor),
                    ),
                    display_name,
                };
                return Ok((
                    ProjectResolution {
                        status: ProjectResolutionStatus::Marker,
                        cwd: PrivatePath::from_canonical_named(
                            canonical_cwd,
                            self.config.privacy_mode,
                            &project.display_name,
                        ),
                        worktree_root: Some(PrivatePath::from_canonical(
                            ancestor,
                            self.config.privacy_mode,
                        )),
                        repository_location: None,
                        repository: None,
                        project: Some(project),
                        confidence_millis: 600,
                        cache_hit: false,
                    },
                    vec![(marker, Some(MetadataFingerprint::from_metadata(&metadata)))],
                ));
            }
        }

        Err(ProjectUnresolvedReason::OutsideConfiguredWorkspace)
    }

    fn unresolved(&self, cwd: &Path, reason: ProjectUnresolvedReason) -> ProjectResolution {
        let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        ProjectResolution {
            status: ProjectResolutionStatus::Unresolved(reason),
            cwd: PrivatePath::from_canonical(&canonical, self.config.privacy_mode),
            worktree_root: None,
            repository_location: None,
            repository: None,
            project: None,
            confidence_millis: 0,
            cache_hit: false,
        }
    }

    fn insert_cache(
        &mut self,
        path: PathBuf,
        resolution: ProjectResolution,
        watches: Vec<(PathBuf, Option<MetadataFingerprint>)>,
        now: i64,
    ) {
        if self.config.max_cache_entries == 0 {
            return;
        }
        while self.cache.len() >= self.config.max_cache_entries {
            let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at_ms)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.cache.remove(&oldest);
        }
        let ttl = self.config.cache_ttl.as_millis().min(i64::MAX as u128) as i64;
        self.cache.insert(
            path,
            CacheEntry {
                expires_at_ms: now.saturating_add(ttl),
                inserted_at_ms: now,
                watches,
                resolution,
            },
        );
    }
}

fn read_small_text(path: &Path, maximum: usize) -> Result<String, ProjectUnresolvedReason> {
    let mut file = File::open(path).map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)?;
    if bytes.len() > maximum {
        return Err(ProjectUnresolvedReason::GitMetadataTooLarge);
    }
    String::from_utf8(bytes).map_err(|_| ProjectUnresolvedReason::MalformedGitMetadata)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataFingerprint {
    is_file: bool,
    is_dir: bool,
    length: u64,
    modified_nanos: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl MetadataFingerprint {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            length: metadata.len(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_nanos()),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        }
    }
}

fn metadata_fingerprint(path: &Path) -> Option<MetadataFingerprint> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| MetadataFingerprint::from_metadata(&metadata))
}

fn watches_unchanged(watches: &[(PathBuf, Option<MetadataFingerprint>)]) -> bool {
    !watches.is_empty()
        && watches
            .iter()
            .all(|(path, expected)| metadata_fingerprint(path) == *expected)
}

fn typed_hash(kind: &[u8], source: &[u8], value: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(kind);
    digest.update([0]);
    digest.update(source);
    digest.update([0]);
    digest.update(value);
    digest.finalize().into()
}

fn path_display_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .map(|name| sanitize_component(&name.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_path_display(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .filter(|character| !character.is_control())
        .take(4_096)
        .collect()
}

fn path_bytes(path: &Path) -> Vec<u8> {
    os_bytes(path.as_os_str()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{atomic::AtomicI64, Mutex},
    };

    #[derive(Clone)]
    struct FakeClock(Arc<AtomicI64>);

    impl FakeClock {
        fn new(millis: i64) -> Self {
            Self(Arc::new(AtomicI64::new(millis)))
        }

        fn set(&self, millis: i64) {
            self.0.store(millis, Ordering::Relaxed);
        }
    }

    impl Clock for FakeClock {
        fn unix_millis(&self) -> i64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    enum FakeResponse {
        Output(CommandOutput),
        Timeout,
        OutputLimit,
    }

    #[derive(Default)]
    struct FakeRunner {
        responses: Mutex<VecDeque<FakeResponse>>,
        calls: Mutex<Vec<Vec<OsString>>>,
    }

    impl FakeRunner {
        fn with(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn success(text: &str) -> FakeResponse {
            FakeResponse::Output(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: text.as_bytes().to_vec(),
            })
        }

        fn failure() -> FakeResponse {
            FakeResponse::Output(CommandOutput {
                success: false,
                exit_code: Some(1),
                stdout: Vec::new(),
            })
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _program: &OsStr,
            args: &[OsString],
            _limits: CommandLimits,
        ) -> Result<CommandOutput, CommandRunError> {
            self.calls.lock().unwrap().push(args.to_vec());
            match self.responses.lock().unwrap().pop_front().unwrap() {
                FakeResponse::Output(output) => Ok(output),
                FakeResponse::Timeout => Err(CommandRunError::Timeout),
                FakeResponse::OutputLimit => Err(CommandRunError::OutputLimitExceeded),
            }
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "bms-watchdog-terminal-{name}-{}-{suffix}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn git_project(root: &Path, name: &str) -> PathBuf {
        let project = root.join(name);
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::create_dir_all(project.join("src")).unwrap();
        project
    }

    fn snapshot_with_clients(client_pids: &[i32], pane_pid: i32) -> ProcessSnapshot {
        let mut processes = vec![ProcessInfo::new(100, 1, "ghostty")];
        for pid in client_pids {
            processes.push(ProcessInfo::new(*pid, 100, "/usr/bin/tmux"));
        }
        processes.push(
            ProcessInfo::new(pane_pid, 1, "bash")
                .with_process_groups(Some(pane_pid), Some(pane_pid + 1)),
        );
        processes.push(
            ProcessInfo::new(pane_pid + 1, pane_pid, "/usr/local/bin/pi")
                .with_process_groups(Some(pane_pid + 1), Some(pane_pid + 1)),
        );
        ProcessSnapshot::new(processes)
    }

    fn extractor(
        runner: FakeRunner,
        clock: FakeClock,
        workspace_roots: Vec<PathBuf>,
    ) -> TmuxExtractor<FakeRunner, FakeClock> {
        TmuxExtractor::new(
            runner,
            clock,
            TmuxExtractorConfig::default(),
            ToolRegistry::common_defaults(),
            ProjectResolverConfig {
                workspace_roots,
                ..ProjectResolverConfig::default()
            },
        )
    }

    #[test]
    fn tmux_format_parsing_is_strict_and_bounded() {
        let server = ServerTarget::Default;
        let clients = parse_clients(b"4327\tbms\t1000\n", &server, 2).unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].pid, 4327);
        assert_eq!(clients[0].session, "bms");
        assert!(parse_clients(b"1\tbad\n", &server, 2).is_err());
        assert!(parse_clients(b"1\ta\t1\n2\tb\t1\n", &server, 1).is_err());
        assert!(parse_clients(b"1\tbad\nname\t1\n", &server, 2).is_err());

        let pane = parse_pane(b"bms\twatchdog\t2\t%9\t0\t4920\t/tmp/work\tpi\n").unwrap();
        assert_eq!(pane.pane, "%9");
        assert_eq!(pane.current_executable, "pi");
        assert!(parse_pane(b"bms\twatchdog\t2\tnine\t0\t4920\t/tmp\tpi\n").is_err());
        assert!(parse_pane(b"bms\twatchdog\t2\t%9\t0\t4920\t/tmp\tpi\textra\n").is_err());
    }

    #[test]
    fn one_descendant_client_resolves_exact_pane_project_and_foreground_tool() {
        let root = temp_root("exact");
        let project = git_project(&root, "watchdog");
        let cwd = project.join("src");
        let runner = FakeRunner::with([
            FakeRunner::success("110\tbms\t1\n"),
            FakeRunner::success(&format!(
                "bms\twatchdog\t2\t%9\t0\t200\t{}\tpi\n",
                cwd.display()
            )),
        ]);
        let clock = FakeClock::new(10_000);
        let mut extractor = extractor(runner, clock, vec![]);
        let result = extractor
            .extract(
                &FocusedWindow::new("com.mitchellh.ghostty", Some(100)),
                &snapshot_with_clients(&[110], 200),
            )
            .unwrap();
        let TerminalExtraction::Resolved(context) = result else {
            panic!("expected resolved context")
        };
        assert_eq!(
            context.match_method,
            TerminalMatchMethod::ExactDescendantClient
        );
        assert_eq!(context.match_confidence, 1.0);
        assert_eq!(context.tmux_pane, "%9");
        assert_eq!(context.foreground.recognized_tool.as_deref(), Some("pi"));
        assert_eq!(context.project.status, ProjectResolutionStatus::Git);
        assert_eq!(
            context
                .project
                .project
                .as_ref()
                .map(|entity| entity.display_name.as_str()),
            Some("watchdog")
        );
        assert_eq!(context.cwd.display(), Some("watchdog"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn multiple_clients_require_one_recent_candidate() {
        let root = temp_root("recent");
        let project = git_project(&root, "project");
        let runner = FakeRunner::with([
            FakeRunner::success("110\tone\t90\n111\ttwo\t100\n"),
            FakeRunner::success(&format!(
                "two\tproject\t1\t%2\t0\t200\t{}\tbash\n",
                project.display()
            )),
        ]);
        let mut extractor = extractor(runner, FakeClock::new(100_000), vec![]);
        let result = extractor
            .extract(
                &FocusedWindow::new("ghostty", Some(100)),
                &snapshot_with_clients(&[110, 111], 200),
            )
            .unwrap();
        let TerminalExtraction::Resolved(context) = result else {
            panic!("expected recent resolution")
        };
        assert_eq!(
            context.match_method,
            TerminalMatchMethod::UniqueRecentDescendantClient
        );
        assert_eq!(context.match_confidence, 0.8);
        assert_eq!(context.tmux_client_pid, 111);
        assert_eq!(context.candidate_count, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tied_or_stale_multiple_clients_remain_unresolved() {
        for (activities, reason) in [
            (
                "110\tone\t100\n111\ttwo\t99\n",
                AmbiguityReason::MultipleEligibleClients,
            ),
            (
                "110\tone\t80\n111\ttwo\t70\n",
                AmbiguityReason::StaleClients,
            ),
        ] {
            let runner = FakeRunner::with([FakeRunner::success(activities)]);
            let mut extractor = extractor(runner, FakeClock::new(100_000), vec![]);
            let result = extractor
                .extract(
                    &FocusedWindow::new("ghostty", Some(100)),
                    &snapshot_with_clients(&[110, 111], 200),
                )
                .unwrap();
            assert_eq!(
                result,
                TerminalExtraction::Unresolved(TerminalAmbiguity {
                    reason,
                    candidate_count: 2,
                })
            );
        }
    }

    #[test]
    fn missing_server_timeout_and_output_limit_are_typed_nonfatal_outcomes() {
        for (response, expected) in [
            (
                FakeRunner::failure(),
                TerminalExtraction::NotApplicable(NotApplicableReason::MissingTmuxServer),
            ),
            (
                FakeResponse::Timeout,
                TerminalExtraction::Unresolved(TerminalAmbiguity {
                    reason: AmbiguityReason::TmuxTimeout,
                    candidate_count: 0,
                }),
            ),
            (
                FakeResponse::OutputLimit,
                TerminalExtraction::Unresolved(TerminalAmbiguity {
                    reason: AmbiguityReason::TmuxOutputLimit,
                    candidate_count: 0,
                }),
            ),
        ] {
            let runner = FakeRunner::with([response]);
            let mut extractor = extractor(runner, FakeClock::new(1_000), vec![]);
            assert_eq!(
                extractor
                    .extract(
                        &FocusedWindow::new("ghostty", Some(100)),
                        &snapshot_with_clients(&[110], 200),
                    )
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn detached_or_unrelated_clients_are_not_focused_work() {
        let runner = FakeRunner::with([FakeRunner::success("999\tbackground\t100\n")]);
        let mut extractor = extractor(runner, FakeClock::new(100_000), vec![]);
        assert_eq!(
            extractor
                .extract(
                    &FocusedWindow::new("ghostty", Some(100)),
                    &snapshot_with_clients(&[110], 200),
                )
                .unwrap(),
            TerminalExtraction::NotApplicable(NotApplicableReason::NoAttachedClient)
        );
    }

    #[test]
    fn foreground_detection_uses_process_groups_and_never_arguments() {
        let snapshot = ProcessSnapshot::new([
            ProcessInfo::new(200, 1, "bash").with_process_groups(Some(200), Some(201)),
            ProcessInfo::new(201, 200, "/opt/bin/pi").with_process_groups(Some(201), Some(201)),
            ProcessInfo::new(202, 200, "nvim").with_process_groups(Some(202), Some(201)),
        ]);
        let selected = select_foreground(200, "bash", &snapshot, &ToolRegistry::common_defaults());
        assert_eq!(selected.executable.as_deref(), Some("pi"));
        assert_eq!(
            selected.method,
            ForegroundMethod::TerminalForegroundProcessGroup
        );
        assert_eq!(snapshot.get(201).unwrap().executable(), "pi");
    }

    #[test]
    fn several_agent_descendants_are_exposed_without_arbitrary_selection() {
        let snapshot = ProcessSnapshot::new([
            ProcessInfo::new(200, 1, "bash"),
            ProcessInfo::new(201, 200, "pi"),
            ProcessInfo::new(202, 200, "claude"),
        ]);
        let selected = select_foreground(200, "bash", &snapshot, &ToolRegistry::common_defaults());
        assert!(selected.ambiguous);
        assert!(selected.executable.is_none());
        assert_eq!(selected.candidates.len(), 2);
    }

    #[test]
    fn ancestry_is_cycle_safe_and_requires_structural_descent() {
        let snapshot = ProcessSnapshot::new([
            ProcessInfo::new(10, 11, "tmux"),
            ProcessInfo::new(11, 10, "helper"),
            ProcessInfo::new(12, 10, "tmux"),
            ProcessInfo::new(20, 1, "ghostty"),
            ProcessInfo::new(21, 20, "tmux"),
        ]);
        assert!(!snapshot.is_descendant_of(12, 20));
        assert!(!snapshot.is_descendant_of(10, 20));
        assert!(snapshot.is_descendant_of(21, 20));
    }

    #[test]
    fn git_common_directory_unifies_main_and_linked_worktrees() {
        let root = temp_root("worktrees");
        let main = git_project(&root, "main-repo");
        let linked = root.join("linked-worktree");
        let linked_git_dir = main.join(".git/worktrees/linked");
        fs::create_dir_all(linked.join("src")).unwrap();
        fs::create_dir_all(&linked_git_dir).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", linked_git_dir.display()),
        )
        .unwrap();
        fs::write(linked_git_dir.join("commondir"), "../..\n").unwrap();
        let clock = FakeClock::new(1_000);
        let mut resolver = ProjectResolver::new(ProjectResolverConfig::default(), clock);
        let main_result = resolver.resolve(&main.join("src"));
        let linked_result = resolver.resolve(&linked.join("src"));
        assert_eq!(main_result.status, ProjectResolutionStatus::Git);
        assert_eq!(linked_result.status, ProjectResolutionStatus::Git);
        assert_eq!(
            main_result.repository.unwrap().canonical_key,
            linked_result.repository.unwrap().canonical_key
        );
        assert_eq!(
            main_result.project.unwrap().canonical_key,
            linked_result.project.unwrap().canonical_key
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_basename_different_repositories_have_different_keys() {
        let root = temp_root("same-name");
        let one = git_project(&root.join("one"), "shared");
        let two = git_project(&root.join("two"), "shared");
        let mut resolver =
            ProjectResolver::new(ProjectResolverConfig::default(), FakeClock::new(1_000));
        let a = resolver.resolve(&one);
        let b = resolver.resolve(&two);
        assert_eq!(a.repository.as_ref().unwrap().display_name, "shared");
        assert_eq!(b.repository.as_ref().unwrap().display_name, "shared");
        assert_ne!(
            a.repository.unwrap().canonical_key,
            b.repository.unwrap().canonical_key
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_root_and_marker_fallbacks_are_lower_confidence() {
        let root = temp_root("fallbacks");
        let workspace_project = root.join("workspace/new-project/src");
        fs::create_dir_all(&workspace_project).unwrap();
        let marker_project = root.join("elsewhere/marked/src");
        fs::create_dir_all(&marker_project).unwrap();
        fs::write(root.join("elsewhere/marked/project.marker"), "x").unwrap();
        let mut resolver = ProjectResolver::new(
            ProjectResolverConfig {
                workspace_roots: vec![root.join("workspace")],
                marker_files: vec![OsString::from("project.marker")],
                ..ProjectResolverConfig::default()
            },
            FakeClock::new(1_000),
        );
        let workspace = resolver.resolve(&workspace_project);
        assert_eq!(workspace.status, ProjectResolutionStatus::WorkspaceRoot);
        assert_eq!(
            workspace.project.as_ref().unwrap().display_name,
            "new-project"
        );
        assert_eq!(workspace.confidence(), 0.7);
        let marker = resolver.resolve(&marker_project);
        assert_eq!(marker.status, ProjectResolutionStatus::Marker);
        assert_eq!(marker.project.as_ref().unwrap().display_name, "marked");
        assert_eq!(marker.confidence(), 0.6);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_privacy_modes_preserve_stable_identity_without_debug_leaks() {
        let root = temp_root("privacy");
        let project = git_project(&root, "secret-project");
        let canonical = fs::canonicalize(&project).unwrap();
        let full = PrivatePath::from_canonical(&canonical, PathPrivacyMode::Full);
        let basename = PrivatePath::from_canonical(&canonical, PathPrivacyMode::Basename);
        let hash = PrivatePath::from_canonical(&canonical, PathPrivacyMode::Hash);
        assert_eq!(full.stable_hash(), basename.stable_hash());
        assert_eq!(basename.stable_hash(), hash.stable_hash());
        assert!(full.display().unwrap().contains("secret-project"));
        assert_eq!(basename.display(), Some("secret-project"));
        assert_eq!(hash.display(), None);
        assert!(!format!("{full:?}").contains("secret-project"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_hits_then_invalidates_when_git_metadata_changes() {
        let root = temp_root("cache");
        let project = git_project(&root, "project");
        let clock = FakeClock::new(1_000);
        let mut resolver = ProjectResolver::new(
            ProjectResolverConfig {
                cache_ttl: Duration::from_secs(60),
                ..ProjectResolverConfig::default()
            },
            clock.clone(),
        );
        assert!(!resolver.resolve(&project).cache_hit);
        assert!(resolver.resolve(&project).cache_hit);
        fs::write(project.join(".git/commondir"), ".\n").unwrap();
        let changed = resolver.resolve(&project);
        assert!(!changed.cache_hit);
        clock.set(100_000);
        assert!(!resolver.resolve(&project).cache_hit);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unresolved_cache_invalidates_when_git_marker_is_created() {
        let root = temp_root("cache-new-git");
        let project = root.join("project/src");
        fs::create_dir_all(&project).unwrap();
        let mut resolver =
            ProjectResolver::new(ProjectResolverConfig::default(), FakeClock::new(1_000));
        let first = resolver.resolve(&project);
        assert_eq!(
            first.status,
            ProjectResolutionStatus::Unresolved(
                ProjectUnresolvedReason::OutsideConfiguredWorkspace
            )
        );
        assert!(resolver.resolve(&project).cache_hit);
        fs::create_dir_all(root.join("project/.git")).unwrap();
        let git = resolver.resolve(&project);
        assert!(!git.cache_hit);
        assert_eq!(git.status, ProjectResolutionStatus::Git);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_ignores_unrelated_ancestor_directory_changes() {
        let root = temp_root("cache-unrelated");
        let project = git_project(&root, "project");
        let mut resolver =
            ProjectResolver::new(ProjectResolverConfig::default(), FakeClock::new(1_000));
        assert!(!resolver.resolve(&project).cache_hit);
        fs::create_dir_all(root.join("unrelated-child")).unwrap();
        assert!(resolver.resolve(&project).cache_hit);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unresolved_cache_invalidates_only_for_configured_marker_metadata() {
        let root = temp_root("cache-new-marker");
        let project = root.join("project/src");
        fs::create_dir_all(&project).unwrap();
        let mut resolver = ProjectResolver::new(
            ProjectResolverConfig {
                marker_files: vec![OsString::from("project.marker")],
                ..ProjectResolverConfig::default()
            },
            FakeClock::new(1_000),
        );
        assert!(!resolver.resolve(&project).cache_hit);
        fs::write(root.join("project/unrelated.marker"), "x").unwrap();
        assert!(resolver.resolve(&project).cache_hit);
        fs::write(root.join("project/project.marker"), "x").unwrap();
        let marked = resolver.resolve(&project);
        assert!(!marked.cache_hit);
        assert_eq!(marked.status, ProjectResolutionStatus::Marker);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_git_pointer_is_rejected_without_unbounded_read() {
        let root = temp_root("oversized-git");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".git"), vec![b'x'; MAX_GIT_FILE_BYTES + 1]).unwrap();
        let mut resolver =
            ProjectResolver::new(ProjectResolverConfig::default(), FakeClock::new(1_000));
        assert_eq!(
            resolver.resolve(&project).status,
            ProjectResolutionStatus::Unresolved(ProjectUnresolvedReason::GitMetadataTooLarge)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_runner_rejects_argument_array_limits_before_spawn() {
        let runner = BoundedCommandRunner;
        let limits = CommandLimits {
            max_args: 1,
            max_arg_bytes: 3,
            ..CommandLimits::default()
        };
        assert!(matches!(
            runner.run(
                OsStr::new("echo"),
                &[OsString::from("one"), OsString::from("two")],
                limits
            ),
            Err(CommandRunError::InvalidInvocation)
        ));
        assert!(matches!(
            runner.run(OsStr::new("echo"), &[OsString::from("long")], limits),
            Err(CommandRunError::InvalidInvocation)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn production_runner_enforces_output_and_timeout_without_a_shell() {
        let runner = BoundedCommandRunner;
        if Path::new("/usr/bin/printf").exists() {
            let output = runner
                .run(
                    OsStr::new("/usr/bin/printf"),
                    &[OsString::from("bounded")],
                    CommandLimits::default(),
                )
                .unwrap();
            assert_eq!(output.stdout, b"bounded");
        }
        if Path::new("/usr/bin/yes").exists() {
            assert!(matches!(
                runner.run(
                    OsStr::new("/usr/bin/yes"),
                    &[],
                    CommandLimits {
                        timeout: Duration::from_secs(1),
                        max_output_bytes: 128,
                        ..CommandLimits::default()
                    }
                ),
                Err(CommandRunError::OutputLimitExceeded)
            ));
        }
        if Path::new("/usr/bin/sleep").exists() {
            assert!(matches!(
                runner.run(
                    OsStr::new("/usr/bin/sleep"),
                    &[OsString::from("1")],
                    CommandLimits {
                        timeout: Duration::from_millis(20),
                        ..CommandLimits::default()
                    }
                ),
                Err(CommandRunError::Timeout)
            ));
        }
    }
}
