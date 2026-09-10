mod diagnostics;

use std::{
    fmt::Write as _,
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use diagnostics::{DiagnosticsInfo, recent_log_tail};
use eframe::egui;
use tracing::{Instrument, error, info, info_span};
use waker_core::{MacAddress, WakeFailure, WakeFailureStage, WakeState, WakeTarget, run_wake};
use waker_net::{WakerWireGuardBackend, WireGuardProfile};

pub use diagnostics::DiagnosticsRuntime;

const APP_NAME: &str = "Waker";
const DIAGNOSTIC_TAIL_BYTES: usize = 64 * 1024;
static LAST_ATTEMPT_ID: AtomicU64 = AtomicU64::new(0);

struct ActiveAttempt {
    id: u64,
    started: Instant,
}

#[derive(Clone)]
struct AttemptSummary {
    id: u64,
    elapsed: Duration,
    failure: Option<WakeFailure>,
}

#[derive(Default)]
struct WakerFileSettings {
    fritz_ip: Option<String>,
    pc_mac: Option<String>,
    probe_address: Option<String>,
}

pub struct WakerApp {
    config_path: String,
    fritz_ip: String,
    pc_mac: String,
    probe_address: String,
    state: WakeState,
    state_rx: Option<Receiver<WakeState>>,
    busy: bool,
    active_attempt: Option<ActiveAttempt>,
    last_attempt: Option<AttemptSummary>,
    diagnostics: DiagnosticsInfo,
    diagnostics_text: String,
}

impl Default for WakerApp {
    fn default() -> Self {
        Self::new(DiagnosticsInfo::default(), None)
    }
}

impl WakerApp {
    fn new(diagnostics: DiagnosticsInfo, default_config_path: Option<PathBuf>) -> Self {
        let config_path = std::env::var("WAKER_WG_CONFIG").unwrap_or_else(|_| {
            default_config_path
                .unwrap_or_else(|| PathBuf::from("waker.local.conf"))
                .to_string_lossy()
                .into_owned()
        });
        let WakerFileSettings {
            fritz_ip,
            pc_mac,
            probe_address,
        } = load_waker_file_settings(&config_path);
        Self {
            config_path,
            fritz_ip: std::env::var("WAKER_FRITZ_IP")
                .ok()
                .or(fritz_ip)
                .unwrap_or_else(|| "192.168.178.1".to_owned()),
            pc_mac: std::env::var("WAKER_PC_MAC")
                .ok()
                .or(pc_mac)
                .unwrap_or_default(),
            probe_address: std::env::var("WAKER_PC_PROBE")
                .ok()
                .or(probe_address)
                .unwrap_or_default(),
            state: WakeState::Idle,
            state_rx: None,
            busy: false,
            active_attempt: None,
            last_attempt: None,
            diagnostics,
            diagnostics_text: String::new(),
        }
    }

    fn begin_wake(&mut self, ctx: &egui::Context) {
        let attempt_id = next_attempt_id();
        self.active_attempt = Some(ActiveAttempt {
            id: attempt_id,
            started: Instant::now(),
        });
        info!(attempt_id, "wake attempt requested");

        match self.build_job() {
            Ok(job) => {
                info!(
                    attempt_id,
                    client_address = %job.profile.address,
                    endpoint = %job.profile.endpoint,
                    allowed_routes = job.profile.allowed_ips.len(),
                    has_preshared_key = job.profile.has_preshared_key(),
                    fritz_ip = %job.fritz_ip,
                    probe_address = %job.target.probe_address,
                    "wake configuration validated"
                );

                let (state_tx, state_rx) = mpsc::channel();
                self.state_rx = Some(state_rx);
                self.state = WakeState::Connecting;
                self.busy = true;

                if let Err(error) = spawn_wake_worker(job, attempt_id, state_tx, ctx.clone()) {
                    let failure = WakeFailure::new(
                        WakeFailureStage::Runtime,
                        format!("could not create worker thread: {error}"),
                    );
                    error!(
                        attempt_id,
                        stage = failure.stage.log_name(),
                        detail = %failure.detail,
                        "wake attempt failed before worker start"
                    );
                    self.state = WakeState::Failed(failure);
                    self.state_rx = None;
                    self.busy = false;
                    self.finish_attempt();
                }
            }
            Err(message) => {
                let failure = WakeFailure::new(WakeFailureStage::Configuration, message);
                error!(
                    attempt_id,
                    stage = failure.stage.log_name(),
                    detail = %failure.detail,
                    "wake attempt rejected"
                );
                self.state = WakeState::Failed(failure);
                self.busy = false;
                self.finish_attempt();
            }
        }
    }

    fn build_job(&self) -> Result<WakeJob, String> {
        let config_path = expand_home(&self.config_path);
        let config = fs::read_to_string(&config_path).map_err(|error| {
            format!(
                "could not read WireGuard profile {}: {error}",
                config_path.display()
            )
        })?;
        let profile = WireGuardProfile::parse(&config).map_err(|error| error.to_string())?;
        let fritz_ip = self
            .fritz_ip
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|error| format!("invalid FRITZ!Box IPv4 address: {error}"))?;
        let mac = self
            .pc_mac
            .trim()
            .parse::<MacAddress>()
            .map_err(|error| error.to_string())?;
        let probe_address = parse_probe_address(&self.probe_address)?;

        Ok(WakeJob {
            profile,
            fritz_ip,
            target: WakeTarget::new(mac, probe_address),
        })
    }

    fn drain_state_updates(&mut self) {
        let Some(receiver) = &self.state_rx else {
            return;
        };

        let mut terminal = false;
        loop {
            match receiver.try_recv() {
                Ok(state) => {
                    terminal |= matches!(state, WakeState::Awake | WakeState::Failed(_));
                    self.state = state;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.busy && !terminal {
                        let failure = WakeFailure::new(
                            WakeFailureStage::Runtime,
                            "wake worker stopped without reporting a terminal result",
                        );
                        if let Some(active) = &self.active_attempt {
                            error!(
                                attempt_id = active.id,
                                stage = failure.stage.log_name(),
                                detail = %failure.detail,
                                "wake worker disconnected unexpectedly"
                            );
                        }
                        self.state = WakeState::Failed(failure);
                        terminal = true;
                    }
                    break;
                }
            }
        }
        if terminal {
            self.busy = false;
            self.state_rx = None;
            self.finish_attempt();
        }
    }

    fn finish_attempt(&mut self) {
        let Some(active) = self.active_attempt.take() else {
            return;
        };
        let failure = match &self.state {
            WakeState::Failed(failure) => Some(failure.clone()),
            _ => None,
        };
        self.last_attempt = Some(AttemptSummary {
            id: active.id,
            elapsed: active.started.elapsed(),
            failure,
        });
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        match &self.state {
            WakeState::Failed(failure) => {
                ui.colored_label(ui.visuals().error_fg_color, failure.user_message());
                if let Some(summary) = &self.last_attempt {
                    ui.small(format!(
                        "Attempt #{} failed after {}",
                        summary.id,
                        format_duration(summary.elapsed)
                    ));
                }
                ui.collapsing("Details", |ui| {
                    ui.monospace(format!(
                        "Stage: {}\n{}",
                        failure.stage.log_name(),
                        failure.detail
                    ));
                });
            }
            WakeState::Awake => {
                ui.strong("PC awake");
                if let Some(summary) = &self.last_attempt {
                    ui.small(format!(
                        "Attempt #{} completed in {}",
                        summary.id,
                        format_duration(summary.elapsed)
                    ));
                }
            }
            state if self.busy => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(state.label());
                });
                if let Some(active) = &self.active_attempt {
                    ui.small(format!("Attempt #{}", active.id));
                }
            }
            state => {
                ui.label(state.label());
            }
        }
    }

    fn render_diagnostics(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.collapsing("Diagnostics", |ui| {
            if let Some(summary) = &self.last_attempt {
                let outcome = if summary.failure.is_some() {
                    "failed"
                } else {
                    "succeeded"
                };
                ui.label(format!(
                    "Last attempt: #{} {outcome} in {}",
                    summary.id,
                    format_duration(summary.elapsed)
                ));
            } else {
                ui.label("No wake attempt has completed in this session.");
            }

            if let Some(log_dir) = &self.diagnostics.log_dir {
                ui.small(format!("Persistent log directory: {}", log_dir.display()));
                ui.small("Retention: seven daily log files");
            } else {
                ui.small("Persistent log unavailable");
            }
            if let Some(warning) = &self.diagnostics.warning {
                ui.colored_label(ui.visuals().warn_fg_color, warning);
            }

            ui.horizontal(|ui| {
                if ui.button("Refresh log").clicked() {
                    self.diagnostics_text = sanitize_diagnostics(&recent_log_tail(
                        &self.diagnostics,
                        DIAGNOSTIC_TAIL_BYTES,
                    ));
                }
                if ui.button("Copy diagnostics").clicked() {
                    self.diagnostics_text = sanitize_diagnostics(&recent_log_tail(
                        &self.diagnostics,
                        DIAGNOSTIC_TAIL_BYTES,
                    ));
                    ctx.copy_text(self.diagnostics_bundle());
                }
            });

            if !self.diagnostics_text.is_empty() {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.diagnostics_text)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
            }
        });
    }

    fn diagnostics_bundle(&self) -> String {
        let mut output = String::from("Waker diagnostics\n");
        if let Some(summary) = &self.last_attempt {
            let outcome = match &summary.failure {
                Some(failure) => format!(
                    "failed ({}) - {}",
                    failure.stage.log_name(),
                    failure.user_message()
                ),
                None => "succeeded".to_owned(),
            };
            let _ = write!(
                output,
                "Last attempt: #{} {outcome} in {}\n\n",
                summary.id,
                format_duration(summary.elapsed)
            );
        }
        output.push_str(&self.diagnostics_text);
        sanitize_diagnostics(&output)
    }
}

