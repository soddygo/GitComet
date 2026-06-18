use super::*;
use futures::AsyncReadExt;
use gitcomet_core::apply_update::ApplyUpdateRequest;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy, Request};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::fmt::{Display, Formatter};

const UPDATE_STAGING_DIR: &str = "gitcomet-update-staging";
const UPDATE_LOCK_FILE: &str = ".update-in-progress";
const UPDATE_LOCK_TTL_SECS: u64 = 600;

#[derive(Debug, Serialize, Deserialize)]
struct UpdateLock {
    pid: u32,
    started_at: u64,
}

#[derive(Debug)]
enum UpdateError {
    Http(String),
    Io(std::io::Error),
    #[cfg(target_os = "windows")]
    Zip(zip::result::ZipError),
    UnsupportedArchive(String),
    BinaryNotFound,
    UpdateInProgress,
}

impl Display for UpdateError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(msg) => write!(f, "HTTP error: {msg}"),
            Self::Io(err) => write!(f, "IO error: {err}"),
            #[cfg(target_os = "windows")]
            Self::Zip(err) => write!(f, "ZIP error: {err}"),
            Self::UnsupportedArchive(name) => write!(f, "Unsupported archive format: {name}"),
            Self::BinaryNotFound => write!(f, "Binary not found in archive"),
            Self::UpdateInProgress => write!(f, "An update is already in progress"),
        }
    }
}

/// Clean up leftover staging when no active update lock is present.
pub(in crate::view) fn cleanup_staging_if_safe() {
    let staging = update_staging_dir();
    if !staging.exists() {
        return;
    }
    if is_staging_locked(&staging) {
        return;
    }
    let _ = std::fs::remove_dir_all(&staging);
}

pub(in crate::view) fn update_staging_dir() -> PathBuf {
    // macOS: ~/Library/Caches, Linux: ~/.cache, Windows: %LOCALAPPDATA%
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    base.join(UPDATE_STAGING_DIR)
}

