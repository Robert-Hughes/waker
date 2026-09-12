use std::{
    env, fs,
    path::{Path, PathBuf},
};

use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    filter::Targets,
    fmt,
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

const LOG_PREFIX: &str = "waker.log";
const RETAINED_LOG_FILES: usize = 7;

#[derive(Clone, Debug, Default)]
pub struct DiagnosticsInfo {
    pub log_dir: Option<PathBuf>,
    pub warning: Option<String>,
}

pub struct DiagnosticsRuntime {
    guard: Option<WorkerGuard>,
    info: DiagnosticsInfo,
}

impl DiagnosticsRuntime {
    #[must_use]
    pub const fn info(&self) -> &DiagnosticsInfo {
        &self.info
    }

    #[must_use]
    pub const fn is_persistent(&self) -> bool {
        self.guard.is_some()
    }
}

fn log_level() -> Level {
    match env::var("WAKER_LOG")
        .unwrap_or_else(|_| "debug".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => Level::TRACE,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::DEBUG,
    }
}

fn safe_targets(level: Level) -> Targets {
    Targets::new()
        .with_target("waker_app", level)
        .with_target("waker_core", level)
        .with_target("waker_net", level)
}

fn open_persistent_log(
    log_dir: &Path,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard), String> {
    fs::create_dir_all(log_dir)
        .map_err(|error| format!("could not create diagnostics directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(log_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure diagnostics directory: {error}"))?;
    }
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .max_log_files(RETAINED_LOG_FILES)
        .build(log_dir)
        .map_err(|error| format!("could not open persistent diagnostic log: {error}"))?;
    Ok(
        tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .thread_name("waker-log")
            .finish(appender),
    )
}

#[cfg(not(target_os = "android"))]
#[must_use]
pub fn init_desktop() -> DiagnosticsRuntime {
    let log_dir = desktop_log_dir();
    let level = log_level();

    if let Some(log_dir) = log_dir {
        match open_persistent_log(&log_dir) {
            Ok((writer, guard)) => {
                let file_layer = fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_names(true)
                    .with_writer(writer)
                    .with_filter(safe_targets(level));
                let stderr_layer = fmt::layer()
                    .with_target(true)
                    .with_thread_names(true)
                    .with_writer(std::io::stderr)
                    .with_filter(safe_targets(level));
                return match tracing_subscriber::registry()
                    .with(file_layer)
                    .with(stderr_layer)
                    .try_init()
                {
                    Ok(()) => DiagnosticsRuntime {
                        guard: Some(guard),
                        info: DiagnosticsInfo {
                            log_dir: Some(log_dir),
                            warning: None,
                        },
                    },
                    Err(error) => DiagnosticsRuntime {
                        guard: None,
                        info: DiagnosticsInfo {
                            log_dir: None,
                            warning: Some(format!("could not initialise tracing: {error}")),
                        },
                    },
                };
            }
            Err(error) => {
                let stderr_layer = fmt::layer()
                    .with_target(true)
                    .with_thread_names(true)
                    .with_writer(std::io::stderr)
                    .with_filter(safe_targets(level));
                let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
                return DiagnosticsRuntime {
                    guard: None,
                    info: DiagnosticsInfo {
                        log_dir: None,
                        warning: Some(error),
                    },
                };
            }
        }
    }

    let stderr_layer = fmt::layer()
        .with_target(true)
        .with_thread_names(true)
        .with_writer(std::io::stderr)
        .with_filter(safe_targets(level));
    let _ = tracing_subscriber::registry().with(stderr_layer).try_init();
    DiagnosticsRuntime {
        guard: None,
        info: DiagnosticsInfo {
            log_dir: None,
            warning: Some("HOME is unavailable; persistent diagnostics are disabled".to_owned()),
        },
    }
}

#[cfg(not(target_os = "android"))]
fn desktop_log_dir() -> Option<PathBuf> {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(state_home).join("waker/logs"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/waker/logs"))
}

#[cfg(target_os = "android")]
static ANDROID_DIAGNOSTICS: std::sync::OnceLock<DiagnosticsRuntime> = std::sync::OnceLock::new();

#[cfg(target_os = "android")]
#[must_use]
pub fn init_android(internal_data_path: Option<PathBuf>) -> &'static DiagnosticsRuntime {
    ANDROID_DIAGNOSTICS.get_or_init(|| init_android_once(internal_data_path))
}