impl eframe::App for WakerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_state_updates();
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading(APP_NAME);
                    ui.add_space(8.0);

                    let wake_button = egui::Button::new("Wake").min_size(egui::vec2(160.0, 52.0));
                    if ui.add_enabled(!self.busy, wake_button).clicked() {
                        self.begin_wake(&ctx);
                    }

                    ui.add_space(6.0);
                    self.render_status(ui);
                });

                ui.add_space(16.0);
                ui.separator();
                ui.collapsing("Development settings", |ui| {
                    ui.label("WireGuard profile");
                    ui.text_edit_singleline(&mut self.config_path);

                    ui.horizontal(|ui| {
                        ui.label("FRITZ!Box");
                        ui.text_edit_singleline(&mut self.fritz_ip);
                    });
                    ui.horizontal(|ui| {
                        ui.label("PC MAC");
                        ui.text_edit_singleline(&mut self.pc_mac);
                    });
                    ui.horizontal(|ui| {
                        ui.label("PC probe");
                        ui.text_edit_singleline(&mut self.probe_address);
                    });
                    ui.small("Probe format: IPv4:port, for example 192.0.2.42:22");
                });
                ui.separator();
                self.render_diagnostics(ui, &ctx);
            });
        });
    }
}

struct WakeJob {
    profile: WireGuardProfile,
    fritz_ip: Ipv4Addr,
    target: WakeTarget,
}