impl GitCometView {
    pub(in crate::view) fn begin_update_download(
        &mut self,
        download_url: String,
        target_version: String,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.update_in_progress {
            return;
        }

        let http_client = cx.http_client();
        let current_exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.push_toast(
                    components::ToastKind::Error,
                    format!("Cannot determine current executable path: {err}"),
                    cx,
                );
                return;
            }
        };

        let staging_dir = match prepare_staging_dir() {
            Ok(dir) => dir,
            Err(UpdateError::UpdateInProgress) => {
                self.push_toast(
                    components::ToastKind::Warning,
                    "An update is already in progress.".to_string(),
                    cx,
                );
                return;
            }
            Err(err) => {
                self.push_toast(
                    components::ToastKind::Error,
                    format!("Cannot create staging directory: {err}"),
                    cx,
                );
                return;
            }
        };

        self.update_in_progress = true;
        self.update_phase = super::update_service::UpdatePhase::Downloading {
            version: target_version.clone(),
        };
        self.sync_update_status_line();
        self.sync_settings_update_status(cx);

        let archive_name = archive_filename_from_url(&download_url);
        let archive_path = staging_dir.join(&archive_name);

        let mut toast_id = self.push_update_progress_toast(&target_version, cx);

        cx.spawn(
            async move |view: WeakEntity<GitCometView>, cx: &mut gpui::AsyncApp| {
                let version = target_version.clone();
                let download_result = download_update(
                    http_client,
                    &download_url,
                    &archive_path,
                    |mb_downloaded| {
                        let _ = view.update(cx, |this, cx| {
                            toast_id = this.replace_update_progress_toast(
                                toast_id,
                                &format!("Downloading update v{version}... {mb_downloaded} MB"),
                                cx,
                            );
                        });
                    },
                )
                .await;

                if let Err(err) = download_result {
                    cleanup_staging_dir(&staging_dir);
                    let _ = view.update(cx, |this, cx| {
                        this.update_in_progress = false;
                        this.update_phase = super::update_service::UpdatePhase::Failed(err.to_string());
                        this.sync_update_status_line();
                        this.sync_settings_update_status(cx);
                        this.push_toast(
                            components::ToastKind::Error,
                            format!("Update download failed: {err}"),
                            cx,
                        );
                    });
                    return;
                }

                let _ = view.update(cx, |this, cx| {
                    toast_id = this.replace_update_progress_toast(
                        toast_id,
                        &format!("Extracting update v{target_version}..."),
                        cx,
                    );
                });

                let extract_result = extract_archive(&archive_path, &staging_dir);
                let new_binary = match extract_result {
                    Ok(path) => path,
                    Err(err) => {
                        cleanup_staging_dir(&staging_dir);
                        let _ = view.update(cx, |this, cx| {
                            this.update_in_progress = false;
                            this.update_phase =
                                super::update_service::UpdatePhase::Failed(err.to_string());
                            this.sync_update_status_line();
                            this.sync_settings_update_status(cx);
                            this.push_toast(
                                components::ToastKind::Error,
                                format!("Failed to extract update: {err}"),
                                cx,
                            );
                        });
                        return;
                    }
                };

                #[cfg(target_os = "macos")]
                let app_bundle = find_app_bundle_in_dir(&staging_dir).filter(|_| {
                    gitcomet_core::apply_update::macos_app_bundle_root(&current_exe).is_some()
                });
                #[cfg(not(target_os = "macos"))]
                let app_bundle: Option<PathBuf> = None;

                let wait_pid = std::process::id();
                let request = ApplyUpdateRequest {
                    wait_pid,
                    target_exe: current_exe.clone(),
                    new_binary: new_binary.clone(),
                    staging_dir: staging_dir.clone(),
                    app_bundle,
                };

                if let Err(err) = write_update_lock(&staging_dir, wait_pid) {
                    cleanup_staging_dir(&staging_dir);
                    let _ = view.update(cx, |this, cx| {
                        this.update_in_progress = false;
                        this.update_phase =
                            super::update_service::UpdatePhase::Failed(err.to_string());
                        this.sync_update_status_line();
                        this.sync_settings_update_status(cx);
                        this.push_toast(
                            components::ToastKind::Error,
                            format!("Failed to prepare update lock: {err}"),
                            cx,
                        );
                    });
                    return;
                }

                let spawn_result = request.spawn_updater(&current_exe);
                if let Err(err) = spawn_result {
                    cleanup_staging_dir(&staging_dir);
                    let _ = view.update(cx, |this, cx| {
                        this.update_in_progress = false;
                        this.update_phase =
                            super::update_service::UpdatePhase::Failed(err.to_string());
                        this.sync_update_status_line();
                        this.sync_settings_update_status(cx);
                        this.push_toast(
                            components::ToastKind::Error,
                            format!("Failed to launch apply-update: {err}"),
                            cx,
                        );
                    });
                    return;
                }

                let _ = view.update(cx, |this, cx| {
                    this.update_phase = super::update_service::UpdatePhase::Applying;
                    this.sync_update_status_line();
                    this.sync_settings_update_status(cx);
                    this.push_toast(
                        components::ToastKind::Success,
                        "Restarting to apply update...".to_string(),
                        cx,
                    );
                });

                let _ = view.update(cx, |_this, cx| {
                    cx.quit();
                });
            },
        )
        .detach();
    }

    fn push_update_progress_toast(
        &mut self,
        target_version: &str,
        cx: &mut gpui::Context<Self>,
    ) -> u64 {
        self.toast_host.update(cx, |host, cx| {
            host.push_toast_inner(
                components::ToastKind::Warning,
                format!("Downloading update v{target_version}..."),
                Vec::new(),
                mod_helpers::ToastDismissBehavior::Remove,
                None,
                cx,
            )
        })
    }

    fn replace_update_progress_toast(
        &mut self,
        old_toast_id: u64,
        message: &str,
        cx: &mut gpui::Context<Self>,
    ) -> u64 {
        self.toast_host.update(cx, |host, cx| {
            host.remove_toast(old_toast_id, cx);
            host.push_toast_inner(
                components::ToastKind::Warning,
                message.to_string(),
                Vec::new(),
                mod_helpers::ToastDismissBehavior::Remove,
                None,
                cx,
            )
        })
    }
}

/// Prepare a clean staging directory for the update.
fn prepare_staging_dir() -> Result<PathBuf, UpdateError> {
    let dir = update_staging_dir();
    if is_staging_locked(&dir) {
        return Err(UpdateError::UpdateInProgress);
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(UpdateError::Io)?;
    }
    std::fs::create_dir_all(&dir).map_err(UpdateError::Io)?;
    Ok(dir)
}

