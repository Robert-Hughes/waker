mod diagnostics;

use std::{
    fmt::Write as _,
    fs,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use diagnostics::{DiagnosticsInfo, recent_log_tail};
use eframe::egui;
use tracing::{Instrument, error, info, info_span};
use waker_core::{
    MacAddress, WakeBackend, WakeFailure, WakeFailureStage, WakeState, WakeTarget, run_wake,
};
use waker_net::{WakerWireGuardBackend, WireGuardProfile};

pub use diagnostics::DiagnosticsRuntime;

const APP_NAME: &str = "Waker";
const DIAGNOSTIC_TAIL_BYTES: usize = 64 * 1024;
#[cfg(not(target_os = "android"))]
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/app-icon.png");
const BIG_LOGO_PNG: &[u8] = include_bytes!("../../../assets/big-logo.png");
const BACKGROUND_PNG: &[u8] = include_bytes!("../../../assets/background.png");
const TERMINAL_STATE_DISPLAY_DURATION: Duration = Duration::from_secs(5);
static LAST_ATTEMPT_ID: AtomicU64 = AtomicU64::new(0);

struct ActiveAttempt {
    id: u64,
    started: Instant,
}

#[derive(Clone)]
struct AttemptSummary {
    elapsed: Duration,
    failure: Option<WakeFailure>,
}

struct BrandingTextures {
    logo: egui::TextureHandle,
    background: egui::TextureHandle,
}

#[derive(Default)]
struct WakerFileSettings {
    fritz_ip: Option<String>,
    pc_mac: Option<String>,
}

pub struct WakerApp {
    config_path: String,
    fritz_ip: String,
    pc_mac: String,
    state: WakeState,
    state_rx: Option<Receiver<WakeState>>,
    busy: bool,
    active_attempt: Option<ActiveAttempt>,
    last_attempt: Option<AttemptSummary>,
    terminal_state_deadline: Option<Instant>,
    status_check_rx: Option<Receiver<Result<bool, String>>>,
    host_status: Option<Result<bool, String>>,
    host_status_deadline: Option<Instant>,
    ping_check_rx: Option<Receiver<Result<bool, String>>>,
    ping_status: Option<Result<bool, String>>,
    ping_status_deadline: Option<Instant>,
    diagnostics: DiagnosticsInfo,
    branding: Option<BrandingTextures>,
    clipboard_status: Option<Result<(), String>>,
    open_log_status: Option<Result<(), String>>,
    #[cfg(target_os = "android")]
    android_app: Option<winit::platform::android::activity::AndroidApp>,
    #[cfg(target_os = "android")]
    android_system_insets: Option<AndroidSystemInsets>,
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
        let WakerFileSettings { fritz_ip, pc_mac } = load_waker_file_settings(&config_path);
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
            state: WakeState::Idle,
            state_rx: None,
            busy: false,
            active_attempt: None,
            last_attempt: None,
            terminal_state_deadline: None,
            status_check_rx: None,
            host_status: None,
            host_status_deadline: None,
            ping_check_rx: None,
            ping_status: None,
            ping_status_deadline: None,
            diagnostics,
            branding: None,
            clipboard_status: None,
            open_log_status: None,
            #[cfg(target_os = "android")]
            android_app: None,
            #[cfg(target_os = "android")]
            android_system_insets: None,
        }
    }

    fn install_branding(&mut self, ctx: &egui::Context) {
        match Self::load_branding(ctx) {
            Ok(branding) => self.branding = Some(branding),
            Err(error) => error!(%error, "could not load branding assets"),
        }
    }

    fn load_branding(ctx: &egui::Context) -> Result<BrandingTextures, image::ImageError> {
        fn load_texture(
            ctx: &egui::Context,
            name: &'static str,
            png: &[u8],
        ) -> Result<egui::TextureHandle, image::ImageError> {
            let rgba = image::load_from_memory(png)?.into_rgba8();
            let size = [
                usize::try_from(rgba.width()).expect("PNG width fits usize"),
                usize::try_from(rgba.height()).expect("PNG height fits usize"),
            ];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            Ok(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
        }

        Ok(BrandingTextures {
            logo: load_texture(ctx, "waker-big-logo", BIG_LOGO_PNG)?,
            background: load_texture(ctx, "waker-background", BACKGROUND_PNG)?,
        })
    }

    fn paint_branding_background(&self, ui: &egui::Ui) {
        let Some(branding) = &self.branding else {
            return;
        };

        // Fill the panel width while preserving the artwork's aspect ratio, then
        // anchor it to the bottom. Any unused height remains plain panel fill.
        let panel_rect = ui.max_rect().expand(8.0);
        let image_size = branding.background.size_vec2();
        if image_size.x <= 0.0 || image_size.y <= 0.0 || panel_rect.width() <= 0.0 {
            return;
        }

        let scale = panel_rect.width() / image_size.x;
        let draw_size = image_size * scale;
        let draw_rect = egui::Rect::from_min_size(
            egui::pos2(panel_rect.left(), panel_rect.bottom() - draw_size.y),
            draw_size,
        );

        ui.painter().image(
            branding.background.id(),
            draw_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::from_white_alpha(56),
        );
    }

    fn render_branding_logo(&self, ui: &mut egui::Ui) {
        const LOGO_MARGIN: f32 = 16.0;
        let Some(branding) = &self.branding else {
            ui.heading(APP_NAME);
            return;
        };
        let source_size = branding.logo.size_vec2();
        let available_width = (ui.available_width() - 2.0 * LOGO_MARGIN).max(0.0);
        let width = available_width.min(300.0);
        if source_size.x <= 0.0 || source_size.y <= 0.0 || width <= 0.0 {
            return;
        }

        let height = width * source_size.y / source_size.x;
        ui.add_space(LOGO_MARGIN);
        ui.add(
            egui::Image::from_texture(&branding.logo).fit_to_exact_size(egui::vec2(width, height)),
        );
        ui.add_space(LOGO_MARGIN);
    }

    fn begin_wake(&mut self, ctx: &egui::Context) {
        self.terminal_state_deadline = None;
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
                    client_address = %job.fritz.profile.address,
                    endpoint = %job.fritz.profile.endpoint,
                    allowed_routes = job.fritz.profile.allowed_ips.len(),
                    has_preshared_key = job.fritz.profile.has_preshared_key(),
                    fritz_ip = %job.fritz.fritz_ip,
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

    fn begin_host_status_check(&mut self, ctx: &egui::Context) {
        if self.busy || self.status_check_rx.is_some() || self.ping_check_rx.is_some() {
            return;
        }
        self.host_status_deadline = None;
        info!("FRITZ!Box host status check requested");

        match self.build_fritz_job() {
            Ok(job) => {
                info!(
                    client_address = %job.profile.address,
                    endpoint = %job.profile.endpoint,
                    allowed_routes = job.profile.allowed_ips.len(),
                    has_preshared_key = job.profile.has_preshared_key(),
                    fritz_ip = %job.fritz_ip,
                    pc_mac = %job.mac,
                    "host status configuration validated"
                );
                let (result_tx, result_rx) = mpsc::channel();
                self.status_check_rx = Some(result_rx);
                self.host_status = None;
                if let Err(error) = spawn_host_status_worker(job, result_tx, ctx.clone()) {
                    error!(%error, "host status check failed before worker start");
                    self.status_check_rx = None;
                    self.host_status =
                        Some(Err(format!("Could not create worker thread: {error}")));
                    self.host_status_deadline =
                        Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
                    ctx.request_repaint_after(TERMINAL_STATE_DISPLAY_DURATION);
                }
            }
            Err(message) => {
                error!(detail = %message, "host status check rejected");
                self.host_status = Some(Err(message));
                self.host_status_deadline = Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
                ctx.request_repaint_after(TERMINAL_STATE_DISPLAY_DURATION);
            }
        }
    }

    fn begin_ping_check(&mut self, ctx: &egui::Context) {
        if self.busy || self.status_check_rx.is_some() || self.ping_check_rx.is_some() {
            return;
        }
        self.ping_status_deadline = None;
        info!("PC ICMP ping requested");

        match self.build_fritz_job() {
            Ok(job) => {
                let (result_tx, result_rx) = mpsc::channel();
                self.ping_check_rx = Some(result_rx);
                self.ping_status = None;
                if let Err(error) = spawn_ping_worker(job, result_tx, ctx.clone()) {
                    error!(%error, "PC ICMP ping failed before worker start");
                    self.ping_check_rx = None;
                    self.ping_status =
                        Some(Err(format!("Could not create worker thread: {error}")));
                    self.ping_status_deadline =
                        Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
                    ctx.request_repaint_after(TERMINAL_STATE_DISPLAY_DURATION);
                }
            }
            Err(message) => {
                error!(detail = %message, "PC ICMP ping rejected");
                self.ping_status = Some(Err(message));
                self.ping_status_deadline = Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
                ctx.request_repaint_after(TERMINAL_STATE_DISPLAY_DURATION);
            }
        }
    }

    fn build_fritz_job(&self) -> Result<FritzJob, String> {
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

        Ok(FritzJob {
            profile,
            fritz_ip,
            mac,
        })
    }

    fn build_job(&self) -> Result<WakeJob, String> {
        let fritz = self.build_fritz_job()?;
        let target = WakeTarget::new(fritz.mac);
        Ok(WakeJob { fritz, target })
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

    fn drain_host_status_update(&mut self) {
        let Some(receiver) = self.status_check_rx.as_ref() else {
            return;
        };

        let update = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "Host status worker stopped without reporting a result".to_owned(),
            )),
        };

        if let Some(result) = update {
            self.host_status = Some(result);
            self.host_status_deadline = Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
            self.status_check_rx = None;
        }
    }
    fn drain_ping_update(&mut self) {
        let Some(receiver) = self.ping_check_rx.as_ref() else {
            return;
        };

        let update = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "Ping worker stopped without reporting a result".to_owned(),
            )),
        };

        if let Some(result) = update {
            self.ping_status = Some(result);
            self.ping_status_deadline = Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
            self.ping_check_rx = None;
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
            elapsed: active.started.elapsed(),
            failure,
        });
        if matches!(self.state, WakeState::Awake | WakeState::Failed(_)) {
            self.terminal_state_deadline = Some(Instant::now() + TERMINAL_STATE_DISPLAY_DURATION);
        }
    }

    fn update_terminal_state_timeout(&mut self, now: Instant) -> Option<Duration> {
        let deadline = self.terminal_state_deadline?;

        if !matches!(self.state, WakeState::Awake | WakeState::Failed(_)) {
            self.terminal_state_deadline = None;
            return None;
        }

        if now >= deadline {
            self.state = WakeState::Idle;
            self.terminal_state_deadline = None;
            None
        } else {
            Some(deadline.duration_since(now))
        }
    }

    fn update_diagnostic_result_timeouts(&mut self, now: Instant) -> Option<Duration> {
        let mut next_repaint: Option<Duration> = None;

        for deadline in [
            &mut self.host_status_deadline,
            &mut self.ping_status_deadline,
        ] {
            let Some(value) = *deadline else {
                continue;
            };
            if now >= value {
                *deadline = None;
            } else {
                let remaining = value.duration_since(now);
                next_repaint = Some(match next_repaint {
                    Some(current) => current.min(remaining),
                    None => remaining,
                });
            }
        }

        next_repaint
    }

    fn host_status_button_label(&self) -> &'static str {
        if self.status_check_rx.is_some() {
            return "Checking FRITZ!Box…";
        }
        if self.host_status_deadline.is_some() {
            match self.host_status.as_ref() {
                Some(Ok(true)) => return "PC online",
                Some(Ok(false)) => return "PC offline",
                Some(Err(_)) | None => {}
            }
        }
        "Check PC via FRITZ!Box API"
    }

    fn ping_button_label(&self) -> &'static str {
        if self.ping_check_rx.is_some() {
            return "Pinging PC…";
        }
        if self.ping_status_deadline.is_some() {
            match self.ping_status.as_ref() {
                Some(Ok(true)) => return "PC replied",
                Some(Ok(false)) => return "No ICMP reply",
                Some(Err(_)) | None => {}
            }
        }
        "Ping PC through tunnel"
    }

    fn diagnostic_action_enabled(&self) -> bool {
        !self.busy && self.status_check_rx.is_none() && self.ping_check_rx.is_none()
    }

    fn wake_button_label(&self) -> &'static str {
        match &self.state {
            WakeState::Idle | WakeState::Failed(_) => "Wake",
            WakeState::Connecting => "Connecting…",
            WakeState::ResolvingPc => "Finding PC…",
            WakeState::Waking => "Sending wake request…",
            WakeState::WaitingForPc { .. } => "Waiting for PC…",
            WakeState::Awake => "PC awake",
        }
    }

    fn render_wake_error(&self, ui: &mut egui::Ui) {
        if let WakeState::Failed(failure) = &self.state {
            ui.colored_label(
                ui.visuals().error_fg_color,
                friendly_failure_message(failure),
            );
        }
    }

    fn render_diagnostics(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.scope(|ui| {
            ui.visuals_mut().collapsing_header_frame = true;
            ui.collapsing("Diagnostics", |ui| {
                if let Some(summary) = &self.last_attempt {
                    let outcome = if summary.failure.is_some() {
                        "failed"
                    } else {
                        "succeeded"
                    };
                    ui.label(format!(
                        "Last wake {outcome} in {}",
                        format_duration(summary.elapsed)
                    ));
                } else {
                    ui.label("No wake has completed in this session.");
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

                ui.separator();
                self.render_fritz_status_diagnostic(ui, ctx);
                ui.separator();
                self.render_ping_diagnostic(ui, ctx);
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Open log").clicked() {
                        self.open_log(ctx);
                    }
                    if ui.button("Copy log").clicked() {
                        self.copy_diagnostics_to_clipboard(ctx);
                    }
                });
                if let Some(Err(error)) = &self.open_log_status {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Could not open log: {error}"),
                    );
                }
                if let Some(status) = &self.clipboard_status {
                    match status {
                        Ok(()) => {
                            ui.small("Log copied to clipboard");
                        }
                        Err(error) => {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("Could not copy log: {error}"),
                            );
                        }
                    }
                }
            });
        });
    }

    fn render_fritz_status_diagnostic(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("FRITZ!Box host status");
        if ui
            .add_enabled(
                self.diagnostic_action_enabled(),
                egui::Button::new(self.host_status_button_label()),
            )
            .clicked()
        {
            self.begin_host_status_check(ctx);
        }
        if self.host_status_deadline.is_some()
            && let Some(Err(error)) = &self.host_status
        {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Status check failed: {error}"),
            );
        }
        ui.small("Queries FRITZ!Box only; does not send Wake-on-LAN.");
    }

    fn render_ping_diagnostic(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("PC ICMP reachability");
        if ui
            .add_enabled(
                self.diagnostic_action_enabled(),
                egui::Button::new(self.ping_button_label()),
            )
            .clicked()
        {
            self.begin_ping_check(ctx);
        }
        if self.ping_status_deadline.is_some()
            && let Some(Err(error)) = &self.ping_status
        {
            ui.colored_label(ui.visuals().error_fg_color, format!("Ping failed: {error}"));
        }
        ui.small(
            "Uses the FRITZ!Box API to resolve the PC from its MAC, then sends ICMP through Waker's private tunnel; does not send Wake-on-LAN.",
        );
    }

    fn current_log_text(&self) -> String {
        sanitize_diagnostics(&recent_log_tail(&self.diagnostics, DIAGNOSTIC_TAIL_BYTES))
    }

    fn open_log(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "android")]
        let _ = ctx;
        self.clipboard_status = None;
        let text = self.current_log_text();

        #[cfg(target_os = "android")]
        let result = self
            .android_app
            .as_ref()
            .ok_or_else(|| "Android activity is unavailable".to_owned())
            .and_then(|app| android_open_log(app, &text));

        #[cfg(not(target_os = "android"))]
        let result = {
            ctx.open_url(egui::OpenUrl::same_tab(text_data_uri(&text)));
            Ok(())
        };

        self.open_log_status = Some(result);
    }

    fn copy_diagnostics_to_clipboard(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "android")]
        let _ = ctx;
        self.open_log_status = None;
        let log_text = self.current_log_text();
        let text = self.diagnostics_bundle(&log_text);

        #[cfg(target_os = "android")]
        let result = self
            .android_app
            .as_ref()
            .ok_or_else(|| "Android activity is unavailable".to_owned())
            .and_then(|app| android_copy_text(app, &text));

        #[cfg(not(target_os = "android"))]
        let result = {
            ctx.copy_text(text);
            Ok(())
        };

        self.clipboard_status = Some(result);
    }

    fn diagnostics_bundle(&self, log_text: &str) -> String {
        let mut output = String::from("Waker diagnostics\n");
        if let Some(summary) = &self.last_attempt {
            let outcome = match &summary.failure {
                Some(failure) => format!(
                    "failed ({}) - {}",
                    failure.stage.log_name(),
                    friendly_failure_message(failure)
                ),
                None => "succeeded".to_owned(),
            };
            let _ = write!(
                output,
                "Last wake: {outcome} in {}\n\n",
                format_duration(summary.elapsed)
            );
        }
        if let Some(status) = &self.host_status {
            let status = match status {
                Ok(true) => "online".to_owned(),
                Ok(false) => "offline".to_owned(),
                Err(error) => format!("failed - {error}"),
            };
            let _ = write!(output, "FRITZ!Box host status: {status}\n\n");
        }
        if let Some(status) = &self.ping_status {
            let status = match status {
                Ok(true) => "reply received".to_owned(),
                Ok(false) => "no reply".to_owned(),
                Err(error) => format!("failed - {error}"),
            };
            let _ = write!(output, "PC ICMP ping: {status}\n\n");
        }
        output.push_str(log_text);
        sanitize_diagnostics(&output)
    }
}

