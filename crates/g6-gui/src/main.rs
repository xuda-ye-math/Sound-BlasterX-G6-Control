use eframe::egui;
use g6_core::{
    Device, FeatureEntry, FeatureId, Profile, builtin_profile_json, ensure_profile_dir, is_builtin,
    list_profile_names, profile_path,
};
use std::collections::HashMap;
use std::io::Read;
use std::ops::RangeInclusive;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod instance;
mod mixer_popup;
mod tray;

use instance::{AcquireOutcome, InstanceController};
use tray::{TrayAction, TrayController};

const DAC_FILTERS: &[&str] = &[
    "Fast Roll-off, Minimum Phase",
    "Slow Roll-off, Minimum Phase",
    "NOS (Non-Oversampling)",
    "Fast Roll-off, Linear Phase",
    "Slow Roll-off, Linear Phase",
];

const SMART_VOL_MODES: &[&str] = &["Normal", "Loud", "Night"];

const EQ_BANDS: &[(FeatureId, &str)] = &[
    (FeatureId::Eq31Hz, "31 Hz"),
    (FeatureId::Eq62Hz, "62 Hz"),
    (FeatureId::Eq125Hz, "125 Hz"),
    (FeatureId::Eq250Hz, "250 Hz"),
    (FeatureId::Eq500Hz, "500 Hz"),
    (FeatureId::Eq1kHz, "1 kHz"),
    (FeatureId::Eq2kHz, "2 kHz"),
    (FeatureId::Eq4kHz, "4 kHz"),
    (FeatureId::Eq8kHz, "8 kHz"),
    (FeatureId::Eq16kHz, "16 kHz"),
];

fn main() -> eframe::Result<()> {
    let instance = match InstanceController::acquire()
        .map_err(|error| eframe::Error::AppCreation(Box::new(error)))?
    {
        AcquireOutcome::Primary(instance) => instance,
        AcquireOutcome::Secondary => {
            println!("g6-gui is running. Opening the current window");
            return Ok(());
        }
    };
    let app_icon = eframe::icon_data::from_png_bytes(include_bytes!(
        "../../../assets/icons/png/g6-gui-256.png"
    ))
    .expect("embedded application icon must be valid PNG");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("g6-gui") // for Hyprland: windowrulev2 = float, class:^(g6-gui)$
            .with_title("Creative Sound BlasterX G6 Control")
            .with_icon(app_icon)
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Creative Sound BlasterX G6 Control",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, instance)) as Box<dyn eframe::App>)),
    )
}

struct CliInvocation {
    program: PathBuf,
    prefix_args: Vec<String>,
    display: String,
}

impl CliInvocation {
    fn binary(path: PathBuf) -> Self {
        Self {
            display: path.display().to_string(),
            program: path,
            prefix_args: Vec::new(),
        }
    }
}

