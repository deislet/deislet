use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use zeroize::Zeroizing;

use crate::{DriverNextAction, Sha256Digest};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const REDACTED: &str = "[REDACTED]";

/// A credential value that zeroizes its owned allocation and never exposes
/// its contents through Debug or Display.
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Result<Self, CommandExecutionError> {
        let value = Zeroizing::new(value.into());
        if value.is_empty() || value.len() > MAX_SECRET_BYTES || value.contains('\0') {
            return Err(CommandExecutionError::before_start(
                CommandExecutionErrorKind::InvalidInvocation,
                "credential must be non-empty, contain no NUL, and fit the credential bound",
            ));
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

pub enum SecretInjection {
    ShortLivedEnvironment { name: OsString, value: SecretString },
    Stdin(SecretString),
}

impl SecretInjection {
    pub fn environment(
        name: impl Into<OsString>,
        value: SecretString,
    ) -> Result<Self, CommandExecutionError> {
        let name = name.into();
        validate_environment_name(&name)?;
        Ok(Self::ShortLivedEnvironment { name, value })
    }

    #[must_use]
    pub fn stdin(value: SecretString) -> Self {
        Self::Stdin(value)
    }

    fn value(&self) -> &str {
        match self {
            Self::ShortLivedEnvironment { value, .. } | Self::Stdin(value) => value.expose(),
        }
    }
}

impl fmt::Debug for SecretInjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortLivedEnvironment { name, .. } => formatter
                .debug_struct("ShortLivedEnvironment")
                .field("name", name)
                .field("value", &REDACTED)
                .finish(),
            Self::Stdin(_) => formatter.debug_tuple("Stdin").field(&REDACTED).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideEffectPolicy {
    ReadOnly,
    MayChangeProvider,
}

#[derive(Clone, Debug)]
pub struct PinnedExecutable {
    pub path: PathBuf,
    pub tool_name: String,
    pub version: String,
    pub executable_sha256: Sha256Digest,
}

impl PinnedExecutable {
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        tool_name: impl Into<String>,
        version: impl Into<String>,
        executable_sha256: Sha256Digest,
    ) -> Self {
        Self {
            path: path.into(),
            tool_name: tool_name.into(),
            version: version.into(),
            executable_sha256,
        }
    }
}

pub struct CommandInvocation {
    arguments: Vec<OsString>,
    working_directory: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    credentials: Vec<SecretInjection>,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    side_effect_policy: SideEffectPolicy,
}

impl CommandInvocation {
    #[must_use]
    pub fn new(
        working_directory: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
        side_effect_policy: SideEffectPolicy,
    ) -> Self {
        Self {
            arguments: arguments.into_iter().map(Into::into).collect(),
            working_directory: working_directory.into(),
            environment: BTreeMap::new(),
            credentials: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_stdout_bytes: DEFAULT_OUTPUT_LIMIT,
            max_stderr_bytes: DEFAULT_OUTPUT_LIMIT,
            side_effect_policy,
        }
    }

    pub fn environment(
        &mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Result<&mut Self, CommandExecutionError> {
        let name = name.into();
        validate_environment_name(&name)?;
        self.environment.insert(name, value.into());
        Ok(self)
    }

    pub fn credential(&mut self, credential: SecretInjection) -> &mut Self {
        self.credentials.push(credential);
        self
    }

    pub fn timeout(&mut self, value: Duration) -> Result<&mut Self, CommandExecutionError> {
        if value.is_zero() {
            return Err(CommandExecutionError::before_start(
                CommandExecutionErrorKind::InvalidInvocation,
                "official tool timeout must be greater than zero",
            ));
        }
        self.timeout = value;
        Ok(self)
    }

    pub fn output_limits(
        &mut self,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Result<&mut Self, CommandExecutionError> {
        if stdout_bytes == 0 || stderr_bytes == 0 {
            return Err(CommandExecutionError::before_start(
                CommandExecutionErrorKind::InvalidInvocation,
                "official tool output limits must be greater than zero",
            ));
        }
        self.max_stdout_bytes = stdout_bytes;
        self.max_stderr_bytes = stderr_bytes;
        Ok(self)
    }
}

impl fmt::Debug for CommandInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandInvocation")
            .field("arguments", &self.arguments)
            .field("working_directory", &self.working_directory)
            .field(
                "environment_names",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("credentials", &self.credentials)
            .field("timeout", &self.timeout)
            .field("max_stdout_bytes", &self.max_stdout_bytes)
            .field("max_stderr_bytes", &self.max_stderr_bytes)
            .field("side_effect_policy", &self.side_effect_policy)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandExecutionErrorKind {
    InvalidPin,
    IntegrityMismatch,
    InvalidInvocation,
    Spawn,
    Stdin,
    Timeout,
    OutputLimit,
    ExitStatus,
    InvalidStructuredOutput,
    Wait,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandExecutionError {
    pub kind: CommandExecutionErrorKind,
    pub message: String,
    pub next_action: DriverNextAction,
    pub diagnostic: Option<String>,
}

impl CommandExecutionError {
    fn before_start(kind: CommandExecutionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }

    fn after_start(
        kind: CommandExecutionErrorKind,
        message: impl Into<String>,
        policy: SideEffectPolicy,
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            next_action: match policy {
                SideEffectPolicy::ReadOnly => DriverNextAction::Retry,
                SideEffectPolicy::MayChangeProvider => DriverNextAction::Reconcile,
            },
            diagnostic,
        }
    }
}

impl fmt::Display for CommandExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommandExecutionError {}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub structured: Value,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PinnedCommandRunner;

impl PinnedCommandRunner {
    pub async fn run(
        &self,
        executable: &PinnedExecutable,
        invocation: &CommandInvocation,
    ) -> Result<CommandOutput, CommandExecutionError> {
        validate_pin(executable)?;
        validate_invocation(invocation)?;
        verify_executable(executable)?;

        let secrets = invocation
            .credentials
            .iter()
            .map(SecretInjection::value)
            .collect::<Vec<_>>();
        reject_secret_in_public_inputs(executable, invocation, &secrets)?;

        let mut command = Command::new(&executable.path);
        command
            .args(&invocation.arguments)
            .current_dir(&invocation.working_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (name, value) in &invocation.environment {
            command.env(name, value);
        }
        for credential in &invocation.credentials {
            if let SecretInjection::ShortLivedEnvironment { name, value } = credential {
                command.env(name, value.expose());
            }
        }
        configure_process_group(&mut command);

        let deadline = Instant::now() + invocation.timeout;
        let mut child = command.spawn().map_err(|error| {
            CommandExecutionError::before_start(
                CommandExecutionErrorKind::Spawn,
                format!("could not start pinned official tool: {error}"),
            )
        })?;
        // Drop the std/tokio command builder immediately so its private copy
        // of the short-lived credential environment is not retained for the
        // duration of the provider operation.
        drop(command);
        let mut process_group = match ProcessGroup::attach(&child) {
            Ok(group) => group,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Spawn,
                    format!("could not establish official tool cancellation boundary: {error}"),
                    invocation.side_effect_policy,
                    None,
                ));
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate(&mut child, &mut process_group).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Spawn,
                    "official tool stdout pipe was not created",
                    invocation.side_effect_policy,
                    None,
                ));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate(&mut child, &mut process_group).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Spawn,
                    "official tool stderr pipe was not created",
                    invocation.side_effect_policy,
                    None,
                ));
            }
        };
        let stdout_task = tokio::spawn(read_bounded(stdout, invocation.max_stdout_bytes));
        let stderr_task = tokio::spawn(read_bounded(stderr, invocation.max_stderr_bytes));

        match timeout_at(deadline, write_stdin(&mut child, &invocation.credentials)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                terminate(&mut child, &mut process_group).await;
                drain_capture_tasks(stdout_task, stderr_task).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Stdin,
                    format!("could not deliver official tool credential on stdin: {error}"),
                    invocation.side_effect_policy,
                    None,
                ));
            }
            Err(_) => {
                terminate(&mut child, &mut process_group).await;
                drain_capture_tasks(stdout_task, stderr_task).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Timeout,
                    format!(
                        "pinned official tool exceeded its {} ms timeout while reading stdin",
                        invocation.timeout.as_millis()
                    ),
                    invocation.side_effect_policy,
                    None,
                ));
            }
        }

        let status = match timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                terminate(&mut child, &mut process_group).await;
                drain_capture_tasks(stdout_task, stderr_task).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Wait,
                    format!("could not observe official tool exit: {error}"),
                    invocation.side_effect_policy,
                    None,
                ));
            }
            Err(_) => {
                terminate(&mut child, &mut process_group).await;
                drain_capture_tasks(stdout_task, stderr_task).await;
                return Err(CommandExecutionError::after_start(
                    CommandExecutionErrorKind::Timeout,
                    format!(
                        "pinned official tool exceeded its {} ms timeout",
                        invocation.timeout.as_millis()
                    ),
                    invocation.side_effect_policy,
                    None,
                ));
            }
        };
        // A well-behaved CLI leaves no background process behind. Closing the
        // group here also closes inherited stdout/stderr handles, so an
        // otherwise successful parent cannot make capture wait forever.
        process_group.terminate();

        let stdout = join_capture(stdout_task, "stdout", invocation.side_effect_policy).await?;
        let stderr = join_capture(stderr_task, "stderr", invocation.side_effect_policy).await?;
        let sanitized_stdout = redact(&String::from_utf8_lossy(&stdout.bytes), &secrets);
        let sanitized_stderr = redact(&String::from_utf8_lossy(&stderr.bytes), &secrets);
        if stdout.exceeded || stderr.exceeded {
            return Err(CommandExecutionError::after_start(
                CommandExecutionErrorKind::OutputLimit,
                "official tool output exceeded its configured bound",
                invocation.side_effect_policy,
                Some(sanitized_diagnostic(&sanitized_stdout, &sanitized_stderr)),
            ));
        }
        if !status.success() {
            return Err(CommandExecutionError::after_start(
                CommandExecutionErrorKind::ExitStatus,
                format!(
                    "official tool exited with status {}",
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| code.to_string())
                ),
                invocation.side_effect_policy,
                Some(sanitized_diagnostic(&sanitized_stdout, &sanitized_stderr)),
            ));
        }

        let mut structured: Value = serde_json::from_slice(&stdout.bytes).map_err(|error| {
            CommandExecutionError::after_start(
                CommandExecutionErrorKind::InvalidStructuredOutput,
                format!("official tool did not return one JSON document: {error}"),
                invocation.side_effect_policy,
                Some(sanitized_diagnostic(&sanitized_stdout, &sanitized_stderr)),
            )
        })?;
        redact_json(&mut structured, &secrets);

        Ok(CommandOutput {
            exit_code: status.code().unwrap_or_default(),
            structured,
            stdout: sanitized_stdout,
            stderr: sanitized_stderr,
        })
    }
}