fn is_staging_locked(staging: &Path) -> bool {
    let lock_path = staging.join(UPDATE_LOCK_FILE);
    let Ok(content) = std::fs::read_to_string(&lock_path) else {
        return false;
    };
    let Ok(lock) = serde_json::from_str::<UpdateLock>(&content) else {
        return false;
    };
    let now = current_unix_seconds();
    now.saturating_sub(lock.started_at) <= UPDATE_LOCK_TTL_SECS
}

fn write_update_lock(staging: &Path, pid: u32) -> Result<(), UpdateError> {
    let lock = UpdateLock {
        pid,
        started_at: current_unix_seconds(),
    };
    let content = serde_json::to_string(&lock).map_err(|err| {
        UpdateError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    })?;
    std::fs::write(staging.join(UPDATE_LOCK_FILE), content).map_err(UpdateError::Io)?;
    Ok(())
}

fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn cleanup_staging_dir(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
}

async fn download_update(
    http_client: Arc<dyn HttpClient>,
    download_url: &str,
    dest_path: &Path,
    mut progress_callback: impl FnMut(u64),
) -> Result<(), UpdateError> {
    let user_agent = format!("GitComet/{}", env!("CARGO_PKG_VERSION"));
    let request = Request::get(download_url)
        .header("User-Agent", user_agent)
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::empty())
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    let mut response = http_client
        .send(request)
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    if !response.status().is_success() {
        return Err(UpdateError::Http(format!(
            "HTTP {}",
            response.status()
        )));
    }

    let mut file = std::fs::File::create(dest_path).map_err(UpdateError::Io)?;
    let mut downloaded: u64 = 0;
    let mut last_reported_mb: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];

    loop {
        let n = response
            .body_mut()
            .read(&mut buf)
            .await
            .map_err(UpdateError::Io)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(UpdateError::Io)?;
        downloaded += n as u64;
        let current_mb = downloaded / (1024 * 1024);
        if current_mb > last_reported_mb {
            last_reported_mb = current_mb;
            progress_callback(current_mb);
        }
    }
    std::io::Write::flush(&mut file).map_err(UpdateError::Io)?;
    Ok(())
}

fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, UpdateError> {
    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive_path, dest_dir)
    } else if name.ends_with(".zip") {
        extract_zip(archive_path, dest_dir)
    } else {
        Err(UpdateError::UnsupportedArchive(name.to_string()))
    }
}

fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, UpdateError> {
    let file = std::fs::File::open(archive_path).map_err(UpdateError::Io)?;
    let decompressed = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decompressed);
    archive.unpack(dest_dir).map_err(UpdateError::Io)?;
    find_binary_in_dir(dest_dir)
}