fn friendly_failure_message(failure: &WakeFailure) -> &'static str {
    if failure
        .detail
        .contains("Cisco Umbrella/OpenDNS block-page address")
    {
        "Network DNS is blocking the WireGuard endpoint"
    } else if failure.detail.contains("WireGuard endpoint DNS") {
        "Could not resolve the WireGuard endpoint"
    } else {
        failure.user_message()
    }
}

#[cfg(not(target_os = "android"))]
fn text_data_uri(text: &str) -> String {
    let mut uri = String::with_capacity("data:text/plain;charset=utf-8,".len() + text.len() * 3);
    uri.push_str("data:text/plain;charset=utf-8,");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(char::from(byte));
            }
            _ => {
                let _ = write!(uri, "%{byte:02X}");
            }
        }
    }
    uri
}

#[cfg(target_os = "android")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AndroidSystemInsets {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
fn android_system_window_insets(
    app: &winit::platform::android::activity::AndroidApp,
) -> Result<Option<AndroidSystemInsets>, String> {
    use jni::{JavaVM, jni_sig, jni_str, objects::JObject};

    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_raw = app.activity_as_ptr() as jni::sys::jobject;

    vm.attach_current_thread(|env| -> jni::errors::Result<Option<AndroidSystemInsets>> {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
        let window = env
            .call_method(
                &activity,
                jni_str!("getWindow"),
                jni_sig!("()Landroid/view/Window;"),
                &[],
            )?
            .l()?;
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!("()Landroid/view/View;"),
                &[],
            )?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(None);
        }

        // These accessors are available throughout Waker's supported Android range
        // (API 24+) and remain equivalent to the system-bars inset for this purpose.
        let left = env
            .call_method(
                &insets,
                jni_str!("getSystemWindowInsetLeft"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;
        let top = env
            .call_method(
                &insets,
                jni_str!("getSystemWindowInsetTop"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;
        let right = env
            .call_method(
                &insets,
                jni_str!("getSystemWindowInsetRight"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;
        let bottom = env
            .call_method(
                &insets,
                jni_str!("getSystemWindowInsetBottom"),
                jni_sig!("()I"),
                &[],
            )?
            .i()?;

        Ok(Some(AndroidSystemInsets {
            left,
            top,
            right,
            bottom,
        }))
    })
    .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
fn android_open_log(
    app: &winit::platform::android::activity::AndroidApp,
    text: &str,
) -> Result<(), String> {
    use jni::{
        JavaVM, jni_sig, jni_str,
        objects::{JObject, JValue},
    };

    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_raw = app.activity_as_ptr() as jni::sys::jobject;

    vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
        let class_loader = env
            .call_method(
                &activity,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        let class_loader = env.cast_local::<jni::objects::JClassLoader>(class_loader)?;
        let helper_name = env.new_string("app.waker.android.LogOpener")?;
        let helper_class = class_loader.load_class(env, helper_name)?;
        let text = JObject::from(env.new_string(text)?);

        env.call_static_method(
            &helper_class,
            jni_str!("open"),
            jni_sig!((android.app.Activity, java.lang.String) -> void),
            &[JValue::Object(&activity), JValue::Object(&text)],
        )?;
        Ok(())
    })
    .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
fn android_copy_text(
    app: &winit::platform::android::activity::AndroidApp,
    text: &str,
) -> Result<(), String> {
    use jni::{
        JavaVM, jni_sig, jni_str,
        objects::{JObject, JValue},
    };

    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity_raw = app.activity_as_ptr() as jni::sys::jobject;

    vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };

        let service_name = JObject::from(env.new_string("clipboard")?);
        let clipboard = env
            .call_method(
                activity,
                jni_str!("getSystemService"),
                jni_sig!((java.lang.String) -> java.lang.Object),
                &[JValue::Object(&service_name)],
            )?
            .l()?;

        let label = JObject::from(env.new_string("Waker diagnostics")?);
        let clipboard_text = JObject::from(env.new_string(text)?);
        let clip = env
            .call_static_method(
                jni_str!("android/content/ClipData"),
                jni_str!("newPlainText"),
                jni_sig!((java.lang.CharSequence, java.lang.CharSequence) -> android.content.ClipData),
                &[JValue::Object(&label), JValue::Object(&clipboard_text)],
            )?
            .l()?;

        env.call_method(
            clipboard,
            jni_str!("setPrimaryClip"),
            jni_sig!((android.content.ClipData) -> void),
            &[JValue::Object(&clip)],
        )?;
        Ok(())
    })
    .map_err(|error| error.to_string())
}

impl eframe::App for WakerApp {
    #[cfg(target_os = "android")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let Some(android_app) = &self.android_app else {
            return;
        };
        let Ok(Some(insets)) = android_system_window_insets(android_app) else {
            return;
        };

        if self.android_system_insets != Some(insets) {
            info!(
                left = insets.left,
                top = insets.top,
                right = insets.right,
                bottom = insets.bottom,
                "Android system-bar insets changed"
            );
            self.android_system_insets = Some(insets);
        }

        let native_pixels_per_point = raw_input
            .viewports
            .get(&raw_input.viewport_id)
            .and_then(|viewport| viewport.native_pixels_per_point)
            .unwrap_or(1.0);
        let pixels_per_point = native_pixels_per_point * ctx.zoom_factor();
        if pixels_per_point <= 0.0 {
            return;
        }

        raw_input.safe_area_insets = Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
            left: insets.left.max(0) as f32 / pixels_per_point,
            top: insets.top.max(0) as f32 / pixels_per_point,
            right: insets.right.max(0) as f32 / pixels_per_point,
            bottom: insets.bottom.max(0) as f32 / pixels_per_point,
        }));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_state_updates();
        self.drain_host_status_update();
        self.drain_ping_update();
        let ctx = ui.ctx().clone();
        if let Some(remaining) = self.update_terminal_state_timeout(Instant::now()) {
            ctx.request_repaint_after(remaining);
        }
        if let Some(remaining) = self.update_diagnostic_result_timeouts(Instant::now()) {
            ctx.request_repaint_after(remaining);
        }

        #[cfg(target_os = "android")]
        {
            let viewport = ctx.viewport_rect();
            let content = ctx.content_rect();
            let fill = ui.visuals().panel_fill;
            let painter = egui::Painter::new(
                ctx.clone(),
                egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("android-system-chrome-background"),
                ),
                viewport,
            );
            let system_chrome_rects = [
                egui::Rect::from_min_max(viewport.min, egui::pos2(viewport.max.x, content.min.y)),
                egui::Rect::from_min_max(egui::pos2(viewport.min.x, content.max.y), viewport.max),
                egui::Rect::from_min_max(
                    egui::pos2(viewport.min.x, content.min.y),
                    egui::pos2(content.min.x, content.max.y),
                ),
                egui::Rect::from_min_max(
                    egui::pos2(content.max.x, content.min.y),
                    egui::pos2(viewport.max.x, content.max.y),
                ),
            ];
            for rect in system_chrome_rects {
                if rect.width() > 0.0 && rect.height() > 0.0 {
                    painter.rect_filled(rect, 0.0, fill);
                }
            }
        }

        ui.scope_builder(egui::UiBuilder::new().max_rect(ctx.content_rect()), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                self.paint_branding_background(ui);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        self.render_branding_logo(ui);

                        let wake_label = egui::RichText::new(self.wake_button_label())
                            .size(22.0)
                            .strong()
                            .color(egui::Color32::from_rgb(32, 24, 16));
                        let wake_button = egui::Button::new(wake_label)
                            .fill(egui::Color32::from_rgb(248, 160, 56))
                            .min_size(egui::vec2(160.0, 52.0));
                        if ui
                            .add_enabled(
                                !self.busy
                                    && !matches!(self.state, WakeState::Awake)
                                    && self.status_check_rx.is_none()
                                    && self.ping_check_rx.is_none(),
                                wake_button,
                            )
                            .clicked()
                        {
                            self.begin_wake(&ctx);
                        }

                        if matches!(self.state, WakeState::Failed(_)) {
                            ui.add_space(6.0);
                            self.render_wake_error(ui);
                        }
                    });

                    ui.add_space(16.0);
                    ui.separator();
                    ui.scope(|ui| {
                        ui.visuals_mut().collapsing_header_frame = true;
                        ui.collapsing("Settings", |ui| {
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
                        });
                    });
                    ui.separator();
                    self.render_diagnostics(ui, &ctx);
                });
            });
        });
    }
}