/// Resolve g6-cli in installed, local target, and source-development layouts.
///
/// Installed packages place both executables in `/usr/bin`. A GUI built alone
/// with `cargo build -p g6-gui`, however, has no `target/*/g6-cli` sibling, so
/// it must use an installed PATH copy or run the CLI crate through Cargo.
fn cli_invocation() -> Result<CliInvocation, String> {
    if let Some(path) = std::env::var_os("G6_CLI_PATH").map(PathBuf::from) {
        if is_executable(&path) {
            return Ok(CliInvocation::binary(path));
        }
        return Err(format!(
            "G6_CLI_PATH points to a missing or non-executable file: {}",
            path.display()
        ));
    }

    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("g6-cli")))
        .filter(|path| is_executable(path))
    {
        return Ok(CliInvocation::binary(path));
    }

    if let Some(path) = find_on_path("g6-cli") {
        return Ok(CliInvocation::binary(path));
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    if manifest.is_file()
        && let Some(cargo) = find_on_path("cargo")
    {
        return Ok(CliInvocation {
            display: format!(
                "{} run --quiet --manifest-path {} -p g6-cli --",
                cargo.display(),
                manifest.display()
            ),
            program: cargo,
            prefix_args: vec![
                "run".into(),
                "--quiet".into(),
                "--manifest-path".into(),
                manifest.display().to_string(),
                "-p".into(),
                "g6-cli".into(),
                "--".into(),
            ],
        });
    }

    Err(
        "g6-cli was not found beside g6-gui or on PATH; build the full workspace or install the package"
            .into(),
    )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

struct App {
    device: Arc<Mutex<Option<Device>>>,
    values: HashMap<FeatureId, f32>,
    profiles: Vec<String>,
    selected: Option<String>,
    new_name: String,
    status: String,
    last_output: String,
    last_success: bool,
    last_label: String,
    show_output: bool,
    pending: Option<(String, Receiver<CliOutput>)>,
    watch_installed: bool,
    out_level: LevelSource,
    mic_level: LevelSource,
    volume: VolumeTracker,
    monitor_enabled: bool,
    loopback: Option<Loopback>,
    last_theme: egui::ThemePreference,
    instance: InstanceController,
    tray: Option<TrayController>,
    mixer_popup: Option<mixer_popup::Controller>,
    exit_requested: bool,
}

struct CliOutput {
    header: String,
    stderr: String,
    stdout: String,
    success: bool,
    exit_code: Option<i32>,
}

impl App {
    fn new(ctx: &egui::Context, instance: InstanceController) -> Self {
        // winit currently cannot report the system theme on Linux/X11.  LXQt
        // is light by default on this target, so an unknown `System` choice
        // should remain light instead of inheriting egui's dark fallback.
        ctx.options_mut(|options| options.fallback_theme = egui::Theme::Light);

        // Restore the theme preference saved on the previous run (if any).
        if let Some(t) = load_saved_theme() {
            ctx.options_mut(|opt| opt.theme_preference = t);
        }
        let last_theme = ctx.options(|opt| opt.theme_preference);
        // Make the complete interface visibly larger while preserving egui's
        // built-in Ctrl +/- zoom controls for further user adjustment.
        ctx.set_zoom_factor(1.3);
        instance.attach_context(ctx.clone());

        let volume = VolumeTracker::start(ctx.clone());
        let (mixer_popup, mixer_handle, mixer_warning) =
            match mixer_popup::Controller::start(volume.handle(), ctx.pixels_per_point()) {
                Ok(controller) => {
                    let handle = controller.handle();
                    (Some(controller), handle, None)
                }
                Err(error) => (
                    None,
                    mixer_popup::Handle::unavailable(),
                    Some(format!("G6 tray mixer unavailable: {error}")),
                ),
            };

        // Register the tray before probing hardware so it appears promptly at
        // launch. If this desktop has no StatusNotifier host, keep the normal
        // GUI usable and let its close button exit instead of orphaning a
        // hidden process with no way to restore it.
        let (tray, tray_warning) = match TrayController::start(ctx.clone(), mixer_handle) {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(format!("System tray unavailable: {error}"))),
        };

        let device_opt = Device::open().ok();
        let mut values = HashMap::new();
        if let Some(d) = &device_opt {
            if !d.probe() {
                let _ = Device::reset_usb();
            }
            for &id in FeatureId::ALL {
                if let Ok(v) = d.read_feature(id) {
                    values.insert(id, v);
                }
            }
        }
        let connected = device_opt.is_some();
        let profiles = list_profile_names();
        let selected = if connected {
            find_unique_matching_profile(&profiles, &values)
        } else {
            None
        };
        let mut status = if !connected {
            "Device not connected. Run `g6-cli init` (or use the Setup card in the sidebar), then reopen.".into()
        } else if let Some(name) = &selected {
            format!(
                "Read {} features from device ({} matches current state)",
                values.len(),
                name
            )
        } else {
            format!("Read {} features from device", values.len())
        };
        if let Some(warning) = tray_warning {
            status.push_str(&format!("  {warning}"));
        }
        if let Some(warning) = mixer_warning {
            status.push_str(&format!("  {warning}"));
        }
        Self {
            device: Arc::new(Mutex::new(device_opt)),
            values,
            profiles,
            selected,
            new_name: String::new(),
            status,
            last_output: String::new(),
            last_success: false,
            last_label: String::new(),
            show_output: false,
            pending: None,
            watch_installed: watch_service_installed(),
            out_level: LevelSource::start(default_sink_monitor().as_deref(), ctx.clone()),
            mic_level: LevelSource::start(None, ctx.clone()),
            volume,
            monitor_enabled: false,
            loopback: None,
            last_theme,
            instance,
            tray,
            mixer_popup,
            exit_requested: false,
        }
    }

    fn val(&self, id: FeatureId) -> f32 {
        self.values.get(&id).copied().unwrap_or(0.0)
    }

    fn has_device(&self) -> bool {
        self.device.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    fn write(&mut self, id: FeatureId, value: f32) {
        if let Err(e) = id.validate_value(value) {
            self.status = format!("Invalid: {e}");
            return;
        }
        let result = self
            .device
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.write_feature(id, value));
        match result {
            Some(Ok(())) => {
                self.values.insert(id, value);
                self.status = format!("{id:?} = {value}");
                if id == FeatureId::Output {
                    // The G6 keeps separate DSP slots per output; re-read so
                    // the sliders show the new slot's stored values.
                    std::thread::sleep(Duration::from_millis(60));
                    self.refresh();
                }
            }
            Some(Err(e)) => self.status = format!("Write failed: {e}"),
            None => self.status = "Device not connected".into(),
        }
    }

    fn refresh(&mut self) {
        let guard = self.device.lock().unwrap();
        let Some(d) = guard.as_ref() else {
            return;
        };
        for &id in FeatureId::ALL {
            if let Ok(v) = d.read_feature(id) {
                self.values.insert(id, v);
            }
        }
        drop(guard);
        self.status = "Refreshed from device".into();
    }

    fn load_profile(&mut self, ctx: &egui::Context, name: &str) {
        let json = if let Some(s) = builtin_profile_json(name) {
            s.to_string()
        } else {
            let path = match profile_path(name) {
                Ok(p) => p,
                Err(e) => {
                    self.status = e.to_string();
                    return;
                }
            };
            match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    self.status = format!("Read failed: {e}");
                    return;
                }
            }
        };
        let profile: Profile = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Parse failed: {e}");
                return;
            }
        };

        // If the profile flips Output, write Output FIRST (synchronously) and
        // refresh from the new slot before scheduling the rest -- otherwise the
        // DSP writes would land in the wrong firmware slot.
        let target_output = profile
            .features
            .iter()
            .find(|e| e.id == FeatureId::Output)
            .map(|e| e.value);
        if let Some(out_val) = target_output {
            if self.values.get(&FeatureId::Output).copied() != Some(out_val) {
                let wrote_ok = self
                    .device
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|d| d.write_feature(FeatureId::Output, out_val).is_ok())
                    .unwrap_or(false);
                if wrote_ok {
                    std::thread::sleep(Duration::from_millis(60));
                    self.refresh();
                }
            }
        }

        // Remaining writes against the (possibly refreshed) local state, sans Output.
        let to_write: Vec<FeatureEntry> = profile
            .features
            .iter()
            .filter(|e| e.id != FeatureId::Output)
            .filter(|e| self.values.get(&e.id).copied() != Some(e.value))
            .copied()
            .collect();
        let total = profile.features.len();
        let changed = to_write.len();

        // Optimistic local update for everything except Output (already reflected
        // by the refresh above when Output changed).
        for entry in &profile.features {
            if entry.id != FeatureId::Output {
                self.values.insert(entry.id, entry.value);
            }
        }

        if changed == 0 {
            self.status = format!("{name}: already at this state");
            return;
        }

        // Background-flush so the GUI thread stays responsive. Per-write lock
        // acquisition lets slider drags slip in between writes.
        let device = Arc::clone(&self.device);
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            for entry in to_write {
                if let Some(d) = device.lock().unwrap().as_ref() {
                    let _ = d.write_feature(entry.id, entry.value);
                }
            }
            ctx2.request_repaint();
        });
        self.status = format!("Applying {name} ({changed}/{total} writes)");
    }

    fn save_profile(&mut self, name: &str) {
        if is_builtin(name) {
            self.status = format!("{name} is a reserved built-in name");
            return;
        }
        if let Err(e) = ensure_profile_dir() {
            self.status = format!("Profile dir: {e}");
            return;
        }
        let path = match profile_path(name) {
            Ok(p) => p,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        if path.exists() {
            self.status = format!("{} already exists", path.display());
            return;
        }
        let features: Vec<FeatureEntry> = FeatureId::ALL
            .iter()
            .map(|&id| FeatureEntry {
                id,
                value: self.val(id),
            })
            .collect();
        let count = features.len();
        let mut json = match serde_json::to_string_pretty(&Profile { features }) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("Serialize failed: {e}");
                return;
            }
        };
        json.push('\n');
        if let Err(e) = std::fs::write(&path, json) {
            self.status = format!("Write failed: {e}");
            return;
        }
        self.profiles = list_profile_names();
        self.status = format!("Saved {name} ({count} features)");
    }

    fn remove_profile(&mut self, name: &str) {
        if is_builtin(name) {
            self.status = "Built-in profiles cannot be removed".into();
            return;
        }
        let path = match profile_path(name) {
            Ok(p) => p,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        if !path.exists() {
            return;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            self.status = format!("Remove failed: {e}");
            return;
        }
        self.profiles = list_profile_names();
        if self.selected.as_deref() == Some(name) {
            self.selected = None;
        }
        self.status = format!("Removed {name}");
    }

    /// Spawn a g6-cli command on a background thread so the GUI stays responsive
    /// while pkexec/sudo waits for the password. Result is polled in `update()`.
    fn start_cli(&mut self, ctx: &egui::Context, args: &[&str]) {
        let label = args.join(" ");
        let invocation = match cli_invocation() {
            Ok(invocation) => invocation,
            Err(error) => {
                self.last_output = format!("error: {error}\n");
                self.last_success = false;
                self.last_label = label.clone();
                self.show_output = true;
                self.status = format!("could not run g6-cli {label}");
                return;
            }
        };
        let header = format!("$ {} {}\n", invocation.display, &label);
        let args_owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let out = match Command::new(&invocation.program)
                .args(&invocation.prefix_args)
                .args(&args_owned)
                .output()
            {
                Ok(out) => CliOutput {
                    header,
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    success: out.status.success(),
                    exit_code: out.status.code(),
                },
                Err(e) => CliOutput {
                    header,
                    stderr: format!("error: {e}\n"),
                    stdout: String::new(),
                    success: false,
                    exit_code: None,
                },
            };
            let _ = tx.send(out);
            ctx2.request_repaint();
        });
        self.last_output = String::new();
        self.status = format!("running g6-cli {label} (a polkit password prompt may appear)...");
        self.pending = Some((label, rx));
    }

    /// Pick up a finished background command and surface its output.
    fn poll_pending(&mut self) {
        let Some((label, rx)) = &self.pending else {
            return;
        };
        match rx.try_recv() {
            Ok(out) => {
                self.last_output = format!("{}{}{}", out.header, out.stderr, out.stdout);
                self.last_success = out.success;
                self.last_label = label.clone();
                self.status = if out.success {
                    format!("g6-cli {label} succeeded")
                } else {
                    format!("g6-cli {label} failed (exit {:?})", out.exit_code)
                };
                self.show_output = true;
                self.pending = None;
                // Re-probe the watch-service install state whenever a service
                // command finishes, so the toggle button reflects reality even
                // if systemctl itself returned non-zero.
                if self.last_label.starts_with("service ") {
                    self.watch_installed = watch_service_installed();
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.status = "subprocess thread vanished".into();
                self.pending = None;
            }
        }
    }

    fn poll_external_actions(&mut self, ctx: &egui::Context) {
        while self.instance.try_recv_open() {
            self.open_main_window(ctx);
        }

        loop {
            let action = self.tray.as_ref().and_then(TrayController::try_recv);
            let Some(action) = action else {
                break;
            };

            match action {
                TrayAction::Initialize => {
                    if self.pending.is_none() {
                        self.start_cli(ctx, &["init", "--yes"]);
                    } else {
                        self.status = "A setup command is already running".into();
                    }
                }
                TrayAction::OpenMainWindow => {
                    self.open_main_window(ctx);
                }
                TrayAction::Exit => {
                    self.exit_requested = true;
                    // Do not terminate from the D-Bus callback. Asking eframe
                    // to close lets on_exit and every Drop implementation run.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn open_main_window(&mut self, ctx: &egui::Context) {
        if let Some(mixer_popup) = &self.mixer_popup {
            mixer_popup.hide();
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Logic is also called while the root window is hidden, so tray and
        // single-instance actions remain responsive without a visible window.
        self.poll_external_actions(ctx);

        if ctx.input(|input| input.viewport().close_requested())
            && !self.exit_requested
            && self.tray.is_some()
        {
            // A title-bar close or Alt+F4 only hides the root viewport. The
            // tray remains alive and can restore this same window later.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            if let Some(mixer_popup) = &self.mixer_popup {
                mixer_popup.hide();
            }
        }

        self.poll_pending();

        // Persist Light/Dark/System whenever the user toggles it.
        let current_theme = ctx.options(|opt| opt.theme_preference);
        if current_theme != self.last_theme {
            let _ = save_theme(current_theme);
            self.last_theme = current_theme;
        }
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();

        egui::Panel::top("toolbar")
            .exact_size(48.0)
            .show_inside(root_ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Creative Sound BlasterX G6 Control")
                            .size(22.0)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        egui::widgets::global_theme_preference_buttons(ui);
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("by Xuda Ye and Claude Code")
                                .weak()
                                .size(14.0),
                        );
                    });
                });
            });
        egui::Panel::left("sidebar")
            .frame(egui::Frame::side_top_panel(root_ui.style()).inner_margin(8))
            .resizable(true)
            .default_size(240.0)
            .min_size(200.0)
            .max_size(800.0)
            .show_inside(root_ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.ui_sidebar(ui);
                });
            });
        egui::Panel::bottom("status").show_inside(root_ui, |ui| {
            ui.label(&self.status);
        });
        egui::CentralPanel::default().show_inside(root_ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.ui_features(ui);
            });
        });

        if self.show_output {
            let modal = egui::Modal::new(egui::Id::new("cli_output_modal")).show(&ctx, |ui| {
                ui.set_min_width(440.0);
                ui.set_max_width(680.0);

                let (icon, icon_color, heading) =
                    result_banner(&self.last_label, self.last_success);
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(icon)
                            .color(icon_color)
                            .size(64.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(heading).size(20.0).strong());
                    ui.add_space(8.0);
                });

                ui.separator();
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(280.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&self.last_output)
                                .monospace()
                                .size(12.0),
                        );
                    });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.add_sized(
                        [160.0, 36.0],
                        egui::Button::new(egui::RichText::new("OK").size(16.0).strong()),
                    )
                    .clicked()
                })
                .inner
            });
            if modal.should_close() || modal.inner {
                self.show_output = false;
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.instance.shutdown();
        if let Some(tray) = &mut self.tray {
            tray.shutdown();
        }
        if let Some(mixer_popup) = &mut self.mixer_popup {
            mixer_popup.shutdown();
        }
    }
}