fn spawn_wake_worker(
    job: WakeJob,
    attempt_id: u64,
    state_tx: mpsc::Sender<WakeState>,
    repaint: egui::Context,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("waker-worker".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("waker-net")
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let failure = WakeFailure::new(
                        WakeFailureStage::Runtime,
                        format!("could not start networking runtime: {error}"),
                    );
                    error!(
                        attempt_id,
                        stage = failure.stage.log_name(),
                        detail = %failure.detail,
                        "wake attempt failed"
                    );
                    let _ = state_tx.send(WakeState::Failed(failure));
                    repaint.request_repaint();
                    return;
                }
            };

            let span = info_span!("wake_attempt", attempt_id);
            runtime.block_on(
                async move {
                    let worker_started = Instant::now();
                    let mut backend = WakerWireGuardBackend::new(job.profile, job.fritz_ip);
                    let result = run_wake(&mut backend, &job.target, |state| {
                        log_state(attempt_id, &state);
                        let _ = state_tx.send(state);
                        repaint.request_repaint();
                    })
                    .await;
                    match result {
                        Ok(()) => info!(
                            attempt_id,
                            elapsed_ms = worker_started.elapsed().as_millis(),
                            "wake attempt completed"
                        ),
                        Err(failure) => error!(
                            attempt_id,
                            stage = failure.stage.log_name(),
                            detail = %failure.detail,
                            elapsed_ms = worker_started.elapsed().as_millis(),
                            "wake attempt failed"
                        ),
                    }
                }
                .instrument(span),
            );
        })
        .map(|_| ())
}