fn validate_environment_name(name: &OsStr) -> Result<(), CommandExecutionError> {
    let text = name.to_str().ok_or_else(|| {
        CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "environment variable names must be UTF-8",
        )
    })?;
    if text.is_empty() || text.contains('=') || text.contains('\0') {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "environment variable name is empty or contains a forbidden byte",
        ));
    }
    Ok(())
}

fn validate_pin(executable: &PinnedExecutable) -> Result<(), CommandExecutionError> {
    if !executable.path.is_absolute()
        || executable.tool_name.is_empty()
        || executable.version.is_empty()
    {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidPin,
            "official tool pin needs an absolute path, name, exact version and digest",
        ));
    }
    Ok(())
}

fn validate_invocation(invocation: &CommandInvocation) -> Result<(), CommandExecutionError> {
    if !invocation.working_directory.is_absolute()
        || !invocation.working_directory.is_dir()
        || invocation.timeout.is_zero()
        || invocation.max_stdout_bytes == 0
        || invocation.max_stderr_bytes == 0
    {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "official tool invocation needs an existing absolute cwd and non-zero bounds",
        ));
    }
    let stdin_count = invocation
        .credentials
        .iter()
        .filter(|credential| matches!(credential, SecretInjection::Stdin(_)))
        .count();
    if stdin_count > 1 {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "only one credential payload may use stdin",
        ));
    }
    let credential_environment = invocation
        .credentials
        .iter()
        .filter_map(|credential| match credential {
            SecretInjection::ShortLivedEnvironment { name, .. } => Some(name),
            SecretInjection::Stdin(_) => None,
        })
        .collect::<BTreeSet<_>>();
    if credential_environment.len()
        != invocation
            .credentials
            .iter()
            .filter(|credential| {
                matches!(credential, SecretInjection::ShortLivedEnvironment { .. })
            })
            .count()
        || credential_environment
            .iter()
            .any(|name| invocation.environment.contains_key(*name))
    {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "credential environment names must be unique and may not overlap public environment",
        ));
    }
    Ok(())
}

