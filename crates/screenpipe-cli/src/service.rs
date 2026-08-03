use std::path::{Path, PathBuf};

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
    pub(crate) fn for_current_user(local_app_data: &Path) -> Self {
        let root_path = local_app_data.join("screen-memory");
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
        let wrapper_contents = r#"$ErrorActionPreference = 'Continue'
$agent = Join-Path $env:LOCALAPPDATA 'screen-memory\bin\screenpipe.exe'
while ($true) {
    doppler run -p screen-memory -c dev -- $agent run --machine-slug icarus --display-name Icarus-Laptop
    Start-Sleep -Seconds 10
}
"#
        .to_owned();

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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{
        ServiceSpec, TaskAction, TaskLogonType, TaskPrincipal, TaskRunLevel, TaskTrigger, TaskUser,
    };

    #[test]
    fn service_spec_targets_the_interactive_user_task() {
        let spec = ServiceSpec::for_current_user(Path::new(r"C:\Users\pmacl\AppData\Local"));
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
    fn service_spec_keeps_artifacts_and_launch_settings_safe() {
        let local_app_data = Path::new(r"C:\Users\pmacl\AppData\Local");
        let spec = ServiceSpec::for_current_user(local_app_data);
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
                "$agent = Join-Path $env:LOCALAPPDATA 'screen-memory\\bin\\screenpipe.exe'\n",
                "while ($true) {\n",
                "    doppler run -p screen-memory -c dev -- $agent run --machine-slug icarus --display-name Icarus-Laptop\n",
                "    Start-Sleep -Seconds 10\n",
                "}\n",
            )
        );
        assert!(!spec.wrapper_contents.contains("SCREEN_MEMORY_DATABASE_URL"));
        assert!(!spec.wrapper_contents.contains("postgresql://"));
        assert!(!spec.action.arguments.contains("SCREEN_MEMORY_DATABASE_URL"));
        assert!(!spec.action.arguments.contains("postgresql://"));
    }

    #[test]
    #[cfg(windows)]
    fn generated_wrapper_has_no_powershell_parse_errors() {
        let spec = ServiceSpec::for_current_user(Path::new(r"C:\Users\pmacl\AppData\Local"));
        let parser_script = r#"
$tokens = $null
$parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseInput(
    $env:SCREENPIPE_WRAPPER_TO_PARSE,
    [ref]$tokens,
    [ref]$parseErrors
) | Out-Null
if ($parseErrors.Count -ne 0) {
    [Console]::Error.Write(($parseErrors | ForEach-Object Message) -join [Environment]::NewLine)
    exit 1
}
[Console]::Out.Write($parseErrors.Count)
"#;

        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", parser_script])
            .env("SCREENPIPE_WRAPPER_TO_PARSE", &spec.wrapper_contents)
            .output()
            .expect("Windows PowerShell should be available for syntax validation");

        assert!(
            output.status.success(),
            "PowerShell parser failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "0");
    }
}