fn log_state(attempt_id: u64, state: &WakeState) {
    match state {
        WakeState::Idle => info!(attempt_id, stage = "idle", "wake state changed"),
        WakeState::Connecting => info!(attempt_id, stage = "connect", "wake state changed"),
        WakeState::Waking => info!(attempt_id, stage = "wake_request", "wake state changed"),
        WakeState::WaitingForPc { attempt } => info!(
            attempt_id,
            stage = "probe",
            probe_attempt = attempt,
            "wake state changed"
        ),
        WakeState::Awake => info!(attempt_id, stage = "awake", "wake state changed"),
        WakeState::Failed(failure) => info!(
            attempt_id,
            stage = failure.stage.log_name(),
            "wake state changed to failed"
        ),
    }
}

fn next_attempt_id() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let previous = LAST_ATTEMPT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
            Some(now.max(previous.saturating_add(1)))
        })
        .unwrap_or_else(|previous| previous);
    now.max(previous.saturating_add(1))
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis() < 10 {
        "<10 ms".to_owned()
    } else if duration.as_millis() < 1_000 {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

fn sanitize_diagnostics(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            let lowercase = line.to_ascii_lowercase();
            let trimmed = lowercase.trim_start();
            let secret_assignment = trimmed.starts_with("privatekey")
                || trimmed.starts_with("presharedkey")
                || trimmed.starts_with("private_key=")
                || trimmed.starts_with("private_key:")
                || trimmed.starts_with("preshared_key=")
                || trimmed.starts_with("preshared_key:")
                || lowercase.contains(" private_key=")
                || lowercase.contains(" private_key:")
                || lowercase.contains(" preshared_key=")
                || lowercase.contains(" preshared_key:")
                || lowercase.contains("\"private_key\":")
                || lowercase.contains("\"preshared_key\":");
            if secret_assignment {
                "[REDACTED credential line]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_waker_file_settings(config_path: &str) -> WakerFileSettings {
    let path = expand_home(config_path);
    let Ok(config) = fs::read_to_string(path) else {
        return WakerFileSettings::default();
    };
    parse_waker_file_settings(&config)
}

fn parse_waker_file_settings(config: &str) -> WakerFileSettings {
    let mut settings = WakerFileSettings::default();
    for raw_line in config.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "wakerfritzip" => settings.fritz_ip = Some(value.to_owned()),
            "wakerpcmac" => settings.pc_mac = Some(value.to_owned()),
            "wakerprobeaddress" => settings.probe_address = Some(value.to_owned()),
            _ => {}
        }
    }
    settings
}