impl App {
    fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        let busy = self.pending.is_some();

        card(ui, "Setup", |ui| {
            if full_width_button_enabled(ui, !busy, "Audio Initialize")
                .on_hover_text("Set ALSA mic + PipeWire defaults; install udev rule if missing")
                .clicked()
            {
                self.start_cli(ui.ctx(), &["init", "--yes"]);
            }
            let watch_label = if self.watch_installed {
                "Watch Service: Installed"
            } else {
                "Watch Service: Not Installed"
            };
            let watch_btn = egui::Button::new(watch_label)
                .selected(self.watch_installed)
                .min_size(egui::vec2(ui.available_width(), 24.0));
            let resp = ui.add_enabled(!busy, watch_btn).on_hover_text(
                "Systemd user service that keeps External Mic across PulseAudio resyncs.\n\
                 Click to install if not installed, or uninstall if installed.",
            );
            if resp.clicked() {
                if self.watch_installed {
                    self.start_cli(ui.ctx(), &["service", "uninstall"]);
                } else {
                    self.start_cli(ui.ctx(), &["service", "install"]);
                }
            }
        });

        card(ui, "Profile", |ui| {
            let profiles = self.profiles.clone();
            for name in &profiles {
                let star = if is_builtin(name) { "*" } else { " " };
                let selected = self.selected.as_deref() == Some(name.as_str());
                let mut text = egui::RichText::new(format!("{star} {name}"));
                if selected {
                    text = text.strong();
                }
                if ui.selectable_label(selected, text).clicked() {
                    self.selected = Some(name.clone());
                    self.load_profile(ui.ctx(), name);
                }
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label("Save current as:");
            ui.text_edit_singleline(&mut self.new_name);
            ui.add_space(4.0);
            let can_save = !self.new_name.is_empty() && !is_builtin(&self.new_name);
            if full_width_button_enabled(ui, can_save, "Save").clicked() {
                let name = self.new_name.clone();
                self.save_profile(&name);
                self.new_name.clear();
            }
            let can_remove = self.selected.as_ref().is_some_and(|n| !is_builtin(n));
            if full_width_button_enabled(ui, can_remove, "Remove").clicked() {
                if let Some(name) = self.selected.clone() {
                    self.remove_profile(&name);
                }
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            if full_width_button(ui, "Refresh from Device").clicked() {
                self.refresh();
            }
        });

        card(ui, "Levels", |ui| {
            // Match slider rail width to the level bar above it. We do this
            // once per card draw — `level_bar` uses `ui.available_width()`,
            // so syncing `slider_width` to the same value lines them up.
            ui.spacing_mut().slider_width = ui.available_width();

            ui.label("Output");
            level_bar(ui, self.out_level.value(), self.out_level.peak_hold());
            let mut out_vol = self.volume.sink();
            let resp = ui.add(
                egui::Slider::new(&mut out_vol, 0.0..=1.5)
                    .show_value(false)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            );
            if resp.changed() {
                self.volume.set_sink(out_vol);
            }

            ui.add_space(6.0);
            ui.label("Mic");
            level_bar(ui, self.mic_level.value(), self.mic_level.peak_hold());
            let mut mic_vol = self.volume.source();
            let resp = ui
                .add(
                    egui::Slider::new(&mut mic_vol, 0.0..=1.5)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                )
                .on_hover_text(
                    "Pulse source (system) volume. The ALSA 'External Mic'\n\
                     element is held at 100 % by `g6-cli init` because the\n\
                     G6 mic capture is quiet — this slider is the layer above\n\
                     that and won't fight with init.",
                );
            if resp.changed() {
                self.volume.set_source(mic_vol);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            let was = self.monitor_enabled;
            ui.checkbox(&mut self.monitor_enabled, "Monitor mic (hear yourself)")
                .on_hover_text(
                    "Routes the default mic into the default sink via \
                     pactl module-loopback. Auto-unloaded when G6 Control exits.",
                );
            if self.monitor_enabled != was {
                if self.monitor_enabled {
                    self.loopback = Some(Loopback::start());
                } else {
                    self.loopback = None;
                }
            }
        });

        card(ui, "Notes", |ui| {
            ui.label("• Audio Initialize and Watch Service are safe for any output.");
            ui.label("• Profiles in this build have only been tested with headphones.");
            ui.label(
                "• Loading or saving a profile while routed to speakers may behave unexpectedly.",
            );
            ui.label("• Press Ctrl +/- to zoom the window.");
        });
    }

    fn ui_features(&mut self, ui: &mut egui::Ui) {
        if !self.has_device() {
            ui.colored_label(egui::Color32::DARK_RED, "Device not connected.");
            ui.label("Use the Setup card in the sidebar, then reopen this window.");
            return;
        }

        // Sliders grow with the column width, but keep them compact-first.
        let avail = ui.available_width();
        ui.spacing_mut().slider_width = (avail - 240.0).clamp(140.0, 260.0) * 0.75;
        // Default interact_size.x is 40, which inflates the checkbox column to
        // 40 px even though the visible glyph is ~14 px. Tighten it so the box
        // sits right next to its label.
        ui.spacing_mut().interact_size.x = 18.0;

        card(ui, "Global", |ui| {
            self.global_grid(ui);
        });
        card(ui, "SBX Effects", |ui| {
            self.sbx_grid(ui);
        });
        card(ui, "Equalizer", |ui| {
            self.eq_grid(ui);
        });
        card(ui, "EQ Response", |ui| {
            self.eq_response_plot(ui);
        });
    }

    /// Plot the summed peaking-EQ response across the audible range.
    ///
    /// Each band contributes a Lorentzian bump centered on its ISO frequency
    /// (Q ≈ 1.41, matching `RizeCrime/linuxblaster_control`'s curve), and the
    /// pre-amp shifts the whole thing. When `EqToggle` is off the curve is
    /// drawn dimmed to signal that the device is bypassing it. Drawn with
    /// raw `egui::Painter` calls — no extra plotting dependency.
    fn eq_response_plot(&self, ui: &mut egui::Ui) {
        const BANDS: [(f32, FeatureId); 10] = [
            (31.0, FeatureId::Eq31Hz),
            (62.0, FeatureId::Eq62Hz),
            (125.0, FeatureId::Eq125Hz),
            (250.0, FeatureId::Eq250Hz),
            (500.0, FeatureId::Eq500Hz),
            (1_000.0, FeatureId::Eq1kHz),
            (2_000.0, FeatureId::Eq2kHz),
            (4_000.0, FeatureId::Eq4kHz),
            (8_000.0, FeatureId::Eq8kHz),
            (16_000.0, FeatureId::Eq16kHz),
        ];
        const F_MIN_LOG: f32 = 1.301_03; // log10(20)
        const F_MAX_LOG: f32 = 4.301_03; // log10(20_000)
        const DB_RANGE: f32 = 12.0; // y-axis ±12 dB
        const Q: f32 = 1.41;

        let preamp = self.val(FeatureId::EqPreAmp);
        let gains: [f32; 10] = std::array::from_fn(|i| self.val(BANDS[i].1));
        let enabled = self.val(FeatureId::EqToggle) > 0.5;

        let width = ui.available_width();
        let height = 160.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let bg = ui.visuals().extreme_bg_color;
        let grid = ui.visuals().widgets.noninteractive.bg_stroke.color;

        // Background and outer frame.
        painter.rect_filled(rect, 3.0, bg);
        painter.rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(1.0_f32, grid),
            egui::StrokeKind::Inside,
        );

        let to_x =
            |log_f: f32| rect.min.x + (log_f - F_MIN_LOG) / (F_MAX_LOG - F_MIN_LOG) * rect.width();
        let to_y =
            |db: f32| rect.min.y + (1.0 - (db + DB_RANGE) / (2.0 * DB_RANGE)) * rect.height();

        // Horizontal dB grid lines at -12, -6, 0, +6, +12 — 0 dB drawn brighter.
        for &db in &[-12.0_f32, -6.0, 0.0, 6.0, 12.0] {
            let y = to_y(db);
            let stroke = if db == 0.0 {
                egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color())
            } else {
                egui::Stroke::new(1.0_f32, grid)
            };
            painter.line_segment(
                [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                stroke,
            );
        }
        // Vertical frequency markers at 100 Hz, 1 kHz, 10 kHz with labels.
        for &(f, label) in &[(100.0_f32, "100"), (1_000.0, "1k"), (10_000.0, "10k")] {
            let x = to_x(f.log10());
            painter.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.0_f32, grid),
            );
            painter.text(
                egui::pos2(x + 2.0, rect.max.y - 2.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::proportional(10.0),
                ui.visuals().weak_text_color(),
            );
        }

        // 256 sample points across the log-frequency axis — sum each band's
        // Lorentzian falloff (1 / (1 + (Δf / (fc / (2 Q)))²)) and add the
        // pre-amp offset.
        let n_pts = 256usize;
        let mut pts: Vec<egui::Pos2> = Vec::with_capacity(n_pts);
        for i in 0..n_pts {
            let t = i as f32 / (n_pts - 1) as f32;
            let log_f = F_MIN_LOG + t * (F_MAX_LOG - F_MIN_LOG);
            let f = 10.0_f32.powf(log_f);
            let mut db = preamp;
            for (b, &(fc, _)) in BANDS.iter().enumerate() {
                let gain = gains[b];
                if gain.abs() < 0.01 {
                    continue;
                }
                let half_bw = fc / (2.0 * Q);
                let diff = f - fc;
                db += gain / (1.0 + (diff / half_bw).powi(2));
            }
            pts.push(egui::pos2(to_x(log_f), to_y(db.clamp(-DB_RANGE, DB_RANGE))));
        }
        let curve_color = if enabled {
            egui::Color32::from_rgb(220, 90, 90)
        } else {
            egui::Color32::from_rgba_unmultiplied(220, 90, 90, 96)
        };
        painter.add(egui::Shape::line(
            pts,
            egui::Stroke::new(2.0_f32, curve_color),
        ));

        if !enabled {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "EQ disabled",
                egui::FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );
        }
    }

    // ─── Grid layouts (column 1 = checkbox, column 2 = label, column 3+ = controls) ─

    fn global_grid(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("g_global")
            .num_columns(3)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                for (id, label) in [
                    (FeatureId::SbxMaster, "SBX Master"),
                    (FeatureId::ScoutMode, "Scout Mode"),
                ] {
                    self.grid_check(ui, id);
                    ui.label(label);
                    ui.label("");
                    ui.end_row();
                }
                self.grid_combo(ui, FeatureId::Output, "Output", &["Speakers", "Headphones"]);
                ui.end_row();
                self.grid_combo(ui, FeatureId::DacFilter, "DAC Filter", DAC_FILTERS);
                ui.end_row();
            });
    }

    fn sbx_grid(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("g_sbx")
            .num_columns(4)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                self.grid_fx(
                    ui,
                    FeatureId::SurroundToggle,
                    FeatureId::SurroundLevel,
                    "Surround",
                );
                ui.label("");
                ui.end_row();

                // Distance lives right under Surround (it controls the same effect)
                // and is a label-only row (no toggle of its own).
                ui.label("");
                ui.label("Distance");
                let mut v = self.val(FeatureId::SurroundDistance);
                let resp = ui.add(
                    egui::Slider::new(&mut v, 10.0..=300.0)
                        .custom_formatter(|n, _| format!("{:>3.0} cm", n)),
                );
                if resp.changed() {
                    self.values.insert(FeatureId::SurroundDistance, v);
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    self.write(FeatureId::SurroundDistance, v);
                }
                ui.label("");
                ui.end_row();

                self.grid_fx(
                    ui,
                    FeatureId::DialogPlusToggle,
                    FeatureId::DialogPlusLevel,
                    "Dialog+",
                );
                ui.label("");
                ui.end_row();

                // Smart Vol uses col 4 for its mode combo.
                let on = self.val(FeatureId::SmartVolToggle) > 0.5;
                self.grid_check(ui, FeatureId::SmartVolToggle);
                ui.label("Smart Vol");
                self.grid_pct_slider(ui, FeatureId::SmartVolLevel, on);
                let current = self.val(FeatureId::SmartVolMode) as usize;
                let mut next = current;
                egui::ComboBox::from_id_salt("smartvol_mode")
                    .selected_text(SMART_VOL_MODES.get(current).copied().unwrap_or("?"))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for (i, m) in SMART_VOL_MODES.iter().enumerate() {
                            ui.selectable_value(&mut next, i, *m);
                        }
                    });
                if next != current {
                    self.write(FeatureId::SmartVolMode, next as f32);
                }
                ui.end_row();

                self.grid_fx(
                    ui,
                    FeatureId::CrystalizerToggle,
                    FeatureId::CrystalizerLevel,
                    "Crystalizer",
                );
                ui.label("");
                ui.end_row();

                self.grid_fx(ui, FeatureId::BassToggle, FeatureId::BassLevel, "Bass");
                ui.label("");
                ui.end_row();
            });
    }

    fn eq_grid(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("g_eq")
            .num_columns(3)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                self.grid_check(ui, FeatureId::EqToggle);
                ui.label("EQ Enable");
                ui.label("");
                ui.end_row();

                self.grid_band(ui, FeatureId::EqPreAmp, "Pre-amp", -6.0..=6.0);
                ui.end_row();
                for &(id, label) in EQ_BANDS {
                    self.grid_band(ui, id, label, -12.0..=12.0);
                    ui.end_row();
                }
            });
    }

    // ─── Grid cell helpers (each call fills exactly the columns labelled below) ────

    /// Column 1: bare checkbox (no label text, the label is the next cell).
    fn grid_check(&mut self, ui: &mut egui::Ui, id: FeatureId) {
        let mut on = self.val(id) > 0.5;
        if ui.checkbox(&mut on, "").changed() {
            self.write(id, if on { 1.0 } else { 0.0 });
        }
    }

    /// Columns 1-3: empty | label | combo dropdown.
    fn grid_combo(&mut self, ui: &mut egui::Ui, id: FeatureId, label: &str, options: &[&str]) {
        ui.label("");
        ui.label(label);
        let current = self.val(id) as usize;
        let mut next = current;
        egui::ComboBox::from_id_salt(label)
            .selected_text(options.get(current).copied().unwrap_or("?"))
            .show_ui(ui, |ui| {
                for (i, opt) in options.iter().enumerate() {
                    ui.selectable_value(&mut next, i, *opt);
                }
            });
        if next != current {
            self.write(id, next as f32);
        }
    }

    /// Columns 1-3: checkbox | label | percent slider (enabled iff toggle on).
    fn grid_fx(&mut self, ui: &mut egui::Ui, toggle: FeatureId, level: FeatureId, label: &str) {
        let on = self.val(toggle) > 0.5;
        self.grid_check(ui, toggle);
        ui.label(label);
        self.grid_pct_slider(ui, level, on);
    }

    /// Column 3: a single percent (0..=1) slider rendered as "NN%".
    fn grid_pct_slider(&mut self, ui: &mut egui::Ui, id: FeatureId, enabled: bool) {
        let mut v = self.val(id);
        let resp = ui.add_enabled(
            enabled,
            egui::Slider::new(&mut v, 0.0..=1.0)
                .custom_formatter(|n, _| format!("{:>3.0}%", n * 100.0))
                .custom_parser(|s| {
                    s.trim_end_matches('%')
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .map(|n| n / 100.0)
                }),
        );
        if resp.changed() {
            self.values.insert(id, v);
        }
        if resp.drag_stopped() || resp.lost_focus() {
            self.write(id, v);
        }
    }

    /// Columns 1-3: empty | label | dB slider. Fixed-width value (always 5 chars
    /// with sign) so column 3 never resizes between -12.0 / +0.0 / +12.0 etc.
    fn grid_band(
        &mut self,
        ui: &mut egui::Ui,
        id: FeatureId,
        label: &str,
        range: RangeInclusive<f32>,
    ) {
        ui.label("");
        ui.label(label);
        let mut v = self.val(id);
        let resp = ui.add(
            egui::Slider::new(&mut v, range).custom_formatter(|n, _| format!("{:>+5.1} dB", n)),
        );
        if resp.changed() {
            self.values.insert(id, v);
        }
        if resp.drag_stopped() || resp.lost_focus() {
            self.write(id, v);
        }
    }
}