fn verify_executable(executable: &PinnedExecutable) -> Result<(), CommandExecutionError> {
    let metadata = std::fs::symlink_metadata(&executable.path).map_err(|error| {
        CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidPin,
            format!("could not inspect pinned official tool: {error}"),
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > MAX_EXECUTABLE_BYTES
    {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidPin,
            "pinned official tool must be a bounded regular file, not a symlink",
        ));
    }
    let mut file = File::open(&executable.path).map_err(|error| {
        CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidPin,
            format!("could not open pinned official tool: {error}"),
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            CommandExecutionError::before_start(
                CommandExecutionErrorKind::InvalidPin,
                format!("could not hash pinned official tool: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_EXECUTABLE_BYTES {
            return Err(CommandExecutionError::before_start(
                CommandExecutionErrorKind::InvalidPin,
                "pinned official tool grew beyond the executable size bound while hashing",
            ));
        }
        digest.update(&buffer[..read]);
    }
    let hashed_metadata = file.metadata().map_err(|error| {
        CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidPin,
            format!("could not re-inspect pinned official tool after hashing: {error}"),
        )
    })?;
    if hashed_metadata.len() != total || metadata.len() != total {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::IntegrityMismatch,
            "pinned official tool changed size while it was being verified",
        ));
    }
    let actual = hex::encode(digest.finalize());
    if actual != executable.executable_sha256.as_str() {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::IntegrityMismatch,
            format!(
                "pinned official tool digest mismatch: expected {}, observed {actual}",
                executable.executable_sha256
            ),
        ));
    }
    Ok(())
}

