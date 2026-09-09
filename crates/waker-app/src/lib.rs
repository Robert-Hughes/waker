use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    str::FromStr,
    sync::mpsc::{self, Receiver},
};

use eframe::egui;
use tracing::error;
use waker_core::{MacAddress, WakeState, WakeTarget, run_wake};
use waker_net::{WakerWireGuardBackend, WireGuardProfile};

const APP_NAME: &str = "Waker";

pub struct WakerApp {
    config_path: String,
    fritz_ip: String,
    pc_mac: String,
    probe_address: String,
    state: WakeState,
    state_rx: Option<Receiver<WakeState>>,
    busy: bool,
}

impl Default for WakerApp {
    fn default() -> Self {
        Self {
            config_path: std::env::var("WAKER_WG_CONFIG")
                .unwrap_or_else(|_| "waker.local.conf".to_owned()),
            fritz_ip: std::env::var("WAKER_FRITZ_IP")
                .unwrap_or_else(|_| "192.168.178.1".to_owned()),
            pc_mac: std::env::var("WAKER_PC_MAC").unwrap_or_default(),
            probe_address: std::env::var("WAKER_PC_PROBE").unwrap_or_default(),
            state: WakeState::Idle,
            state_rx: None,
            busy: false,
        }
    }
}

impl WakerApp {
    fn begin_wake(&mut self, ctx: &egui::Context) {
        match self.build_job() {
            Ok(job) => {
                let (state_tx, state_rx) = mpsc::channel();
                self.state_rx = Some(state_rx);
                self.state = WakeState::Connecting;
                self.busy = true;
                let repaint = ctx.clone();

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
                                let _ = state_tx.send(WakeState::Failed(format!(
                                    "could not start networking runtime: {error}"
                                )));
                                repaint.request_repaint();
                                return;
                            }
                        };

                        runtime.block_on(async move {
                            let mut backend = WakerWireGuardBackend::new(job.profile, job.fritz_ip);
                            let result = run_wake(&mut backend, &job.target, |state| {
                                let _ = state_tx.send(state);
                                repaint.request_repaint();
                            })
                            .await;
                            if let Err(error) = result {
                                error!(%error, "wake attempt failed");
                            }
                        });
                    })
                    .expect("worker thread creation should succeed");
            }
            Err(message) => {
                self.state = WakeState::Failed(message);
                self.busy = false;
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

        while let Ok(state) = receiver.try_recv() {
            let terminal = matches!(state, WakeState::Awake | WakeState::Failed(_));
            self.state = state;
            if terminal {
                self.busy = false;
            }
        }
    }
}

impl eframe::App for WakerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_state_updates();
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(APP_NAME);
                ui.add_space(8.0);

                let wake_button = egui::Button::new("Wake").min_size(egui::vec2(160.0, 52.0));
                if ui.add_enabled(!self.busy, wake_button).clicked() {
                    self.begin_wake(&ctx);
                }

                ui.add_space(6.0);
                ui.label(self.state.label());
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
        });
    }
}

struct WakeJob {
    profile: WireGuardProfile,
    fritz_ip: Ipv4Addr,
    target: WakeTarget,
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

/// Run the native desktop Waker application.
///
/// # Errors
///
/// Returns an eframe error if window, graphics, or event-loop initialisation fails.
pub fn run_desktop() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([420.0, 360.0])
            .with_min_inner_size([340.0, 260.0]),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_creation_context| Ok(Box::new(WakerApp::default()))),
    )
}

#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(app: winit::platform::android::activity::AndroidApp) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(APP_NAME),
        android_app: Some(app),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_creation_context| Ok(Box::new(WakerApp::default()))),
    )
    .expect("Waker Android event loop");
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
}