/// A button that stretches to fill the available column width.
fn full_width_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized([ui.available_width(), 24.0], egui::Button::new(label))
}

/// Same as `full_width_button` but greyed out when `enabled` is false.
fn full_width_button_enabled(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    let btn = egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 24.0));
    ui.add_enabled(enabled, btn)
}

/// Wrap a labelled section in a bordered card with breathing room.
/// Stream one mono channel from `parec` and continuously update two atomic
/// peaks: a smoothed bar level (instant rise, slow exponential decay) and a
/// peak-hold marker (held briefly at the recent maximum then falling). Used
/// for the input/output level meters in the sidebar. Dropping the source kills
/// the child process and joins the reader thread.
struct LevelSource {
    level: Arc<AtomicU32>,
    hold: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    child: Option<std::process::Child>,
}

impl LevelSource {
    /// `device` is `None` for `parec`'s default (the active record source = mic),
    /// or `Some("<sink-name>.monitor")` to tap a sink's monitor stream.
    fn start(device: Option<&str>, ctx: egui::Context) -> Self {
        let level = Arc::new(AtomicU32::new(0));
        let hold = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut cmd = Command::new("parec");
        cmd.arg("--format=s16le")
            .arg("--rate=44100")
            .arg("--channels=1")
            .arg("--latency-msec=30");
        if let Some(d) = device {
            cmd.arg(format!("--device={d}"));
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                return Self {
                    level,
                    hold,
                    stop,
                    child: None,
                };
            }
        };
        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                return Self {
                    level,
                    hold,
                    stop,
                    child: Some(child),
                };
            }
        };

        let level_c = level.clone();
        let hold_c = hold.clone();
        let stop_c = stop.clone();
        std::thread::spawn(move || {
            // 512 s16le samples ≈ 12 ms @ 44.1 kHz — `parec` aggregates ~30 ms
            // of audio into each read in practice, so the loop spins ~33 Hz.
            let mut buf = [0u8; 1024];
            let mut bar: f32 = 0.0;
            let mut peak_hold: f32 = 0.0;
            let mut hold_frames: u32 = 0;
            loop {
                if stop_c.load(Ordering::Relaxed) {
                    return;
                }
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let mut peak: i32 = 0;
                        let mut i = 0;
                        while i + 1 < n {
                            let s = i16::from_le_bytes([buf[i], buf[i + 1]]).unsigned_abs() as i32;
                            if s > peak {
                                peak = s;
                            }
                            i += 2;
                        }
                        let p = peak as f32 / 32768.0;

                        // Bar: instant rise, slow exponential decay (~half life
                        // ≈ 0.4 s at 33 Hz update). Feels less twitchy than a
                        // raw peak read but still responds to transients.
                        bar = if p >= bar { p } else { bar * 0.94 + p * 0.06 };

                        // Peak-hold marker: latch onto a new peak immediately,
                        // hold it for ~50 frames (~1.5 s), then fall ~6 % per
                        // frame. Matches the "floating particle" look in OBS.
                        if p >= peak_hold {
                            peak_hold = p;
                            hold_frames = 0;
                        } else if hold_frames < 50 {
                            hold_frames += 1;
                        } else {
                            peak_hold *= 0.94;
                        }
                        if peak_hold < bar {
                            peak_hold = bar;
                        }

                        level_c.store(bar.to_bits(), Ordering::Relaxed);
                        hold_c.store(peak_hold.to_bits(), Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
            }
        });

        Self {
            level,
            hold,
            stop,
            child: Some(child),
        }
    }

    fn value(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    fn peak_hold(&self) -> f32 {
        f32::from_bits(self.hold.load(Ordering::Relaxed))
    }
}

