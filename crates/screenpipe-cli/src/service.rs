use std::path::{Path, PathBuf};

pub(crate) struct ServiceSpec {
    pub(crate) task_name: &'static str,
    pub(crate) root_path: PathBuf,
    pub(crate) binary_path: PathBuf,
    pub(crate) wrapper_path: PathBuf,
    pub(crate) task_arguments: String,
    pub(crate) wrapper_contents: String,
}

impl ServiceSpec {
    pub(crate) fn for_current_user(local_app_data: &Path) -> Self {
        let root_path = local_app_data.join("screen-memory");
        let binary_path = root_path.join(r"bin\screenpipe.exe");
        let wrapper_path = root_path.join("run-screenpipe.ps1");
        let task_arguments = format!(
            r#"-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "{}""#,
            wrapper_path.display()
        );
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
            task_arguments,
            wrapper_contents,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ServiceSpec;

    #[test]
    fn service_spec_targets_the_interactive_user_task() {
        let spec = ServiceSpec::for_current_user(Path::new(r"C:\Users\pmacl\AppData\Local"));
        assert_eq!(spec.task_name, "MooseGoose Screen Memory");
        assert!(
            spec.wrapper_path
                .ends_with(r"screen-memory\run-screenpipe.ps1")
        );
        assert!(!spec.wrapper_contents.contains("postgresql://"));
        assert!(
            spec.wrapper_contents
                .contains("doppler run -p screen-memory -c dev")
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
        assert!(spec.task_arguments.contains("-WindowStyle Hidden"));

        let wrapper = &spec.wrapper_contents;
        assert!(wrapper.contains("$env:LOCALAPPDATA"));
        assert!(wrapper.contains("--machine-slug icarus"));
        assert!(wrapper.contains("--display-name Icarus-Laptop"));
        assert!(wrapper.contains("Start-Sleep -Seconds 10"));
        assert!(!wrapper.contains("SCREEN_MEMORY_DATABASE_URL"));
        assert!(!wrapper.contains("postgresql://"));
    }
}