struct FritzJob {
    profile: WireGuardProfile,
    fritz_ip: Ipv4Addr,
    mac: MacAddress,
}

struct WakeJob {
    fritz: FritzJob,
    target: WakeTarget,
}

fn network_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("waker-net")
        .build()
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
            let runtime = match network_runtime() {
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
                    let mut backend =
                        WakerWireGuardBackend::new(job.fritz.profile, job.fritz.fritz_ip);
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

fn spawn_host_status_worker(
    job: FritzJob,
    result_tx: mpsc::Sender<Result<bool, String>>,
    repaint: egui::Context,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("waker-status-worker".to_owned())
        .spawn(move || {
            let runtime = match network_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let message = format!("could not start networking runtime: {error}");
                    error!(detail = %message, "host status check failed");
                    let _ = result_tx.send(Err(message));
                    repaint.request_repaint();
                    return;
                }
            };

            let span = info_span!("host_status_check");
            runtime.block_on(
                async move {
                    let started = Instant::now();
                    let mut backend = WakerWireGuardBackend::new(job.profile, job.fritz_ip);
                    let result = async {
                        backend.connect().await.map_err(|error| error.to_string())?;
                        backend
                            .host_active(job.mac)
                            .await
                            .map_err(|error| error.to_string())
                    }
                    .await;
                    backend.disconnect().await;

                    match &result {
                        Ok(active) => info!(
                            active,
                            elapsed_ms = started.elapsed().as_millis(),
                            "host status check completed"
                        ),
                        Err(message) => error!(
                            detail = %message,
                            elapsed_ms = started.elapsed().as_millis(),
                            "host status check failed"
                        ),
                    }
                    let _ = result_tx.send(result);
                    repaint.request_repaint();
                }
                .instrument(span),
            );
        })
        .map(|_| ())
}
fn spawn_ping_worker(
    job: FritzJob,
    result_tx: mpsc::Sender<Result<bool, String>>,
    repaint: egui::Context,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("waker-ping-worker".to_owned())
        .spawn(move || {
            let runtime = match network_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let message = format!("could not start networking runtime: {error}");
                    error!(detail = %message, "PC ICMP ping failed");
                    let _ = result_tx.send(Err(message));
                    repaint.request_repaint();
                    return;
                }
            };

            let span = info_span!("pc_icmp_ping");
            runtime.block_on(
                async move {
                    let started = Instant::now();
                    let mut backend = WakerWireGuardBackend::new(job.profile, job.fritz_ip);
                    let result = async {
                        backend.connect().await.map_err(|error| error.to_string())?;
                        backend
                            .ping_host(job.mac)
                            .await
                            .map_err(|error| error.to_string())
                    }
                    .await;
                    backend.disconnect().await;

                    match &result {
                        Ok(reachable) => info!(
                            reachable,
                            elapsed_ms = started.elapsed().as_millis(),
                            "PC ICMP ping completed"
                        ),
                        Err(message) => error!(
                            detail = %message,
                            elapsed_ms = started.elapsed().as_millis(),
                            "PC ICMP ping failed"
                        ),
                    }
                    let _ = result_tx.send(result);
                    repaint.request_repaint();
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
        WakeState::ResolvingPc => info!(attempt_id, stage = "resolve_target", "wake state changed"),
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
            _ => {}
        }
    }
    settings
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
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([460.0, 460.0])
        .with_min_inner_size([360.0, 320.0]);
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => error!(%error, "could not load desktop app icon"),
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation_context| {
            let mut app = WakerApp::new(diagnostics_info.clone(), None);
            app.install_branding(&creation_context.egui_ctx);
            Ok(Box::new(app))
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
    if let Err(error) = android_open_log(&app, "Waker Open log integration test\n") {
        error!(%error, "temporary Open log integration test failed");
    }
    let clipboard_app = app.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(APP_NAME),
        android_app: Some(app),
        ..Default::default()
    };

    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation_context| {
            let mut app = WakerApp::new(diagnostics_info.clone(), default_config_path.clone());
            app.install_branding(&creation_context.egui_ctx);
            app.android_app = Some(clipboard_app.clone());
            Ok(Box::new(app))
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
    fn parses_waker_fields_from_combined_profile() {
        let settings = parse_waker_file_settings(
            "[Interface]\nAddress = 192.0.2.2/24\nWakerFritzIP = 192.168.178.1\n\
             WakerPcMac = AA:BB:CC:DD:EE:FF\n\
             [Peer]\nEndpoint = example.invalid:51820\n",
        );
        assert_eq!(settings.fritz_ip.as_deref(), Some("192.168.178.1"));
        assert_eq!(settings.pc_mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
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
    fn text_log_uri_percent_encodes_content() {
        let uri = text_data_uri("line 1\nA&B");
        assert_eq!(uri, "data:text/plain;charset=utf-8,line%201%0AA%26B");
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
    fn wake_button_contains_the_primary_status() {
        let mut app = WakerApp::default();
        assert_eq!(app.wake_button_label(), "Wake");

        app.state = WakeState::Connecting;
        assert_eq!(app.wake_button_label(), "Connecting…");

        app.state = WakeState::ResolvingPc;
        assert_eq!(app.wake_button_label(), "Finding PC…");

        app.state = WakeState::Waking;
        assert_eq!(app.wake_button_label(), "Sending wake request…");

        app.state = WakeState::WaitingForPc { attempt: 42 };
        assert_eq!(app.wake_button_label(), "Waiting for PC…");

        app.state = WakeState::Awake;
        assert_eq!(app.wake_button_label(), "PC awake");

        app.state = WakeState::Failed(WakeFailure::new(WakeFailureStage::Timeout, "test failure"));
        assert_eq!(app.wake_button_label(), "Wake");
    }

    #[test]
    fn diagnostic_results_are_repeatable_and_expire_visually() {
        let now = Instant::now();
        let mut app = WakerApp {
            host_status: Some(Ok(true)),
            host_status_deadline: Some(now + TERMINAL_STATE_DISPLAY_DURATION),
            ping_status: Some(Ok(false)),
            ping_status_deadline: Some(now + TERMINAL_STATE_DISPLAY_DURATION),
            ..WakerApp::default()
        };

        assert_eq!(app.host_status_button_label(), "PC online");
        assert_eq!(app.ping_button_label(), "No ICMP reply");
        assert!(app.diagnostic_action_enabled());
        assert_eq!(
            app.update_diagnostic_result_timeouts(now),
            Some(TERMINAL_STATE_DISPLAY_DURATION)
        );

        assert_eq!(
            app.update_diagnostic_result_timeouts(now + TERMINAL_STATE_DISPLAY_DURATION),
            None
        );
        assert_eq!(app.host_status_button_label(), "Check PC via FRITZ!Box API");
        assert_eq!(app.ping_button_label(), "Ping PC through tunnel");
        assert!(app.host_status.is_some());
        assert!(app.ping_status.is_some());
    }

    #[test]
    fn diagnostic_failures_keep_the_action_label() {
        let now = Instant::now();
        let app = WakerApp {
            host_status: Some(Err("test failure".to_owned())),
            host_status_deadline: Some(now + TERMINAL_STATE_DISPLAY_DURATION),
            ping_status: Some(Err("test failure".to_owned())),
            ping_status_deadline: Some(now + TERMINAL_STATE_DISPLAY_DURATION),
            ..WakerApp::default()
        };

        assert_eq!(app.host_status_button_label(), "Check PC via FRITZ!Box API");
        assert_eq!(app.ping_button_label(), "Ping PC through tunnel");
        assert!(app.diagnostic_action_enabled());
    }

    #[test]
    fn terminal_wake_states_expire_after_five_seconds() {
        let now = Instant::now();

        for state in [
            WakeState::Awake,
            WakeState::Failed(WakeFailure::new(WakeFailureStage::Timeout, "test failure")),
        ] {
            let mut app = WakerApp {
                state,
                terminal_state_deadline: Some(now + TERMINAL_STATE_DISPLAY_DURATION),
                ..WakerApp::default()
            };

            assert_eq!(
                app.update_terminal_state_timeout(now),
                Some(TERMINAL_STATE_DISPLAY_DURATION)
            );
            assert!(!matches!(app.state, WakeState::Idle));

            assert_eq!(
                app.update_terminal_state_timeout(now + TERMINAL_STATE_DISPLAY_DURATION),
                None
            );
            assert!(matches!(app.state, WakeState::Idle));
            assert!(app.terminal_state_deadline.is_none());
        }
    }

    #[test]
    fn finishing_wake_attempt_schedules_terminal_state_reset() {
        let mut app = WakerApp {
            state: WakeState::Awake,
            active_attempt: Some(ActiveAttempt {
                id: 42,
                started: Instant::now(),
            }),
            ..WakerApp::default()
        };

        app.finish_attempt();

        assert!(app.last_attempt.is_some());
        assert!(app.terminal_state_deadline.is_some());
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
        assert!(app.last_attempt.is_some());
        assert!(app.active_attempt.is_none());
    }

    #[test]
    fn copied_diagnostics_include_host_status() {
        let app = WakerApp {
            host_status: Some(Ok(false)),
            ..WakerApp::default()
        };
        assert!(
            app.diagnostics_bundle("log tail")
                .contains("FRITZ!Box host status: offline")
        );
    }

    #[test]
    fn umbrella_dns_failure_has_helpful_heading() {
        let failure = WakeFailure::new(
            WakeFailureStage::Connect,
            "network error: WireGuard endpoint DNS for example.myfritz.net:51820 resolved to 146.112.61.104, which is a Cisco Umbrella/OpenDNS block-page address.",
        );
        assert_eq!(
            friendly_failure_message(&failure),
            "Network DNS is blocking the WireGuard endpoint"
        );
    }

    #[test]
    fn ordinary_dns_failure_has_helpful_heading() {
        let failure = WakeFailure::new(
            WakeFailureStage::Connect,
            "network error: WireGuard endpoint DNS lookup failed for example.myfritz.net:51820",
        );
        assert_eq!(
            friendly_failure_message(&failure),
            "Could not resolve the WireGuard endpoint"
        );
    }

    #[test]
    fn copied_diagnostics_include_icmp_ping_status() {
        let app = WakerApp {
            ping_status: Some(Ok(true)),
            ..WakerApp::default()
        };
        assert!(
            app.diagnostics_bundle("log tail")
                .contains("PC ICMP ping: reply received")
        );
    }
}