impl Drop for LevelSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Track the Sound BlasterX G6 sink and source volumes via `pactl` (falling
/// back to the defaults only when the G6 nodes are unavailable). On start it
/// does an initial read, then spawns a `pactl subscribe` reader that re-queries
/// whenever a sink/source change event fires — so keyboard volume keys,
/// pavucontrol, and any other tool that adjusts Pulse volumes will be
/// reflected in the GUI sliders without polling. Writes are queued to
/// `pactl set-sink-volume` / `set-source-volume`; the atomics are also
/// updated eagerly so the slider stays put until the subscribe thread catches
/// up. `Drop` kills the subscribe child.
///
/// Note on the mic side: `g6-cli init` cranks the ALSA "External Mic" mixer
/// element to 100 % so the G6 actually captures usable signal. The slider
/// here lives one layer up (PulseAudio source volume), so adjusting it does
/// *not* fight with the init step.
#[derive(Clone)]
pub(crate) struct VolumeHandle {
    sink_vol: Arc<AtomicU32>,
    source_vol: Arc<AtomicU32>,
    sink_tx: Option<std::sync::mpsc::Sender<f32>>,
    source_tx: Option<std::sync::mpsc::Sender<f32>>,
}

impl VolumeHandle {
    pub(crate) fn sink(&self) -> f32 {
        f32::from_bits(self.sink_vol.load(Ordering::Relaxed))
    }

    pub(crate) fn source(&self) -> f32 {
        f32::from_bits(self.source_vol.load(Ordering::Relaxed))
    }

    pub(crate) fn set_sink(&self, value: f32) {
        self.sink_vol.store(value.to_bits(), Ordering::Relaxed);
        if let Some(sender) = &self.sink_tx {
            let _ = sender.send(value);
        }
    }

    pub(crate) fn set_source(&self, value: f32) {
        self.source_vol.store(value.to_bits(), Ordering::Relaxed);
        if let Some(sender) = &self.source_tx {
            let _ = sender.send(value);
        }
    }
}

struct VolumeTracker {
    sink_vol: Arc<AtomicU32>,
    source_vol: Arc<AtomicU32>,
    /// Channel into the sink-volume writer thread. Dropping it (when the
    /// tracker dies) closes the channel and the worker exits.
    sink_tx: Option<std::sync::mpsc::Sender<f32>>,
    source_tx: Option<std::sync::mpsc::Sender<f32>>,
    stop: Arc<AtomicBool>,
    child: Option<std::process::Child>,
}

impl VolumeTracker {
    fn start(ctx: egui::Context) -> Self {
        let sink_target = find_g6_pulse_node("sinks", "alsa_output.")
            .unwrap_or_else(|| "@DEFAULT_SINK@".to_owned());
        let source_target = find_g6_pulse_node("sources", "alsa_input.")
            .unwrap_or_else(|| "@DEFAULT_SOURCE@".to_owned());
        let sink_vol = Arc::new(AtomicU32::new(
            read_sink_volume(&sink_target).unwrap_or(1.0).to_bits(),
        ));
        let source_vol = Arc::new(AtomicU32::new(
            read_source_volume(&source_target).unwrap_or(1.0).to_bits(),
        ));
        let stop = Arc::new(AtomicBool::new(false));

        // Spawn one writer thread per device. Each consumes a channel of
        // desired volumes (f32 ∈ 0..=1.5); after each `pactl set-*-volume` call
        // it drains any newer messages so a fast slider drag collapses to
        // just the latest value, instead of queueing one pactl spawn per
        // frame and stalling the GUI thread.
        let sink_tx = spawn_volume_writer("set-sink-volume", sink_target.clone());
        let source_tx = spawn_volume_writer("set-source-volume", source_target.clone());

        let mut child = match Command::new("pactl")
            .arg("subscribe")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                return Self {
                    sink_vol,
                    source_vol,
                    sink_tx: Some(sink_tx),
                    source_tx: Some(source_tx),
                    stop,
                    child: None,
                };
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                return Self {
                    sink_vol,
                    source_vol,
                    sink_tx: Some(sink_tx),
                    source_tx: Some(source_tx),
                    stop,
                    child: Some(child),
                };
            }
        };

        let sink_c = sink_vol.clone();
        let source_c = source_vol.clone();
        let stop_c = stop.clone();
        let sink_target_c = sink_target.clone();
        let source_target_c = source_target.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                if stop_c.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(line) = line else {
                    return;
                };
                // pactl emits e.g. "Event 'change' on sink #N", "Event 'new' on
                // sink-input #N", etc. We only care about sink/source.
                if line.contains(" on sink #") {
                    if let Some(v) = read_sink_volume(&sink_target_c) {
                        sink_c.store(v.to_bits(), Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                } else if line.contains(" on source #") {
                    if let Some(v) = read_source_volume(&source_target_c) {
                        source_c.store(v.to_bits(), Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
            }
        });

        Self {
            sink_vol,
            source_vol,
            sink_tx: Some(sink_tx),
            source_tx: Some(source_tx),
            stop,
            child: Some(child),
        }
    }

    fn sink(&self) -> f32 {
        f32::from_bits(self.sink_vol.load(Ordering::Relaxed))
    }
    fn source(&self) -> f32 {
        f32::from_bits(self.source_vol.load(Ordering::Relaxed))
    }

    fn set_sink(&self, v: f32) {
        // Eager local update so the slider sticks immediately; the actual
        // `pactl` write happens on the writer thread.
        self.sink_vol.store(v.to_bits(), Ordering::Relaxed);
        if let Some(tx) = &self.sink_tx {
            let _ = tx.send(v);
        }
    }

    fn set_source(&self, v: f32) {
        self.source_vol.store(v.to_bits(), Ordering::Relaxed);
        if let Some(tx) = &self.source_tx {
            let _ = tx.send(v);
        }
    }

    fn handle(&self) -> VolumeHandle {
        VolumeHandle {
            sink_vol: Arc::clone(&self.sink_vol),
            source_vol: Arc::clone(&self.source_vol),
            sink_tx: self.sink_tx.clone(),
            source_tx: self.source_tx.clone(),
        }
    }
}

/// Background worker that owns one `pactl set-*-volume` channel. On every
/// message it drains any queued newer messages and only writes the latest —
/// so a 60 Hz slider drag produces a handful of pactl spawns instead of one
/// per frame. Returns the `Sender`; dropping it closes the channel and the
/// worker thread exits naturally.
fn spawn_volume_writer(verb: &'static str, target: String) -> std::sync::mpsc::Sender<f32> {
    let (tx, rx) = std::sync::mpsc::channel::<f32>();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut latest = first;
            while let Ok(v) = rx.try_recv() {
                latest = v;
            }
            let pct = (latest * 100.0).round().clamp(0.0, 150.0) as u32;
            let percentage = format!("{pct}%");
            let _ = Command::new("pactl")
                .args([verb, target.as_str(), percentage.as_str()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    });
    tx
}

impl Drop for VolumeTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Drop senders first so the writer threads' `rx.recv()` returns Err
        // and they exit; otherwise they'd linger past app shutdown.
        self.sink_tx = None;
        self.source_tx = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn find_g6_pulse_node(kind: &str, prefix: &str) -> Option<String> {
    let output = Command::new("pactl")
        .args(["list", "short", kind])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    find_g6_node_in_listing(&String::from_utf8_lossy(&output.stdout), prefix)
}

fn find_g6_node_in_listing(listing: &str, prefix: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let name = line.split('\t').nth(1)?;
        (name.starts_with(prefix)
            && name.contains("Sound_BlasterX_G6")
            && name.ends_with("analog-stereo"))
        .then(|| name.to_owned())
    })
}

