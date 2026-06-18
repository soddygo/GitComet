//! Apply a downloaded GitComet update and restart the application.
//!
//! Invoked via the hidden `gitcomet apply-update` CLI subcommand after the main
//! GUI process downloads and extracts a release archive.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const WAIT_PID_TIMEOUT: Duration = Duration::from_secs(3600);
#[cfg(target_os = "windows")]
const WINDOWS_REEXEC_ENV: &str = "GITCOMET_UPDATER_REEXEC";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyUpdateRequest {
    pub wait_pid: u32,
    pub target_exe: PathBuf,
    pub new_binary: PathBuf,
    pub staging_dir: PathBuf,
    pub app_bundle: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum ApplyUpdateError {
    #[error("failed waiting for process {0}: {1}")]
    WaitPid(u32, String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("apply update failed: {0}")]
    Apply(String),
}

impl ApplyUpdateRequest {
    pub fn run(self) -> Result<(), ApplyUpdateError> {
        #[cfg(target_os = "windows")]
        if std::env::var_os(WINDOWS_REEXEC_ENV).is_none() {
            return self.reexec_from_temp();
        }

        wait_for_process_exit(self.wait_pid)?;
        let used_bundle_replace = self.app_bundle.is_some();
        if let Some(new_bundle) = &self.app_bundle {
            replace_macos_app_bundle(&self.target_exe, new_bundle)?;
        } else {
            replace_standalone_binary(&self.target_exe, &self.new_binary)?;
        }
        restart_application(&self.target_exe, used_bundle_replace)?;
        cleanup_staging(&self.staging_dir)?;
        Ok(())
    }

    pub fn spawn_updater(&self, updater_exe: &Path) -> io::Result<Child> {
        let mut command = Command::new(updater_exe);
        command
            .arg("apply-update")
            .arg("--wait-pid")
            .arg(self.wait_pid.to_string())
            .arg("--target-exe")
            .arg(&self.target_exe)
            .arg("--new-binary")
            .arg(&self.new_binary)
            .arg("--staging-dir")
            .arg(&self.staging_dir);
        if let Some(bundle) = &self.app_bundle {
            command.arg("--app-bundle").arg(bundle);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    #[cfg(test)]
    pub(crate) fn apply_update_cli_args(&self) -> Vec<String> {
        let mut args = vec![
            "apply-update".to_string(),
            "--wait-pid".to_string(),
            self.wait_pid.to_string(),
            "--target-exe".to_string(),
            self.target_exe.display().to_string(),
            "--new-binary".to_string(),
            self.new_binary.display().to_string(),
            "--staging-dir".to_string(),
            self.staging_dir.display().to_string(),
        ];
        if let Some(bundle) = &self.app_bundle {
            args.push("--app-bundle".to_string());
            args.push(bundle.display().to_string());
        }
        args
    }

    #[cfg(target_os = "windows")]
    fn reexec_from_temp(self) -> Result<(), ApplyUpdateError> {
        let current_exe = std::env::current_exe()?;
        let temp_exe = std::env::temp_dir().join(format!(
            "gitcomet-updater-{}.exe",
            std::process::id()
        ));
        fs::copy(&current_exe, &temp_exe)?;
        let mut command = Command::new(&temp_exe);
        command.env(WINDOWS_REEXEC_ENV, "1");
        command
            .arg("apply-update")
            .arg("--wait-pid")
            .arg(self.wait_pid.to_string())
            .arg("--target-exe")
            .arg(&self.target_exe)
            .arg("--new-binary")
            .arg(&self.new_binary)
            .arg("--staging-dir")
            .arg(&self.staging_dir);
        if let Some(bundle) = &self.app_bundle {
            command.arg("--app-bundle").arg(bundle);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(ApplyUpdateError::Io)?;
        Ok(())
    }
}

/// Resolve the `.app` bundle root for an executable inside `Contents/MacOS/`.
pub fn macos_app_bundle_root(exe: &Path) -> Option<PathBuf> {
    if !exe
        .to_string_lossy()
        .contains(".app/Contents/MacOS/")
    {
        return None;
    }
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let bundle = contents.parent()?;
    Some(bundle.to_path_buf())
}

fn wait_for_process_exit(pid: u32) -> Result<(), ApplyUpdateError> {
    let deadline = Instant::now() + WAIT_PID_TIMEOUT;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return Err(ApplyUpdateError::WaitPid(
                pid,
                "timed out waiting for main process to exit".to_string(),
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match output {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                text.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
}

fn replace_macos_app_bundle(
    target_exe: &Path,
    new_bundle: &Path,
) -> Result<(), ApplyUpdateError> {
    let current_bundle = macos_app_bundle_root(target_exe).ok_or_else(|| {
        ApplyUpdateError::Apply("target executable is not inside a macOS app bundle".to_string())
    })?;
    if current_bundle.exists() {
        fs::remove_dir_all(&current_bundle).map_err(ApplyUpdateError::Io)?;
    }
    if let Some(parent) = current_bundle.parent() {
        fs::create_dir_all(parent).map_err(ApplyUpdateError::Io)?;
    }
    copy_dir_all(new_bundle, &current_bundle)?;
    #[cfg(target_os = "macos")]
    adhoc_codesign_path(&current_bundle)?;
    Ok(())
}

fn replace_standalone_binary(target: &Path, new_binary: &Path) -> Result<(), ApplyUpdateError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(ApplyUpdateError::Io)?;
    }
    fs::copy(new_binary, target).map_err(ApplyUpdateError::Io)?;
    #[cfg(unix)]
    {
        let permissions = fs::metadata(new_binary)
            .map_err(ApplyUpdateError::Io)?
            .permissions();
        fs::set_permissions(target, permissions).map_err(ApplyUpdateError::Io)?;
    }
    #[cfg(target_os = "macos")]
    adhoc_codesign_path(target)?;
    Ok(())
}

fn restart_application(
    target_exe: &Path,
    used_bundle_replace: bool,
) -> Result<(), ApplyUpdateError> {
    if used_bundle_replace {
        #[cfg(target_os = "macos")]
        if let Some(bundle) = macos_app_bundle_root(target_exe) {
            Command::new("open")
                .arg(bundle)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(ApplyUpdateError::Io)?;
            return Ok(());
        }
    }

    Command::new(target_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ApplyUpdateError::Io)?;
    Ok(())
}

fn cleanup_staging(staging_dir: &Path) -> Result<(), ApplyUpdateError> {
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir).map_err(ApplyUpdateError::Io)?;
    }
    Ok(())
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), ApplyUpdateError> {
    fs::create_dir_all(destination).map_err(ApplyUpdateError::Io)?;
    for entry in fs::read_dir(source).map_err(ApplyUpdateError::Io)? {
        let entry = entry.map_err(ApplyUpdateError::Io)?;
        let file_type = entry.file_type().map_err(ApplyUpdateError::Io)?;
        let target_path = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target_path)?;
        } else {
            fs::copy(entry.path(), &target_path).map_err(ApplyUpdateError::Io)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn adhoc_codesign_path(path: &Path) -> Result<(), ApplyUpdateError> {
    let status = Command::new("codesign")
        .args([
            "--force",
            "--sign",
            "-",
            &path.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(ApplyUpdateError::Io)?;
    if status.success() {
        Ok(())
    } else {
        // Ad-hoc signing is best-effort; the old shell updater ignored failures too.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_app_bundle_root_resolves_three_levels_up_from_macos_dir() {
        let exe = Path::new(
            "/Applications/GitComet.app/Contents/MacOS/gitcomet",
        );
        assert_eq!(
            macos_app_bundle_root(exe),
            Some(PathBuf::from("/Applications/GitComet.app"))
        );
    }

    #[test]
    fn macos_app_bundle_root_returns_none_for_standalone_binary() {
        let exe = Path::new("/usr/local/bin/gitcomet");
        assert_eq!(macos_app_bundle_root(exe), None);
    }

    #[test]
    fn apply_update_cli_args_include_expected_flags() {
        let request = ApplyUpdateRequest {
            wait_pid: 42,
            target_exe: PathBuf::from("/tmp/gitcomet"),
            new_binary: PathBuf::from("/tmp/staging/gitcomet"),
            staging_dir: PathBuf::from("/tmp/staging"),
            app_bundle: Some(PathBuf::from("/tmp/staging/GitComet.app")),
        };
        assert_eq!(
            request.apply_update_cli_args(),
            vec![
                "apply-update".to_string(),
                "--wait-pid".to_string(),
                "42".to_string(),
                "--target-exe".to_string(),
                "/tmp/gitcomet".to_string(),
                "--new-binary".to_string(),
                "/tmp/staging/gitcomet".to_string(),
                "--staging-dir".to_string(),
                "/tmp/staging".to_string(),
                "--app-bundle".to_string(),
                "/tmp/staging/GitComet.app".to_string(),
            ]
        );
    }
}