#[cfg(target_os = "android")]
fn init_android_once(internal_data_path: Option<PathBuf>) -> DiagnosticsRuntime {
    use tracing_logcat::{LogcatMakeWriter, LogcatTag};

    let level = log_level();
    let log_dir = internal_data_path.map(|path| path.join("logs"));
    let logcat = LogcatMakeWriter::new(LogcatTag::Fixed("Waker".to_owned()));

    match (log_dir, logcat) {
        (Some(log_dir), Ok(logcat)) => match open_persistent_log(&log_dir) {
            Ok((writer, guard)) => {
                let file_layer = fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_names(true)
                    .with_writer(writer)
                    .with_filter(safe_targets(level));
                let logcat_layer = fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_target(true)
                    .with_writer(logcat)
                    .with_filter(safe_targets(level));
                match tracing_subscriber::registry()
                    .with(file_layer)
                    .with(logcat_layer)
                    .try_init()
                {
                    Ok(()) => DiagnosticsRuntime {
                        guard: Some(guard),
                        info: DiagnosticsInfo {
                            log_dir: Some(log_dir),
                            warning: None,
                        },
                    },
                    Err(error) => DiagnosticsRuntime {
                        guard: None,
                        info: DiagnosticsInfo {
                            log_dir: None,
                            warning: Some(format!("could not initialise tracing: {error}")),
                        },
                    },
                }
            }
            Err(file_error) => {
                let logcat_layer = fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_target(true)
                    .with_writer(logcat)
                    .with_filter(safe_targets(level));
                let init = tracing_subscriber::registry().with(logcat_layer).try_init();
                let warning = match init {
                    Ok(()) => format!("persistent diagnostics unavailable: {file_error}"),
                    Err(init_error) => format!(
                        "persistent diagnostics unavailable ({file_error}); logcat initialisation failed ({init_error})"
                    ),
                };
                DiagnosticsRuntime {
                    guard: None,
                    info: DiagnosticsInfo {
                        log_dir: None,
                        warning: Some(warning),
                    },
                }
            }
        },
        (Some(log_dir), Err(logcat_error)) => match open_persistent_log(&log_dir) {
            Ok((writer, guard)) => {
                let file_layer = fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_names(true)
                    .with_writer(writer)
                    .with_filter(safe_targets(level));
                match tracing_subscriber::registry().with(file_layer).try_init() {
                    Ok(()) => DiagnosticsRuntime {
                        guard: Some(guard),
                        info: DiagnosticsInfo {
                            log_dir: Some(log_dir),
                            warning: Some(format!(
                                "logcat diagnostics unavailable: {logcat_error}"
                            )),
                        },
                    },
                    Err(init_error) => DiagnosticsRuntime {
                        guard: None,
                        info: DiagnosticsInfo {
                            log_dir: None,
                            warning: Some(format!(
                                "logcat diagnostics unavailable ({logcat_error}); persistent diagnostics initialisation failed ({init_error})"
                            )),
                        },
                    },
                }
            }
            Err(file_error) => DiagnosticsRuntime {
                guard: None,
                info: DiagnosticsInfo {
                    log_dir: None,
                    warning: Some(format!(
                        "persistent diagnostics unavailable ({file_error}); logcat unavailable ({logcat_error})"
                    )),
                },
            },
        },
        (None, Ok(logcat)) => {
            let logcat_layer = fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(true)
                .with_writer(logcat)
                .with_filter(safe_targets(level));
            let warning = match tracing_subscriber::registry().with(logcat_layer).try_init() {
                Ok(()) => {
                    "Android internal data path is unavailable; persistent diagnostics are disabled"
                        .to_owned()
                }
                Err(init_error) => format!(
                    "Android internal data path is unavailable and logcat initialisation failed: {init_error}"
                ),
            };
            DiagnosticsRuntime {
                guard: None,
                info: DiagnosticsInfo {
                    log_dir: None,
                    warning: Some(warning),
                },
            }
        }
        (None, Err(error)) => DiagnosticsRuntime {
            guard: None,
            info: DiagnosticsInfo {
                log_dir: None,
                warning: Some(format!(
                    "Android internal data path and logcat diagnostics are unavailable: {error}"
                )),
            },
        },
    }
}

#[must_use]
pub fn recent_log_tail(info: &DiagnosticsInfo, max_bytes: usize) -> String {
    let Some(log_dir) = &info.log_dir else {
        return "Persistent diagnostics are unavailable for this run.".to_owned();
    };
    let Ok(entries) = fs::read_dir(log_dir) else {
        return format!(
            "Could not read diagnostics directory: {}",
            log_dir.display()
        );
    };
    let mut logs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(LOG_PREFIX))
        .collect();
    logs.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let Some(path) = logs.last().map(std::fs::DirEntry::path) else {
        return "No persistent diagnostic log has been written yet.".to_owned();
    };
    match fs::read(&path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(max_bytes);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(error) => format!("Could not read {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_target_filter_does_not_enable_third_party_targets() {
        let filter = safe_targets(Level::TRACE);
        assert!(filter.would_enable("waker_net", &Level::TRACE));
        assert!(!filter.would_enable("gotatun", &Level::ERROR));
    }
}