fn read_sink_volume(target: &str) -> Option<f32> {
    let out = Command::new("pactl")
        .args(["get-sink-volume", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_volume_percent(&String::from_utf8_lossy(&out.stdout))
}

fn read_source_volume(target: &str) -> Option<f32> {
    let out = Command::new("pactl")
        .args(["get-source-volume", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_volume_percent(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the first `NN%` out of `pactl get-sink-volume` / `get-source-volume`
/// output (e.g. `Volume: front-left: ... /  60% / ...`). Returns the value as
/// a fraction where `1.0 == 100 %`.
fn parse_volume_percent(s: &str) -> Option<f32> {
    let pct_idx = s.find('%')?;
    let before = &s[..pct_idx];
    let num_start = before
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    let num: f32 = before[num_start..].trim().parse().ok()?;
    Some(num / 100.0)
}

/// `pactl get-default-sink` → `"<sink-name>.monitor"`, the PulseAudio convention
/// for the loopback source attached to every sink.
fn default_sink_monitor() -> Option<String> {
    let out = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then(|| format!("{name}.monitor"))
}

/// `pactl load-module module-loopback ...` — routes the default mic into the
/// default sink so you hear yourself ("monitoring"). The module ID is captured
/// so `Drop` can unload it cleanly on app exit.
struct Loopback {
    module_id: Option<u32>,
}

impl Loopback {
    fn start() -> Self {
        let out = Command::new("pactl")
            .args([
                "load-module",
                "module-loopback",
                "source=@DEFAULT_SOURCE@",
                "sink=@DEFAULT_SINK@",
                "latency_msec=50",
            ])
            .output();
        let module_id = out.ok().filter(|o| o.status.success()).and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        });
        Self { module_id }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(id) = self.module_id {
            let _ = Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .status();
        }
    }
}

/// OBS-style horizontal level meter. `level` and `peak_hold` are linear sample
/// peaks in `0.0..=1.0`; both are drawn on a -60 dB → 0 dB log scale so quiet
/// signals register visibly. The fill uses a smooth green → yellow → red
/// gradient (1-pixel-wide strips), and the peak-hold value is rendered as a
/// thin floating marker that lingers above the bar before falling.
fn level_bar(ui: &mut egui::Ui, level: f32, peak_hold: f32) {
    let frac = level_to_frac(level);
    let peak_frac = level_to_frac(peak_hold);

    let avail_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, 14.0), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(28, 28, 28));

    // Smooth gradient: paint 1-pixel-wide strips coloured by their position in
    // the *full* bar, clipped to the current fill width. This gives a continuous
    // green→yellow→red transition rather than the previous three hard stops.
    let bar_end_x = rect.min.x + rect.width() * frac;
    let n_pix = rect.width().ceil() as i32;
    for i in 0..n_pix {
        let x0 = rect.min.x + i as f32;
        let x1 = x0 + 1.0;
        if x0 >= bar_end_x {
            break;
        }
        let strip_frac = ((x0 - rect.min.x) / rect.width()).clamp(0.0, 1.0);
        let color = meter_color(strip_frac);
        let strip_rect = egui::Rect::from_min_max(
            egui::pos2(x0, rect.min.y),
            egui::pos2(x1.min(bar_end_x), rect.max.y),
        );
        painter.rect_filled(strip_rect, 0.0, color);
    }

    // Peak-hold marker — a 2-pixel-wide vertical line floating at the most
    // recent peak. Coloured to match its position so it visually matches the
    // bar segment underneath.
    if peak_hold > 0.0 && peak_frac > frac {
        let pk_x = rect.min.x + rect.width() * peak_frac;
        let pk_rect = egui::Rect::from_min_max(
            egui::pos2(pk_x - 1.0, rect.min.y),
            egui::pos2(pk_x + 1.0, rect.max.y),
        );
        painter.rect_filled(pk_rect, 0.0, meter_color(peak_frac));
    }

    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(80)),
        egui::StrokeKind::Inside,
    );
}

