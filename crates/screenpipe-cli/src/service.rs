use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub(crate) struct ServiceRoot {
    local_app_data: PathBuf,
}

impl ServiceRoot {
    pub(crate) fn current_user() -> Result<Self> {
        let local_app_data = windows_local_app_data()?;
        if !local_app_data.is_absolute() {
            bail!("Windows LocalAppData known folder must be absolute");
        }
        Ok(Self { local_app_data })
    }

    #[cfg(test)]
    fn for_test(local_app_data: PathBuf) -> Self {
        assert!(local_app_data.is_absolute());
        Self { local_app_data }
    }
}

#[cfg(windows)]
fn windows_local_app_data() -> Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .context("resolve Windows LocalAppData known folder")?;
    let value = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(
        value.context("decode Windows LocalAppData known folder")?,
    ))
}

#[cfg(not(windows))]
fn windows_local_app_data() -> Result<PathBuf> {
    bail!("per-user service management requires Windows")
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TaskAction {
    pub(crate) executable: &'static str,
    pub(crate) arguments: String,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskTrigger {
    UserLogon,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TaskPrincipal {
    pub(crate) user: TaskUser,
    pub(crate) logon_type: TaskLogonType,
    pub(crate) run_level: TaskRunLevel,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskUser {
    CurrentUser,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskLogonType {
    InteractiveToken,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskRunLevel {
    Limited,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ServiceSpec {
    pub(crate) task_name: &'static str,
    pub(crate) root_path: PathBuf,
    pub(crate) binary_path: PathBuf,
    pub(crate) wrapper_path: PathBuf,
    pub(crate) action: TaskAction,
    pub(crate) trigger: TaskTrigger,
    pub(crate) principal: TaskPrincipal,
    pub(crate) wrapper_contents: String,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ServiceSpec {
    pub(crate) fn for_current_user(root: &ServiceRoot) -> Self {
        let root_path = root.local_app_data.join("screen-memory");
        let binary_path = root_path.join(r"bin\screenpipe.exe");
        let wrapper_path = root_path.join("run-screenpipe.ps1");
        let action = TaskAction {
            executable: "powershell.exe",
            arguments: format!(
                r#"-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "{}""#,
                wrapper_path.display()
            ),
        };
        let trigger = TaskTrigger::UserLogon;
        let principal = TaskPrincipal {
            user: TaskUser::CurrentUser,
            logon_type: TaskLogonType::InteractiveToken,
            run_level: TaskRunLevel::Limited,
        };
        let escaped_agent = binary_path.to_string_lossy().replace('\'', "''");
        let escaped_log_directory = root_path.join("logs").to_string_lossy().replace('\'', "''");
        // Task Scheduler runs this wrapper hidden, so without redirection every
        // `screen_started`, `capture_gap`, and `capture_error` line the agent
        // prints goes to a console nobody can read and dies with the process -
        // leaving a 24-hour unattended run with no diagnostic record at all.
        // `*>>` captures every stream, appending so a wrapper restart never
        // truncates the evidence from the run that just failed.
        let wrapper_contents = format!(
            r#"$ErrorActionPreference = 'Continue'
$agent = '{escaped_agent}'
$logDirectory = '{escaped_log_directory}'
if (-not (Test-Path -LiteralPath $logDirectory)) {{
    New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
}}
while ($true) {{
    $log = Join-Path $logDirectory ('screenpipe-agent-{{0:yyyy-MM-dd}}.log' -f (Get-Date))
    "=== agent start {{0:o}} ===" -f (Get-Date).ToUniversalTime() | Out-File -LiteralPath $log -Append -Encoding utf8
    doppler run -p homelab -c dev_personal -- $agent run --machine-slug icarus --display-name Icarus-Laptop *>> $log
    "=== agent exited {{0:o}} exit={{1}} ===" -f (Get-Date).ToUniversalTime(), $LASTEXITCODE | Out-File -LiteralPath $log -Append -Encoding utf8
    Start-Sleep -Seconds 10
}}
"#
        );

        Self {
            task_name: "MooseGoose Screen Memory",
            root_path,
            binary_path,
            wrapper_path,
            action,
            trigger,
            principal,
            wrapper_contents,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskState {
    Absent,
    Ready,
    Running,
    Other(String),
}

impl fmt::Display for TaskState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Ready => formatter.write_str("ready"),
            Self::Running => formatter.write_str("running"),
            Self::Other(state) => write!(formatter, "{}", state.to_ascii_lowercase()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    pub(crate) task_state: TaskState,
    pub(crate) process_running: bool,
}

pub(crate) trait TaskScheduler {
    fn stop(&mut self, spec: &ServiceSpec) -> Result<()>;
    fn install(&mut self, spec: &ServiceSpec) -> Result<()>;
    fn uninstall(&mut self, task_name: &str) -> Result<()>;
    fn status(&mut self, task_name: &str, binary_path: &Path) -> Result<ServiceStatus>;
}

pub(crate) struct WindowsTaskScheduler;

impl TaskScheduler for WindowsTaskScheduler {
    fn stop(&mut self, spec: &ServiceSpec) -> Result<()> {
        run_powershell(
            stop_task_script(),
            &[
                ("SCREENPIPE_TASK_NAME", spec.task_name.to_owned()),
                (
                    "SCREENPIPE_SERVICE_BINARY",
                    spec.binary_path.to_string_lossy().into_owned(),
                ),
            ],
        )?;
        Ok(())
    }

    fn install(&mut self, spec: &ServiceSpec) -> Result<()> {
        run_powershell(
            install_task_script(),
            &[
                ("SCREENPIPE_TASK_NAME", spec.task_name.to_owned()),
                (
                    "SCREENPIPE_TASK_EXECUTABLE",
                    spec.action.executable.to_owned(),
                ),
                (
                    "SCREENPIPE_TASK_ARGUMENTS",
                    spec.action.arguments.to_owned(),
                ),
            ],
        )?;
        Ok(())
    }

    fn uninstall(&mut self, task_name: &str) -> Result<()> {
        run_powershell(
            uninstall_task_script(),
            &[("SCREENPIPE_TASK_NAME", task_name.to_owned())],
        )?;
        Ok(())
    }

    fn status(&mut self, task_name: &str, binary_path: &Path) -> Result<ServiceStatus> {
        let output = run_powershell(
            status_task_script(),
            &[
                ("SCREENPIPE_TASK_NAME", task_name.to_owned()),
                (
                    "SCREENPIPE_SERVICE_BINARY",
                    binary_path.to_string_lossy().into_owned(),
                ),
            ],
        )?;
        parse_status_output(&output)
    }
}

pub(crate) struct ServiceManager<S> {
    scheduler: S,
}

impl<S: TaskScheduler> ServiceManager<S> {
    pub(crate) fn new(scheduler: S) -> Self {
        Self { scheduler }
    }

    pub(crate) fn install(
        &mut self,
        root: &ServiceRoot,
        current_exe: &Path,
    ) -> Result<ServiceStatus> {
        let spec = ServiceSpec::for_current_user(root);
        assert_owned_artifact(&spec, &spec.binary_path)?;
        assert_owned_artifact(&spec, &spec.wrapper_path)?;
        if !current_exe.is_file() {
            bail!("running screenpipe executable is not a file");
        }

        self.scheduler
            .stop(&spec)
            .context("stop existing service task and process tree")?;
        fs::create_dir_all(
            spec.binary_path
                .parent()
                .context("service binary path has no parent")?,
        )
        .context("create service binary directory")?;
        assert_owned_artifact(&spec, &spec.binary_path)?;
        assert_owned_artifact(&spec, &spec.wrapper_path)?;
        if current_exe != spec.binary_path {
            replace_file_from(&spec, current_exe, &spec.binary_path)
                .context("copy service executable")?;
        }
        replace_file_contents(&spec, &spec.wrapper_path, spec.wrapper_contents.as_bytes())
            .context("write service wrapper")?;
        self.scheduler
            .install(&spec)
            .context("register per-user service task")?;
        self.scheduler
            .status(spec.task_name, &spec.binary_path)
            .context("query installed service status")
    }

    pub(crate) fn uninstall(&mut self, root: &ServiceRoot) -> Result<ServiceStatus> {
        let spec = ServiceSpec::for_current_user(root);
        assert_owned_artifact(&spec, &spec.binary_path)?;
        assert_owned_artifact(&spec, &spec.wrapper_path)?;
        self.scheduler
            .stop(&spec)
            .context("stop service task and process tree")?;
        self.scheduler
            .uninstall(spec.task_name)
            .context("unregister per-user service task")?;
        assert_owned_artifact(&spec, &spec.binary_path)?;
        remove_owned_file(&spec.binary_path).context("remove copied service executable")?;
        assert_owned_artifact(&spec, &spec.wrapper_path)?;
        remove_owned_file(&spec.wrapper_path).context("remove service wrapper")?;
        self.scheduler
            .status(spec.task_name, &spec.binary_path)
            .context("query uninstalled service status")
    }

    pub(crate) fn status(&mut self, root: &ServiceRoot) -> Result<ServiceStatus> {
        let spec = ServiceSpec::for_current_user(root);
        self.scheduler
            .status(spec.task_name, &spec.binary_path)
            .context("query service status")
    }
}

fn assert_owned_artifact(spec: &ServiceSpec, path: &Path) -> Result<()> {
    if !path.starts_with(&spec.root_path) || path == spec.root_path {
        bail!("service artifact escapes the screen-memory root");
    }
    assert_no_reparse_components(&spec.root_path, path)?;
    Ok(())
}

fn assert_no_reparse_components(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("service artifact escapes the screen-memory root")?;
    let mut component_path = root.to_path_buf();
    assert_not_reparse_point(&component_path)?;
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("service artifact contains an unsafe path component");
        };
        component_path.push(component);
        assert_not_reparse_point(&component_path)?;
    }
    Ok(())
}

fn assert_not_reparse_point(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata_is_reparse_point(&metadata) {
        bail!(
            "service artifact path contains a reparse point: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn replace_file_from(spec: &ServiceSpec, source: &Path, destination: &Path) -> Result<()> {
    let temporary = destination.with_extension("exe.installing");
    assert_owned_artifact(spec, &temporary)?;
    assert_owned_artifact(spec, destination)?;
    remove_owned_file(&temporary)?;
    fs::copy(source, &temporary)?;
    replace_temporary_file(spec, &temporary, destination)
}

fn replace_file_contents(spec: &ServiceSpec, destination: &Path, contents: &[u8]) -> Result<()> {
    let temporary = destination.with_extension("ps1.installing");
    assert_owned_artifact(spec, &temporary)?;
    assert_owned_artifact(spec, destination)?;
    remove_owned_file(&temporary)?;
    fs::write(&temporary, contents)?;
    replace_temporary_file(spec, &temporary, destination)
}

fn replace_temporary_file(spec: &ServiceSpec, temporary: &Path, destination: &Path) -> Result<()> {
    assert_owned_artifact(spec, temporary)?;
    assert_owned_artifact(spec, destination)?;
    remove_owned_file(destination)?;
    fs::rename(temporary, destination)?;
    Ok(())
}

fn remove_owned_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn install_task_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $env:SCREENPIPE_TASK_EXECUTABLE -Argument $env:SCREENPIPE_TASK_ARGUMENTS
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -Action $action -Trigger $trigger -Principal $principal -Force | Out-Null
"#
}

fn stop_task_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$task = Get-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -ErrorAction SilentlyContinue
if ($null -ne $task) {
    Stop-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -ErrorAction Stop
    $taskDeadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $task = Get-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -ErrorAction Stop
        if ($task.State -ne 'Running') { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $taskDeadline)
    if ($task.State -eq 'Running') {
        throw "Exact screenpipe scheduled task did not leave Running within five seconds."
    }
}
function Get-ScreenpipeOwnedProcess {
    @(
        Get-CimInstance Win32_Process |
            Where-Object {
                [string]::Equals(
                    $_.ExecutablePath,
                    $env:SCREENPIPE_SERVICE_BINARY,
                    [System.StringComparison]::OrdinalIgnoreCase
                )
            }
    )
}
$deadline = [DateTime]::UtcNow.AddSeconds(5)
do {
    $owned = @(Get-ScreenpipeOwnedProcess)
    foreach ($process in $owned) {
        $process | Invoke-CimMethod -MethodName Terminate -ErrorAction SilentlyContinue | Out-Null
    }
    if ($owned.Count -gt 0) { Start-Sleep -Milliseconds 100 }
} while ($owned.Count -gt 0 -and [DateTime]::UtcNow -lt $deadline)
$remaining = @(Get-ScreenpipeOwnedProcess)
if ($remaining.Count -gt 0) {
    throw "Exact screenpipe service process tree did not stop within five seconds."
}
"#
}

fn uninstall_task_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$task = Get-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -ErrorAction SilentlyContinue
if ($null -ne $task) {
    Unregister-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -Confirm:$false
}
"#
}

fn status_task_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$task = Get-ScheduledTask -TaskName $env:SCREENPIPE_TASK_NAME -ErrorAction SilentlyContinue
$taskState = if ($null -eq $task) { 'Absent' } else { $task.State.ToString() }
$processRunning = @(
    Get-CimInstance Win32_Process -Filter "Name = 'screenpipe.exe'" |
        Where-Object {
            [string]::Equals(
                $_.ExecutablePath,
                $env:SCREENPIPE_SERVICE_BINARY,
                [System.StringComparison]::OrdinalIgnoreCase
            )
        }
).Count -gt 0
[Console]::Out.Write("$taskState|$processRunning")
"#
}

fn parse_status_output(output: &str) -> Result<ServiceStatus> {
    let mut fields = output.trim().split('|');
    let state = fields.next().context("task status output has no state")?;
    let process = fields
        .next()
        .context("task status output has no process state")?;
    if state.is_empty() || fields.next().is_some() {
        bail!("task status output has an invalid field count");
    }
    let process_running = if process.eq_ignore_ascii_case("true") {
        true
    } else if process.eq_ignore_ascii_case("false") {
        false
    } else {
        bail!("task status output has an invalid process state");
    };
    let task_state = if state.eq_ignore_ascii_case("absent") {
        TaskState::Absent
    } else if state.eq_ignore_ascii_case("ready") {
        TaskState::Ready
    } else if state.eq_ignore_ascii_case("running") {
        TaskState::Running
    } else {
        TaskState::Other(state.to_owned())
    };
    Ok(ServiceStatus {
        task_state,
        process_running,
    })
}

fn run_powershell(script: &str, environment: &[(&str, String)]) -> Result<String> {
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env_clear();
    for name in [
        "SystemRoot",
        "WINDIR",
        "PATH",
        "PATHEXT",
        "COMSPEC",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(environment.iter().map(|(name, value)| (*name, value)));
    let output = command.output().context("launch Windows PowerShell")?;
    if !output.status.success() {
        bail!(
            "Windows Task Scheduler command failed with exit code {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("Windows Task Scheduler output is not UTF-8")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;

    use anyhow::{Result, bail};
    use tempfile::TempDir;

    use super::{
        ServiceManager, ServiceRoot, ServiceSpec, ServiceStatus, TaskAction, TaskLogonType,
        TaskPrincipal, TaskRunLevel, TaskScheduler, TaskState, TaskTrigger, TaskUser,
        install_task_script, parse_status_output, status_task_script, stop_task_script,
        uninstall_task_script,
    };

    #[derive(Default)]
    struct FakeTaskScheduler {
        status: Option<ServiceStatus>,
        reject_install: bool,
        reject_stop: bool,
    }

    impl TaskScheduler for FakeTaskScheduler {
        fn stop(&mut self, _spec: &ServiceSpec) -> Result<()> {
            if self.reject_stop {
                bail!("injected task stop failure");
            }
            if let Some(status) = &mut self.status {
                status.task_state = TaskState::Ready;
                status.process_running = false;
            }
            Ok(())
        }

        fn install(&mut self, _spec: &ServiceSpec) -> Result<()> {
            if self.reject_install {
                bail!("injected task registration failure");
            }
            if self
                .status
                .as_ref()
                .is_some_and(|status| status.process_running)
            {
                bail!("cannot register while installed process is running");
            }
            self.status = Some(ServiceStatus {
                task_state: TaskState::Ready,
                process_running: false,
            });
            Ok(())
        }

        fn uninstall(&mut self, _task_name: &str) -> Result<()> {
            if self
                .status
                .as_ref()
                .is_some_and(|status| status.process_running)
            {
                bail!("cannot unregister while installed process is running");
            }
            self.status = None;
            Ok(())
        }

        fn status(&mut self, _task_name: &str, _binary_path: &Path) -> Result<ServiceStatus> {
            Ok(self.status.clone().unwrap_or(ServiceStatus {
                task_state: TaskState::Absent,
                process_running: false,
            }))
        }
    }

    fn service_fixture() -> (TempDir, ServiceRoot, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let local_app_data = temp.path().join("LocalAppData");
        let source = temp.path().join("source-screenpipe.exe");
        fs::write(&source, b"screenpipe-test-binary").unwrap();
        (temp, ServiceRoot::for_test(local_app_data), source)
    }

    /// Sorted recursive `relative-path=contents` listing of `root`.
    ///
    /// A test that only re-reads the one file it expects to be protected cannot
    /// see a write that lands somewhere else in the tree, so failure paths that
    /// must be filesystem-inert compare a whole snapshot instead.
    fn directory_snapshot(root: &Path) -> Vec<String> {
        fn walk(root: &Path, directory: &Path, entries: &mut Vec<String>) {
            let Ok(children) = fs::read_dir(directory) else {
                return;
            };
            for child in children {
                let child = child.unwrap();
                let path = child.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if child.file_type().unwrap().is_dir() {
                    entries.push(format!("{relative}\\"));
                    walk(root, &path, entries);
                } else {
                    entries.push(format!(
                        "{relative}={}",
                        String::from_utf8_lossy(&fs::read(&path).unwrap())
                    ));
                }
            }
        }

        let mut entries = Vec::new();
        walk(root, root, &mut entries);
        entries.sort();
        entries
    }

    /// Parse-error messages Windows PowerShell reports for `script`.
    ///
    /// Parsing is the only cheap way to prove a generated script is not
    /// silently broken by quoting, so both the wrapper and the scheduler
    /// scripts go through it.
    #[cfg(windows)]
    fn powershell_parse_errors(script: &str) -> Vec<String> {
        let parser_script = r#"
$tokens = $null
$parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseInput(
    $env:SCREENPIPE_SCRIPT_TO_PARSE,
    [ref]$tokens,
    [ref]$parseErrors
) | Out-Null
[Console]::Out.Write((($parseErrors | ForEach-Object Message) -join [Environment]::NewLine))
"#;

        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", parser_script])
            .env("SCREENPIPE_SCRIPT_TO_PARSE", script)
            .output()
            .expect("Windows PowerShell should be available for syntax validation");
        assert!(
            output.status.success(),
            "PowerShell parser failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|message| !message.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[cfg(windows)]
    fn create_junction(link: &Path, target: &Path) {
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:SCREENPIPE_TEST_LINK -Target $env:SCREENPIPE_TEST_TARGET | Out-Null",
            ])
            .env("SCREENPIPE_TEST_LINK", link)
            .env("SCREENPIPE_TEST_TARGET", target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    static LOCAL_APP_DATA_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    #[cfg(windows)]
    fn current_user_service_root_ignores_localappdata_environment_override() {
        let _lock = LOCAL_APP_DATA_ENV_LOCK.lock().unwrap();
        let override_root = tempfile::tempdir().unwrap();
        let original = std::env::var_os("LOCALAPPDATA");
        unsafe { std::env::set_var("LOCALAPPDATA", override_root.path()) };

        let actual = ServiceRoot::current_user().unwrap();

        match original {
            Some(value) => unsafe { std::env::set_var("LOCALAPPDATA", value) },
            None => unsafe { std::env::remove_var("LOCALAPPDATA") },
        }
        assert!(!actual.local_app_data.starts_with(override_root.path()));
        // Ignoring the override is not the same as resolving the right folder:
        // the Roaming known folder also fails the check above, and installing
        // there ships the copied binary and the wrapper into a profile that
        // roams between machines and is synced by folder redirection.
        assert!(
            actual
                .local_app_data
                .ends_with(Path::new("AppData").join("Local")),
            "resolved service root {} is not the Local known folder",
            actual.local_app_data.display()
        );
        assert!(
            !actual
                .local_app_data
                .components()
                .any(|component| component.as_os_str().eq_ignore_ascii_case("Roaming")),
            "resolved service root {} must never sit under Roaming",
            actual.local_app_data.display()
        );
    }

    #[test]
    #[cfg(windows)]
    fn install_rejects_a_junction_component_without_writing_through_it() {
        let temp = tempfile::tempdir().unwrap();
        let local_app_data = temp.path().join("LocalAppData");
        let service_root = local_app_data.join("screen-memory");
        let junction_target = temp.path().join("junction-target");
        fs::create_dir_all(&service_root).unwrap();
        fs::create_dir_all(&junction_target).unwrap();
        create_junction(&service_root.join("bin"), &junction_target);
        let source = temp.path().join("source-screenpipe.exe");
        fs::write(&source, b"screenpipe-test-binary").unwrap();
        let root = ServiceRoot::for_test(local_app_data);
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());

        let error = manager.install(&root, &source).unwrap_err();

        assert!(format!("{error:#}").contains("reparse point"));
        assert!(!junction_target.join("screenpipe.exe").exists());
        assert!(!service_root.join("run-screenpipe.ps1").exists());
    }

    #[test]
    #[cfg(windows)]
    fn uninstall_rejects_a_junction_root_without_removing_target_files() {
        let temp = tempfile::tempdir().unwrap();
        let local_app_data = temp.path().join("LocalAppData");
        let junction_target = temp.path().join("junction-target");
        let binary = junction_target.join(r"bin\screenpipe.exe");
        let wrapper = junction_target.join("run-screenpipe.ps1");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"preserve-binary").unwrap();
        fs::write(&wrapper, b"preserve-wrapper").unwrap();
        fs::create_dir_all(&local_app_data).unwrap();
        create_junction(&local_app_data.join("screen-memory"), &junction_target);
        let root = ServiceRoot::for_test(local_app_data);
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());

        let error = manager.uninstall(&root).unwrap_err();

        assert!(format!("{error:#}").contains("reparse point"));
        assert_eq!(fs::read(binary).unwrap(), b"preserve-binary");
        assert_eq!(fs::read(wrapper).unwrap(), b"preserve-wrapper");
    }

    #[test]
    fn service_spec_targets_the_interactive_user_task() {
        let root = ServiceRoot::for_test(Path::new(r"C:\Users\pmacl\AppData\Local").to_owned());
        let spec = ServiceSpec::for_current_user(&root);
        assert_eq!(spec.task_name, "MooseGoose Screen Memory");
        assert_eq!(
            spec.action,
            TaskAction {
                executable: "powershell.exe",
                arguments: r#"-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "C:\Users\pmacl\AppData\Local\screen-memory\run-screenpipe.ps1""#
                    .to_owned(),
            }
        );
        assert_eq!(spec.trigger, TaskTrigger::UserLogon);
        assert_eq!(
            spec.principal,
            TaskPrincipal {
                user: TaskUser::CurrentUser,
                logon_type: TaskLogonType::InteractiveToken,
                run_level: TaskRunLevel::Limited,
            }
        );
    }

    #[test]
    fn service_spec_uses_accessible_workstation_doppler_namespace_and_keeps_local_artifacts_safe() {
        let local_app_data = Path::new(r"C:\Users\pmacl\AppData\Local");
        let root = ServiceRoot::for_test(local_app_data.to_owned());
        let spec = ServiceSpec::for_current_user(&root);
        let expected_root = local_app_data.join("screen-memory");

        assert_eq!(spec.root_path, expected_root);
        assert_eq!(spec.binary_path, expected_root.join(r"bin\screenpipe.exe"));
        assert_eq!(spec.wrapper_path, expected_root.join("run-screenpipe.ps1"));
        assert!(spec.binary_path.starts_with(&expected_root));
        assert!(spec.wrapper_path.starts_with(&expected_root));
        assert_eq!(
            spec.wrapper_contents,
            concat!(
                "$ErrorActionPreference = 'Continue'\n",
                "$agent = 'C:\\Users\\pmacl\\AppData\\Local\\screen-memory\\bin\\screenpipe.exe'\n",
                "$logDirectory = 'C:\\Users\\pmacl\\AppData\\Local\\screen-memory\\logs'\n",
                "if (-not (Test-Path -LiteralPath $logDirectory)) {\n",
                "    New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null\n",
                "}\n",
                "while ($true) {\n",
                "    $log = Join-Path $logDirectory ('screenpipe-agent-{0:yyyy-MM-dd}.log' -f (Get-Date))\n",
                "    \"=== agent start {0:o} ===\" -f (Get-Date).ToUniversalTime() | Out-File -LiteralPath $log -Append -Encoding utf8\n",
                "    doppler run -p homelab -c dev_personal -- $agent run --machine-slug icarus --display-name Icarus-Laptop *>> $log\n",
                "    \"=== agent exited {0:o} exit={1} ===\" -f (Get-Date).ToUniversalTime(), $LASTEXITCODE | Out-File -LiteralPath $log -Append -Encoding utf8\n",
                "    Start-Sleep -Seconds 10\n",
                "}\n",
            )
        );
        assert_eq!(
            spec.wrapper_contents
                .matches("doppler run -p homelab -c dev_personal --")
                .count(),
            1
        );
        assert!(!spec.wrapper_contents.contains("doppler run -p apps-data"));
        assert!(
            !spec
                .wrapper_contents
                .contains("doppler run -p screen-memory")
        );
        assert!(!spec.wrapper_contents.contains("SCREEN_MEMORY_DATABASE_URL"));
        assert!(!spec.wrapper_contents.contains("postgresql://"));
        assert!(!spec.action.arguments.contains("SCREEN_MEMORY_DATABASE_URL"));
        assert!(!spec.action.arguments.contains("postgresql://"));
    }

    #[test]
    #[cfg(windows)]
    fn generated_wrapper_has_no_powershell_parse_errors() {
        // The second root is the case the escaper exists for: an apostrophe in
        // the user folder closes the single-quoted $agent literal early and the
        // rest of the wrapper stops being a parseable script.
        for local_app_data in [
            r"C:\Users\pmacl\AppData\Local",
            r"C:\Users\O'Brien\AppData\Local",
        ] {
            let root = ServiceRoot::for_test(Path::new(local_app_data).to_owned());
            let spec = ServiceSpec::for_current_user(&root);

            let errors = powershell_parse_errors(&spec.wrapper_contents);

            assert!(
                errors.is_empty(),
                "wrapper generated for {local_app_data} does not parse: {errors:?}"
            );
        }
    }

    #[test]
    fn wrapper_doubles_an_apostrophe_in_the_service_root_path() {
        // Every other fixture uses C:\Users\pmacl, so nothing ever fed the
        // escaper a quote. A real user folder such as C:\Users\O'Brien would
        // terminate the single-quoted literal mid-path, and the task would
        // launch a wrapper that dies on a syntax error at every logon instead
        // of starting the agent.
        let root = ServiceRoot::for_test(Path::new(r"C:\Users\O'Brien\AppData\Local").to_owned());
        let spec = ServiceSpec::for_current_user(&root);

        let agent_line = spec
            .wrapper_contents
            .lines()
            .find(|line| line.starts_with("$agent = "))
            .expect("wrapper must assign the agent path");

        assert!(
            agent_line.contains(r"O''Brien"),
            "apostrophe was not doubled: {agent_line}"
        );
        assert_eq!(
            agent_line,
            r"$agent = 'C:\Users\O''Brien\AppData\Local\screen-memory\bin\screenpipe.exe'"
        );
        // Every interpolated path shares the escaper, so the balance check
        // covers the whole wrapper rather than only the agent assignment.
        for line in spec.wrapper_contents.lines() {
            assert_eq!(
                line.matches('\'').count() % 2,
                0,
                "single-quoted literal is unbalanced: {line}"
            );
        }
    }

    #[test]
    fn install_materializes_the_exact_service_and_returns_scheduler_status() {
        let (_temp, local_app_data, source) = service_fixture();
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());

        let status = manager.install(&local_app_data, &source).unwrap();
        let spec = ServiceSpec::for_current_user(&local_app_data);

        assert_eq!(
            fs::read(&spec.binary_path).unwrap(),
            b"screenpipe-test-binary"
        );
        assert_eq!(
            fs::read_to_string(&spec.wrapper_path).unwrap(),
            spec.wrapper_contents
        );
        assert_eq!(
            status,
            ServiceStatus {
                task_state: TaskState::Ready,
                process_running: false,
            }
        );
        assert_eq!(manager.status(&local_app_data).unwrap(), status);
    }

    #[test]
    fn reinstall_repairs_owned_artifacts_without_touching_siblings() {
        let (_temp, local_app_data, source) = service_fixture();
        let spec = ServiceSpec::for_current_user(&local_app_data);
        fs::create_dir_all(spec.binary_path.parent().unwrap()).unwrap();
        fs::write(&spec.binary_path, b"stale-binary").unwrap();
        fs::write(&spec.wrapper_path, "stale-wrapper").unwrap();
        let sibling = spec.root_path.join("preserve-me.txt");
        fs::write(&sibling, "owned by another checkpoint").unwrap();
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());

        manager.install(&local_app_data, &source).unwrap();
        manager.install(&local_app_data, &source).unwrap();

        assert_eq!(
            fs::read(&spec.binary_path).unwrap(),
            b"screenpipe-test-binary"
        );
        assert_eq!(
            fs::read_to_string(&spec.wrapper_path).unwrap(),
            spec.wrapper_contents
        );
        assert_eq!(
            fs::read_to_string(sibling).unwrap(),
            "owned by another checkpoint"
        );
    }

    #[test]
    fn install_from_the_already_copied_binary_does_not_truncate_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = ServiceRoot::for_test(temp.path().join("LocalAppData"));
        let spec = ServiceSpec::for_current_user(&root);
        fs::create_dir_all(spec.binary_path.parent().unwrap()).unwrap();
        fs::write(&spec.binary_path, b"running-installed-binary").unwrap();
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());

        manager.install(&root, &spec.binary_path).unwrap();

        assert_eq!(
            fs::read(&spec.binary_path).unwrap(),
            b"running-installed-binary"
        );
    }

    #[test]
    fn uninstall_removes_only_the_exact_task_binary_and_wrapper() {
        let (_temp, local_app_data, source) = service_fixture();
        let spec = ServiceSpec::for_current_user(&local_app_data);
        let mut manager = ServiceManager::new(FakeTaskScheduler::default());
        manager.install(&local_app_data, &source).unwrap();
        let sibling = spec.root_path.join("preserve-me.txt");
        let log = spec.root_path.join(r"logs\postgresql.log");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&sibling, "preserve").unwrap();
        fs::write(&log, "preserve").unwrap();

        let status = manager.uninstall(&local_app_data).unwrap();

        assert!(!spec.binary_path.exists());
        assert!(!spec.wrapper_path.exists());
        assert_eq!(fs::read_to_string(sibling).unwrap(), "preserve");
        assert_eq!(fs::read_to_string(log).unwrap(), "preserve");
        assert_eq!(
            status,
            ServiceStatus {
                task_state: TaskState::Absent,
                process_running: false,
            }
        );
    }

    #[test]
    fn task_registration_failure_is_returned_and_never_fabricates_ready_status() {
        let (_temp, local_app_data, source) = service_fixture();
        let scheduler = FakeTaskScheduler {
            reject_install: true,
            ..FakeTaskScheduler::default()
        };
        let mut manager = ServiceManager::new(scheduler);

        let error = manager.install(&local_app_data, &source).unwrap_err();

        assert!(
            format!("{error:#}").contains("injected task registration failure"),
            "scheduler cause must remain available through service context"
        );
        assert_eq!(
            manager.status(&local_app_data).unwrap().task_state,
            TaskState::Absent
        );
    }

    #[test]
    fn stop_failure_prevents_reinstall_from_mutating_the_installed_binary() {
        let (_temp, local_app_data, source) = service_fixture();
        let spec = ServiceSpec::for_current_user(&local_app_data);
        fs::create_dir_all(spec.binary_path.parent().unwrap()).unwrap();
        fs::write(&spec.binary_path, b"still-running-binary").unwrap();
        let before = directory_snapshot(&spec.root_path);
        let scheduler = FakeTaskScheduler {
            status: Some(ServiceStatus {
                task_state: TaskState::Running,
                process_running: true,
            }),
            reject_stop: true,
            ..FakeTaskScheduler::default()
        };
        let mut manager = ServiceManager::new(scheduler);

        let error = manager.install(&local_app_data, &source).unwrap_err();

        assert!(format!("{error:#}").contains("injected task stop failure"));
        assert_eq!(
            fs::read(&spec.binary_path).unwrap(),
            b"still-running-binary"
        );
        // A stop that failed means the previous instance may still be running
        // and holding these paths, so install must be entirely inert until the
        // process tree is confirmed dead - not just leave the binary alone.
        // Re-reading one file cannot see a wrapper, a directory, or a leftover
        // `.installing` temp file written before the failing stop, so the whole
        // root is compared instead.
        assert!(
            !spec.wrapper_path.exists(),
            "a wrapper was written before the stop succeeded"
        );
        let after = directory_snapshot(&spec.root_path);
        assert!(
            !after.iter().any(|entry| entry.contains(".installing")),
            "a half-written install temp file was left behind: {after:?}"
        );
        assert_eq!(
            after, before,
            "a failed stop must leave the service root untouched"
        );
    }

    #[test]
    fn reinstall_stops_an_active_installation_before_replacing_artifacts() {
        let (_temp, local_app_data, source) = service_fixture();
        let scheduler = FakeTaskScheduler {
            status: Some(ServiceStatus {
                task_state: TaskState::Running,
                process_running: true,
            }),
            ..FakeTaskScheduler::default()
        };
        let mut manager = ServiceManager::new(scheduler);

        let status = manager.install(&local_app_data, &source).unwrap();

        assert_eq!(status.task_state, TaskState::Ready);
        assert!(!status.process_running);
    }

    #[test]
    fn uninstall_stops_an_active_installation_before_deleting_artifacts() {
        let (_temp, local_app_data, _source) = service_fixture();
        let spec = ServiceSpec::for_current_user(&local_app_data);
        fs::create_dir_all(spec.binary_path.parent().unwrap()).unwrap();
        fs::write(&spec.binary_path, b"running-installed-binary").unwrap();
        fs::write(&spec.wrapper_path, &spec.wrapper_contents).unwrap();
        let scheduler = FakeTaskScheduler {
            status: Some(ServiceStatus {
                task_state: TaskState::Running,
                process_running: true,
            }),
            ..FakeTaskScheduler::default()
        };
        let mut manager = ServiceManager::new(scheduler);

        let status = manager.uninstall(&local_app_data).unwrap();

        assert_eq!(status.task_state, TaskState::Absent);
        assert!(!status.process_running);
        assert!(!spec.binary_path.exists());
        assert!(!spec.wrapper_path.exists());
    }

    #[test]
    fn scheduler_status_parser_preserves_ready_running_and_absent_states() {
        assert_eq!(
            parse_status_output("Ready|False\r\n").unwrap(),
            ServiceStatus {
                task_state: TaskState::Ready,
                process_running: false,
            }
        );
        assert_eq!(
            parse_status_output("Running|True\n").unwrap(),
            ServiceStatus {
                task_state: TaskState::Running,
                process_running: true,
            }
        );
        assert_eq!(
            parse_status_output("Absent|False").unwrap(),
            ServiceStatus {
                task_state: TaskState::Absent,
                process_running: false,
            }
        );
        assert_eq!(
            parse_status_output("Disabled|False").unwrap(),
            ServiceStatus {
                task_state: TaskState::Other("Disabled".to_owned()),
                process_running: false,
            }
        );
    }

    #[test]
    fn scheduler_status_parser_rejects_ambiguous_or_malformed_output() {
        // Every case must reach the branch it is meant to exercise. The first
        // four all fail on the process field or the field count, so nothing
        // covered the empty-state branch: a scheduler that emits "|False"
        // would otherwise be reported as a task in state "" rather than as
        // output we refuse to trust.
        for invalid in [
            "",
            "Ready",
            "Ready|maybe",
            "Ready|False|extra",
            "|False",
            "  |False",
        ] {
            assert!(
                parse_status_output(invalid).is_err(),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn task_scheduler_scripts_have_no_powershell_parse_errors() {
        let scripts = [
            install_task_script(),
            stop_task_script(),
            uninstall_task_script(),
            status_task_script(),
        ];

        for script in scripts {
            let errors = powershell_parse_errors(script);

            assert!(errors.is_empty(), "script does not parse: {errors:?}");
        }
    }

    #[test]
    fn install_script_registers_a_limited_logon_task_not_an_elevated_startup_one() {
        // `ParseInput` does not resolve cmdlet parameter sets, so parsing the
        // script proves nothing about which switches it passes, and the
        // ServiceSpec principal is only a Rust-side mirror that
        // Register-ScheduledTask never sees. This string is what actually
        // registers the task. `Highest` would hand a capture loop an elevated
        // token it has no use for, and `-AtStartup` would run it in session 0
        // where there is no foreground window to capture at all.
        let script = install_task_script();

        assert!(script.contains("-RunLevel Limited"), "{script}");
        assert!(script.contains("-LogonType Interactive"), "{script}");
        assert!(
            script.contains("New-ScheduledTaskTrigger -AtLogOn"),
            "{script}"
        );
        assert!(!script.contains("-RunLevel Highest"), "{script}");
        assert!(!script.contains("-AtStartup"), "{script}");
    }

    #[test]
    #[cfg(windows)]
    fn stop_script_does_not_terminate_a_decoy_that_only_mentions_the_wrapper_path() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("run-screenpipe.ps1");
        let binary = temp.path().join(r"bin\screenpipe.exe");
        let mut decoy = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &format!("Start-Sleep -Seconds 30 # {}", wrapper.display()),
            ])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let result = super::run_powershell(
            stop_task_script(),
            &[
                (
                    "SCREENPIPE_TASK_NAME",
                    "MooseGoose Goal 1 Missing Negative Control".to_owned(),
                ),
                (
                    "SCREENPIPE_SERVICE_BINARY",
                    binary.to_string_lossy().into_owned(),
                ),
                (
                    "SCREENPIPE_SERVICE_WRAPPER",
                    wrapper.to_string_lossy().into_owned(),
                ),
            ],
        );
        let decoy_state = decoy.try_wait().unwrap();
        if decoy_state.is_none() {
            decoy.kill().unwrap();
            decoy.wait().unwrap();
        }

        result.unwrap();
        assert!(
            decoy_state.is_none(),
            "stop script terminated an unrelated process"
        );
    }

    /// Spawn a real, long-lived process whose executable path is exactly
    /// `path`. `ping` is used because it runs for a controllable duration
    /// without needing a console, stdin, or a window.
    #[cfg(test)]
    fn spawn_process_at(path: &std::path::Path) -> std::process::Child {
        std::fs::create_dir_all(path.parent().expect("parent")).unwrap();
        let system_ping = std::path::Path::new(&std::env::var("SystemRoot").unwrap())
            .join(r"System32\PING.EXE");
        std::fs::copy(&system_ping, path).unwrap();
        Command::new(path)
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn stop_script_terminates_the_exact_installed_binary_and_spares_near_misses() {
        // The decoy test above is a pure NEGATIVE control: it only proves the
        // script spares something. A matcher that owns nothing at all - or one
        // that prefix-matches and would over-kill - passes it trivially, and
        // both of those mutations were empirically confirmed to survive the
        // whole suite. Terminating the right process is the entire purpose of
        // this script, and nothing asserted it.
        //
        // `owned` must die. The two near misses must live:
        //   - `screenpipe.exe.bak` is a strict PREFIX extension of the owned
        //     path, so a `StartsWith`/`-like "$binary*"` matcher kills it.
        //   - `bin2\screenpipe.exe` shares the file name under a sibling
        //     directory, so a file-name-only matcher kills it.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let owned_path = root.join(r"bin\screenpipe.exe");
        let prefix_path = root.join(r"bin\screenpipe.exe.bak");
        let sibling_path = root.join(r"bin2\screenpipe.exe");

        let mut owned = spawn_process_at(&owned_path);
        let mut prefix_decoy = spawn_process_at(&prefix_path);
        let mut sibling_decoy = spawn_process_at(&sibling_path);
        std::thread::sleep(std::time::Duration::from_millis(900));

        let result = super::run_powershell(
            stop_task_script(),
            &[
                (
                    "SCREENPIPE_TASK_NAME",
                    "MooseGoose Goal 1 Missing Positive Control".to_owned(),
                ),
                (
                    "SCREENPIPE_SERVICE_BINARY",
                    owned_path.to_string_lossy().into_owned(),
                ),
            ],
        );

        // Sample every process's state before any cleanup, so the assertions
        // below describe the state the script actually left behind.
        let owned_state = owned.try_wait().unwrap();
        let prefix_state = prefix_decoy.try_wait().unwrap();
        let sibling_state = sibling_decoy.try_wait().unwrap();
        for child in [&mut owned, &mut prefix_decoy, &mut sibling_decoy] {
            let _ = child.kill();
            let _ = child.wait();
        }

        result.expect("stop script failed");
        assert!(
            owned_state.is_some(),
            "the stop script did not terminate the exact installed binary - \
             its whole purpose is unperformed"
        );
        assert!(
            prefix_state.is_none(),
            "the stop script killed screenpipe.exe.bak: it is prefix-matching \
             the binary path instead of comparing it exactly"
        );
        assert!(
            sibling_state.is_none(),
            "the stop script killed bin2\\screenpipe.exe: it is matching on file \
             name instead of the full executable path"
        );
    }
}