fn reject_secret_in_public_inputs(
    executable: &PinnedExecutable,
    invocation: &CommandInvocation,
    secrets: &[&str],
) -> Result<(), CommandExecutionError> {
    let leaked = secrets.iter().any(|secret| {
        executable.path.to_string_lossy().contains(secret)
            || invocation
                .working_directory
                .to_string_lossy()
                .contains(secret)
    }) || invocation.arguments.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        secrets.iter().any(|secret| argument.contains(secret))
    }) || invocation.environment.iter().any(|(name, value)| {
        let name = name.to_string_lossy();
        let value = value.to_string_lossy();
        secrets
            .iter()
            .any(|secret| name.contains(secret) || value.contains(secret))
    });
    if leaked {
        return Err(CommandExecutionError::before_start(
            CommandExecutionErrorKind::InvalidInvocation,
            "credential value appeared in argv or the public process environment",
        ));
    }
    Ok(())
}

async fn write_stdin(child: &mut Child, credentials: &[SecretInjection]) -> io::Result<()> {
    let stdin_secret = credentials.iter().find_map(|credential| match credential {
        SecretInjection::Stdin(value) => Some(value.expose()),
        SecretInjection::ShortLivedEnvironment { .. } => None,
    });
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(secret) = stdin_secret {
            stdin.write_all(secret.as_bytes()).await?;
        }
        stdin.shutdown().await?;
    }
    Ok(())
}