/// Sample peak (linear 0..1) → bar fraction (0..1) on a -60 dB → 0 dB scale.
fn level_to_frac(level: f32) -> f32 {
    let level = level.clamp(0.0, 1.0);
    let db = 20.0 * level.max(1e-6).log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Smooth gradient: green at the bottom, blending into yellow around -20 dB
/// and red around -9 dB. Used per-pixel by [`level_bar`].
fn meter_color(frac: f32) -> egui::Color32 {
    const GREEN: (u8, u8, u8) = (70, 200, 70);
    const YELLOW: (u8, u8, u8) = (220, 200, 60);
    const RED: (u8, u8, u8) = (220, 70, 70);
    if frac < 0.67 {
        let t = (frac / 0.67).clamp(0.0, 1.0);
        // Subtle warming of green as we approach the yellow zone — avoids a
        // perfectly flat bottom half.
        lerp_rgb(GREEN, (110, 210, 80), t)
    } else if frac < 0.85 {
        let t = ((frac - 0.67) / (0.85 - 0.67)).clamp(0.0, 1.0);
        lerp_rgb((110, 210, 80), YELLOW, t)
    } else {
        let t = ((frac - 0.85) / (1.00 - 0.85)).clamp(0.0, 1.0);
        lerp_rgb(YELLOW, RED, t)
    }
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// True iff the watch-service unit file already exists in the user's systemd
/// config directory. Matches what `g6-cli service install/uninstall` writes /
/// deletes, so it's a cheap, accurate proxy for the toggle button's state.
fn watch_service_installed() -> bool {
    let base = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base)
        .join("systemd/user/g6-cli-watch.service")
        .exists()
}

/// Build the success/failure banner for the CLI-output modal: icon, colour, and a
/// friendly heading tailored to the command that just finished. `label` is the
/// argv joined with spaces (e.g. `"init --yes"`, `"service install"`).
fn result_banner(label: &str, success: bool) -> (&'static str, egui::Color32, String) {
    let action = match label {
        "init --yes" => "Initialize Audio",
        "service install" => "Install Watch Service",
        "service uninstall" => "Uninstall Watch Service",
        _ => "Run g6-cli",
    };
    let heading = if success {
        format!("{action} — Success")
    } else {
        format!("{action} — Failed")
    };
    let (icon, color) = if success {
        ("\u{2714}", egui::Color32::from_rgb(46, 160, 67)) // heavy check ✔, GitHub green
    } else {
        ("\u{2716}", egui::Color32::from_rgb(218, 54, 51)) // heavy cross ✖, red
    };
    (icon, color, heading)
}

fn card(ui: &mut egui::Ui, title: &str, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).size(15.0).strong());
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
            contents(ui);
        });
    ui.add_space(10.0);
}