#[cfg(target_os = "windows")]
fn extract_zip(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, UpdateError> {
    let file = std::fs::File::open(archive_path).map_err(UpdateError::Io)?;
    let mut archive = zip::ZipArchive::new(file).map_err(UpdateError::Zip)?;
    archive.extract(dest_dir).map_err(UpdateError::Zip)?;
    find_binary_in_dir(dest_dir)
}

#[cfg(not(target_os = "windows"))]
fn extract_zip(_archive_path: &Path, _dest_dir: &Path) -> Result<PathBuf, UpdateError> {
    Err(UpdateError::UnsupportedArchive(
        "zip not supported on this platform".to_string(),
    ))
}

fn find_binary_in_dir(dir: &Path) -> Result<PathBuf, UpdateError> {
    #[cfg(target_os = "windows")]
    let binary_name = "gitcomet.exe";
    #[cfg(not(target_os = "windows"))]
    let binary_name = "gitcomet";

    find_binary_in_dir_inner(dir, binary_name).ok_or(UpdateError::BinaryNotFound)
}

/// Recursively search for the release binary, preferring shallower matches.
fn find_binary_in_dir_inner(dir: &Path, binary_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?.filter_map(Result::ok).collect::<Vec<_>>();

    for entry in &entries {
        let path = entry.path();
        if path.is_file()
            && path.file_name().and_then(|n| n.to_str()) == Some(binary_name)
        {
            return Some(path);
        }
    }

    for entry in entries {
        let path = entry.path();
        if path.is_dir()
            && let Some(found) = find_binary_in_dir_inner(&path, binary_name)
        {
            return Some(found);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn find_app_bundle_in_dir(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|name| name == "GitComet.app")
        {
            return Some(path);
        }
        let nested = path.join("GitComet.app");
        if nested.is_dir() {
            return Some(nested);
        }
    }
    None
}

fn archive_filename_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .unwrap_or("update-archive")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(not(target_os = "windows"))]
    const BINARY_NAME: &str = "gitcomet";
    #[cfg(target_os = "windows")]
    const BINARY_NAME: &str = "gitcomet.exe";

    fn touch_binary(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, b"stub-binary").expect("write binary stub");
    }

    #[test]
    fn find_binary_in_dir_finds_direct_child() {
        let dir = TempDir::new().expect("temp dir");
        touch_binary(&dir.path().join(BINARY_NAME));

        let found = find_binary_in_dir(dir.path()).expect("binary expected");
        assert_eq!(found, dir.path().join(BINARY_NAME));
    }

    #[test]
    fn find_binary_in_dir_finds_one_level_nested_release_root() {
        let dir = TempDir::new().expect("temp dir");
        let release_root = dir.path().join("gitcomet-v0.0.0-linux-x86_64");
        touch_binary(&release_root.join(BINARY_NAME));

        let found = find_binary_in_dir(dir.path()).expect("binary expected");
        assert_eq!(found, release_root.join(BINARY_NAME));
    }

    #[test]
    fn find_binary_in_dir_finds_deeply_nested_binary() {
        let dir = TempDir::new().expect("temp dir");
        let nested = dir.path().join("release-root").join("app").join("bin");
        touch_binary(&nested.join(BINARY_NAME));

        let found = find_binary_in_dir(dir.path()).expect("binary expected");
        assert_eq!(found, nested.join(BINARY_NAME));
    }

    #[test]
    fn find_binary_in_dir_prefers_shallower_match() {
        let dir = TempDir::new().expect("temp dir");
        let release_root = dir.path().join("gitcomet-v0.0.0-macos-arm64");
        touch_binary(&release_root.join(BINARY_NAME));
        touch_binary(
            &release_root
                .join("GitComet.app")
                .join("Contents")
                .join("MacOS")
                .join(BINARY_NAME),
        );

        let found = find_binary_in_dir(dir.path()).expect("binary expected");
        assert_eq!(found, release_root.join(BINARY_NAME));
    }

    #[test]
    fn find_binary_in_dir_returns_error_when_missing() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir_all(dir.path().join("empty")).expect("create empty dir");

        assert!(matches!(
            find_binary_in_dir(dir.path()),
            Err(UpdateError::BinaryNotFound)
        ));
    }

    #[test]
    fn staging_lock_blocks_cleanup_until_expired() {
        let dir = TempDir::new().expect("temp dir");
        let staging = dir.path().to_path_buf();
        write_update_lock(&staging, std::process::id()).expect("write lock");
        assert!(is_staging_locked(&staging));

        let expired_lock = UpdateLock {
            pid: 1,
            started_at: current_unix_seconds().saturating_sub(UPDATE_LOCK_TTL_SECS + 1),
        };
        let content = serde_json::to_string(&expired_lock).expect("serialize lock");
        fs::write(staging.join(UPDATE_LOCK_FILE), content).expect("write expired lock");
        assert!(!is_staging_locked(&staging));
    }

    #[test]
    fn staging_lock_survives_after_lock_pid_exits() {
        let dir = TempDir::new().expect("temp dir");
        let staging = dir.path().to_path_buf();
        let lock = UpdateLock {
            pid: 1,
            started_at: current_unix_seconds(),
        };
        let content = serde_json::to_string(&lock).expect("serialize lock");
        fs::write(staging.join(UPDATE_LOCK_FILE), content).expect("write lock");
        assert!(is_staging_locked(&staging));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_app_bundle_in_dir_finds_bundle_at_release_root() {
        let dir = TempDir::new().expect("temp dir");
        let release_root = dir.path().join("gitcomet-v0.0.0-macos-arm64");
        let bundle = release_root.join("GitComet.app");
        fs::create_dir_all(bundle.join("Contents/MacOS")).expect("create bundle");

        assert_eq!(find_app_bundle_in_dir(dir.path()), Some(bundle));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_app_bundle_in_dir_finds_bundle_directly_under_staging() {
        let dir = TempDir::new().expect("temp dir");
        let bundle = dir.path().join("GitComet.app");
        fs::create_dir_all(bundle.join("Contents/MacOS")).expect("create bundle");

        assert_eq!(find_app_bundle_in_dir(dir.path()), Some(bundle));
    }
}