struct Capture {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<Capture> {
    let mut retained = Vec::with_capacity(limit.min(64 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    Ok(Capture {
        bytes: retained,
        exceeded,
    })
}

async fn join_capture(
    task: JoinHandle<io::Result<Capture>>,
    stream: &str,
    policy: SideEffectPolicy,
) -> Result<Capture, CommandExecutionError> {
    task.await
        .map_err(|error| {
            CommandExecutionError::after_start(
                CommandExecutionErrorKind::Wait,
                format!("official tool {stream} capture task failed: {error}"),
                policy,
                None,
            )
        })?
        .map_err(|error| {
            CommandExecutionError::after_start(
                CommandExecutionErrorKind::Wait,
                format!("could not read official tool {stream}: {error}"),
                policy,
                None,
            )
        })
}

async fn drain_capture_tasks(
    stdout: JoinHandle<io::Result<Capture>>,
    stderr: JoinHandle<io::Result<Capture>>,
) {
    let _ = stdout.await;
    let _ = stderr.await;
}

fn redact(value: &str, secrets: &[&str]) -> String {
    let mut output = value.to_owned();
    let mut ordered = secrets.to_vec();
    ordered.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in ordered {
        output = output.replace(secret, REDACTED);
    }
    output
}

fn redact_json(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => *text = redact(text, secrets),
        Value::Array(values) => {
            for value in values {
                redact_json(value, secrets);
            }
        }
        Value::Object(object) => {
            let previous = std::mem::take(object);
            for (key, mut value) in previous {
                redact_json(&mut value, secrets);
                object.insert(redact(&key, secrets), value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn sanitized_diagnostic(stdout: &str, stderr: &str) -> String {
    format!("stdout: {stdout}\nstderr: {stderr}")
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
struct ProcessGroup {
    id: Option<i32>,
}

#[cfg(unix)]
impl ProcessGroup {
    fn attach(child: &Child) -> io::Result<Self> {
        Ok(Self {
            id: Some(
                child
                    .id()
                    .ok_or_else(|| io::Error::other("spawned process has no Unix PID"))?
                    .try_into()
                    .map_err(|_| {
                        io::Error::other("spawned Unix PID exceeds process-group range")
                    })?,
            ),
        })
    }

    fn terminate(&mut self) {
        if let Some(id) = self.id.take() {
            // SAFETY: the spawned child was placed in a new process group with
            // its PID as PGID. A negative PID addresses exactly that group.
            unsafe {
                libc::kill(-id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
struct ProcessGroup {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessGroup {
    fn attach(child: &Child) -> io::Result<Self> {
        use std::mem::size_of;
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let raw_process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("spawned process has no Windows handle"))?
            as HANDLE;
        // SAFETY: all pointers reference initialized local values for the
        // documented Windows Job Object APIs. The returned handle is closed by
        // Drop and no borrowed pointer escapes this function.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
                || AssignProcessToJobObject(handle, raw_process) == 0
            {
                let error = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            Ok(Self { handle })
        }
    }

    fn terminate(&mut self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: handle is a live Job Object owned by self.
        unsafe {
            TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: handle is owned by self and closed exactly once here.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessGroup;

#[cfg(not(any(unix, windows)))]
impl ProcessGroup {
    fn attach(_child: &Child) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "official tool process-group cancellation is unsupported on this host",
        ))
    }

    fn terminate(&mut self) {}
}

async fn terminate(child: &mut Child, process_group: &mut ProcessGroup) {
    process_group.terminate();
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use tempfile::TempDir;

    use super::*;

    fn executable(temp: &TempDir, name: &str, body: &str) -> PinnedExecutable {
        let path = temp.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let digest = hex::encode(Sha256::digest(bytes)).parse().unwrap();
        PinnedExecutable::new(path, name, "1.0.0", digest)
    }

    #[tokio::test]
    async fn clears_environment_injects_secret_outside_argv_and_redacts_all_output() {
        // SAFETY: this test owns the variable name, and the child is the only
        // concurrent observer that matters to the assertion.
        unsafe { std::env::set_var("DEIS_DRIVER_MUST_NOT_LEAK", "host-secret") };
        let temp = TempDir::new().unwrap();
        let pin = executable(
            &temp,
            "success",
            r#"
test "${DEIS_DRIVER_MUST_NOT_LEAK+x}" != x
printf 'provider saw %s\n' "$PROVIDER_TOKEN" >&2
printf '{"ok":true,"echo":"%s","%s":"key"}' "$PROVIDER_TOKEN" "$PROVIDER_TOKEN"
"#,
        );
        let secret_text = "credential-which-must-never-survive";
        let mut invocation =
            CommandInvocation::new(temp.path(), ["--machine-json"], SideEffectPolicy::ReadOnly);
        invocation.credential(
            SecretInjection::environment("PROVIDER_TOKEN", SecretString::new(secret_text).unwrap())
                .unwrap(),
        );

        let output = PinnedCommandRunner.run(&pin, &invocation).await.unwrap();
        assert_eq!(output.structured["ok"], true);
        assert_eq!(output.structured["echo"], REDACTED);
        assert_eq!(output.structured[REDACTED], "key");
        assert!(!output.stdout.contains(secret_text));
        assert!(!output.stderr.contains(secret_text));
        assert!(output.stderr.contains(REDACTED));
        assert!(!format!("{invocation:?}").contains(secret_text));
    }

    #[tokio::test]
    async fn delivers_one_stdin_secret_and_closes_the_stream() {
        let temp = TempDir::new().unwrap();
        let pin = executable(
            &temp,
            "stdin",
            "value=$(cat)\nprintf '{\"received\":\"%s\"}' \"$value\"",
        );
        let secret_text = "stdin-credential";
        let mut invocation = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::ReadOnly,
        );
        invocation.credential(SecretInjection::stdin(
            SecretString::new(secret_text).unwrap(),
        ));
        let output = PinnedCommandRunner.run(&pin, &invocation).await.unwrap();
        assert_eq!(output.structured["received"], REDACTED);
        assert!(!output.stdout.contains(secret_text));
    }

    #[tokio::test]
    async fn refuses_argv_secret_before_the_tool_can_start() {
        let temp = TempDir::new().unwrap();
        let marker = temp.path().join("started");
        let pin = executable(
            &temp,
            "must-not-run",
            &format!("touch '{}'\nprintf '{{}}'", marker.display()),
        );
        let secret_text = "argv-credential";
        let mut invocation = CommandInvocation::new(
            temp.path(),
            [format!("--token={secret_text}")],
            SideEffectPolicy::MayChangeProvider,
        );
        invocation.credential(
            SecretInjection::environment("PROVIDER_TOKEN", SecretString::new(secret_text).unwrap())
                .unwrap(),
        );
        let error = PinnedCommandRunner
            .run(&pin, &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::InvalidInvocation);
        assert_eq!(error.next_action, DriverNextAction::None);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn refuses_digest_mismatch_and_symlink() {
        let temp = TempDir::new().unwrap();
        let mut pin = executable(&temp, "real", "printf '{}'");
        pin.executable_sha256 = "f".repeat(64).parse().unwrap();
        let invocation = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::ReadOnly,
        );
        let error = PinnedCommandRunner
            .run(&pin, &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::IntegrityMismatch);

        let real = executable(&temp, "real-two", "printf '{}'");
        let linked_path = temp.path().join("linked");
        symlink(&real.path, &linked_path).unwrap();
        let linked = PinnedExecutable::new(linked_path, "linked", "1.0.0", real.executable_sha256);
        let error = PinnedCommandRunner
            .run(&linked, &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::InvalidPin);
    }

    #[tokio::test]
    async fn bounds_output_and_requires_reconcile_after_side_effecting_failure() {
        let temp = TempDir::new().unwrap();
        let pin = executable(
            &temp,
            "too-much",
            "printf '{\"value\":\"012345678901234567890123456789\"}'",
        );
        let mut invocation = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::MayChangeProvider,
        );
        invocation.output_limits(8, 8).unwrap();
        let error = PinnedCommandRunner
            .run(&pin, &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::OutputLimit);
        assert_eq!(error.next_action, DriverNextAction::Reconcile);
        assert!(error.diagnostic.unwrap().len() < 64);
    }

    #[tokio::test]
    async fn times_out_the_process_group_and_requires_query_before_retry() {
        let temp = TempDir::new().unwrap();
        let pin = executable(&temp, "slow", "sleep 10\nprintf '{}'");
        let mut invocation = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::MayChangeProvider,
        );
        invocation.timeout(Duration::from_millis(50)).unwrap();
        let error = PinnedCommandRunner
            .run(&pin, &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::Timeout);
        assert_eq!(error.next_action, DriverNextAction::Reconcile);
    }

    #[tokio::test]
    async fn successful_parent_cannot_leave_a_background_capture_process() {
        let temp = TempDir::new().unwrap();
        let pin = executable(&temp, "background", "(sleep 10) &\nprintf '{}'");
        let mut invocation = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::ReadOnly,
        );
        // This is a process-tree cleanup assertion, not a scheduler latency
        // benchmark. The background child sleeps for ten seconds, so five
        // seconds still fails decisively if cleanup regresses while leaving
        // enough margin for a cold, concurrently compiling macOS host.
        invocation.timeout(Duration::from_secs(5)).unwrap();
        let output = PinnedCommandRunner
            .run(&pin, &invocation)
            .await
            .expect("the completed parent tears down its remaining process group");
        assert_eq!(output.structured, serde_json::json!({}));
    }

    #[tokio::test]
    async fn nonzero_and_non_json_outputs_are_not_success() {
        let temp = TempDir::new().unwrap();
        let failed = executable(&temp, "failed", "printf 'failure' >&2\nexit 7");
        let read_only = CommandInvocation::new(
            temp.path(),
            Vec::<String>::new(),
            SideEffectPolicy::ReadOnly,
        );
        let error = PinnedCommandRunner
            .run(&failed, &read_only)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::ExitStatus);
        assert_eq!(error.next_action, DriverNextAction::Retry);

        let text = executable(&temp, "text", "printf 'human output'");
        let error = PinnedCommandRunner
            .run(&text, &read_only)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            CommandExecutionErrorKind::InvalidStructuredOutput
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use tempfile::TempDir;

    use super::*;

    fn powershell_pin() -> PinnedExecutable {
        let system_root = std::env::var_os("SystemRoot").expect("Windows defines SystemRoot");
        let path = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let bytes = std::fs::read(&path).unwrap();
        let digest = hex::encode(Sha256::digest(bytes)).parse().unwrap();
        PinnedExecutable::new(path, "Windows PowerShell", "5.1", digest)
    }

    fn powershell_invocation(temp: &TempDir, script: &str) -> CommandInvocation {
        let mut invocation = CommandInvocation::new(
            temp.path(),
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ],
            SideEffectPolicy::ReadOnly,
        );
        invocation
            .environment(
                "SystemRoot",
                std::env::var_os("SystemRoot").expect("Windows defines SystemRoot"),
            )
            .unwrap();
        invocation
    }

    #[tokio::test]
    async fn windows_job_object_allows_bounded_noninteractive_execution() {
        let temp = TempDir::new().unwrap();
        let invocation = powershell_invocation(&temp, "Write-Output '{\"ok\":true}'");
        let output = PinnedCommandRunner
            .run(&powershell_pin(), &invocation)
            .await
            .unwrap();
        assert_eq!(output.structured["ok"], true);
    }

    #[tokio::test]
    async fn windows_job_object_terminates_a_timed_out_tool() {
        let temp = TempDir::new().unwrap();
        let mut invocation = powershell_invocation(
            &temp,
            "Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 30'; Start-Sleep 30",
        );
        invocation.timeout(Duration::from_millis(200)).unwrap();
        let error = PinnedCommandRunner
            .run(&powershell_pin(), &invocation)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CommandExecutionErrorKind::Timeout);
    }
}