fn parse_probe_address(value: &str) -> Result<SocketAddrV4, String> {
    let value = value.trim();
    SocketAddrV4::from_str(value)
        .map_err(|error| format!("invalid PC probe address {value:?}: {error}"))
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return std::env::var_os("HOME").map_or_else(|| PathBuf::from(value), PathBuf::from);
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

#[cfg(not(target_os = "android"))]
#[must_use]
pub fn init_desktop_diagnostics() -> DiagnosticsRuntime {
    diagnostics::init_desktop()
}

/// Run the native desktop Waker application.
///
/// # Errors
///
/// Returns an eframe error if window, graphics, or event-loop initialisation fails.
#[cfg(not(target_os = "android"))]
pub fn run_desktop(diagnostics_runtime: DiagnosticsRuntime) -> eframe::Result {
    let diagnostics_info = diagnostics_runtime.info().clone();
    info!(
        persistent_logging = diagnostics_runtime.is_persistent(),
        log_dir = ?diagnostics_info.log_dir,
        warning = ?diagnostics_info.warning,
        "Waker starting"
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([460.0, 460.0])
            .with_min_inner_size([360.0, 320.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |_creation_context| {
            Ok(Box::new(WakerApp::new(diagnostics_info.clone(), None)))
        }),
    );
    if let Err(error) = &result {
        error!(%error, "desktop event loop failed");
    }
    drop(diagnostics_runtime);
    result
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(app: winit::platform::android::activity::AndroidApp) {
    let internal_data_path = app.internal_data_path();
    let default_config_path = internal_data_path
        .as_ref()
        .map(|path| path.join("waker.local.conf"));
    let diagnostics_runtime = diagnostics::init_android(internal_data_path);
    let diagnostics_info = diagnostics_runtime.info().clone();
    info!(
        persistent_logging = diagnostics_runtime.is_persistent(),
        log_dir = ?diagnostics_info.log_dir,
        warning = ?diagnostics_info.warning,
        "Waker starting"
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(APP_NAME),
        android_app: Some(app),
        ..Default::default()
    };

    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |_creation_context| {
            Ok(Box::new(WakerApp::new(
                diagnostics_info.clone(),
                default_config_path.clone(),
            )))
        }),
    );
    if let Err(error) = result {
        error!(%error, "Android event loop failed");
    }
    drop(diagnostics_runtime);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_prefix() {
        let expanded = expand_home("~/profile.conf");
        assert!(expanded.ends_with("profile.conf"));
    }

    #[test]
    fn parses_ipv4_probe_address() {
        let address = parse_probe_address("192.0.2.42:22").unwrap();
        assert_eq!(address.port(), 22);
    }

    #[test]
    fn parses_waker_fields_from_combined_profile() {
        let settings = parse_waker_file_settings(
            "[Interface]\nAddress = 192.0.2.2/24\nWakerFritzIP = 192.168.178.1\n\
             WakerPcMac = AA:BB:CC:DD:EE:FF\nWakerProbeAddress = 192.0.2.42:22\n\
             [Peer]\nEndpoint = example.invalid:51820\n",
        );
        assert_eq!(settings.fritz_ip.as_deref(), Some("192.168.178.1"));
        assert_eq!(settings.pc_mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(
            settings.probe_address.as_deref(),
            Some("192.0.2.42:22")
        );
    }

    #[test]
    fn copied_diagnostics_redact_wireguard_secret_fields() {
        let input =
            "ok\nPrivateKey = secret\npreshared_key=also-secret\nhas_preshared_key=true\nstill ok";
        let output = sanitize_diagnostics(input);
        assert!(!output.contains("secret"));
        assert!(output.contains("has_preshared_key=true"));
        assert_eq!(output.matches("[REDACTED credential line]").count(), 2);
    }

    #[test]
    fn attempt_ids_are_monotonic() {
        assert!(next_attempt_id() < next_attempt_id());
    }

    #[test]
    fn very_short_durations_are_readable() {
        assert_eq!(format_duration(Duration::ZERO), "<10 ms");
    }

    #[test]
    fn disconnected_worker_becomes_runtime_failure() {
        let mut app = WakerApp::default();
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        app.state_rx = Some(receiver);
        app.state = WakeState::Connecting;
        app.busy = true;
        app.active_attempt = Some(ActiveAttempt {
            id: 42,
            started: Instant::now(),
        });

        app.drain_state_updates();

        assert!(!app.busy);
        assert!(matches!(
            app.state,
            WakeState::Failed(WakeFailure {
                stage: WakeFailureStage::Runtime,
                ..
            })
        ));
        assert_eq!(
            app.last_attempt.as_ref().map(|summary| summary.id),
            Some(42)
        );
    }
}