/// Read the persisted theme preference. Returns `None` if no file or unparseable.
/// Stored as a plain-text `theme` file in [`g6_core::profile_dir`] (one line:
/// `light`, `dark`, or `system`) — no extension, so it doesn't show up in the
/// `*.json` profile scan.
fn load_saved_theme() -> Option<egui::ThemePreference> {
    let path = g6_core::profile_dir().ok()?.join("theme");
    let s = std::fs::read_to_string(&path).ok()?;
    match s.trim() {
        "light" => Some(egui::ThemePreference::Light),
        "dark" => Some(egui::ThemePreference::Dark),
        "system" => Some(egui::ThemePreference::System),
        _ => None,
    }
}

fn save_theme(theme: egui::ThemePreference) -> std::io::Result<()> {
    let dir = g6_core::ensure_profile_dir().map_err(|e| std::io::Error::other(e.to_string()))?;
    let name = match theme {
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::System => "system",
    };
    std::fs::write(dir.join("theme"), format!("{name}\n"))
}

/// Scan saved profiles and return a name iff exactly one matches every device-read
/// value exactly. Called once at startup so the matching profile gets the same
/// strong/highlighted rendering as a manually selected one.
fn find_unique_matching_profile(
    profiles: &[String],
    values: &HashMap<FeatureId, f32>,
) -> Option<String> {
    let mut hit: Option<String> = None;
    for name in profiles {
        let json = if let Some(s) = builtin_profile_json(name) {
            s.to_string()
        } else {
            let Ok(path) = profile_path(name) else {
                continue;
            };
            let Ok(s) = std::fs::read_to_string(&path) else {
                continue;
            };
            s
        };
        let Ok(profile) = serde_json::from_str::<Profile>(&json) else {
            continue;
        };
        let all_match = profile
            .features
            .iter()
            .all(|e| values.get(&e.id) == Some(&e.value));
        if all_match {
            if hit.is_some() {
                return None; // ambiguous: two profiles encode the same state
            }
            hit = Some(name.clone());
        }
    }
    hit
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn selects_g6_nodes_instead_of_other_or_default_devices() {
        let sinks = concat!(
            "41\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\n",
            "72\talsa_output.usb-Creative_Technology_Ltd_Sound_BlasterX_G6_ABC-00.analog-stereo\tPipeWire\n",
        );
        let sources = concat!(
            "42\talsa_input.usb-Generic_USB_Audio-00.analog-stereo\tPipeWire\n",
            "73\talsa_input.usb-Creative_Technology_Ltd_Sound_BlasterX_G6_ABC-00.analog-stereo\tPipeWire\n",
        );

        assert_eq!(
            find_g6_node_in_listing(sinks, "alsa_output.").as_deref(),
            Some("alsa_output.usb-Creative_Technology_Ltd_Sound_BlasterX_G6_ABC-00.analog-stereo")
        );
        assert_eq!(
            find_g6_node_in_listing(sources, "alsa_input.").as_deref(),
            Some("alsa_input.usb-Creative_Technology_Ltd_Sound_BlasterX_G6_ABC-00.analog-stereo")
        );
    }
}
