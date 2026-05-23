use eframe::egui;
use g6_core::{
    Device, FeatureEntry, FeatureId, Profile,
    builtin_profile_json, ensure_profile_dir, is_builtin, list_profile_names, profile_path,
};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DAC_FILTERS: &[&str] = &[
    "Fast Roll-off, Minimum Phase",
    "Slow Roll-off, Minimum Phase",
    "NOS (Non-Oversampling)",
    "Fast Roll-off, Linear Phase",
    "Slow Roll-off, Linear Phase",
];

const SMART_VOL_MODES: &[&str] = &["Normal", "Loud", "Night"];

const EQ_BANDS: &[(FeatureId, &str)] = &[
    (FeatureId::Eq31Hz,  "31 Hz"),
    (FeatureId::Eq62Hz,  "62 Hz"),
    (FeatureId::Eq125Hz, "125 Hz"),
    (FeatureId::Eq250Hz, "250 Hz"),
    (FeatureId::Eq500Hz, "500 Hz"),
    (FeatureId::Eq1kHz,  "1 kHz"),
    (FeatureId::Eq2kHz,  "2 kHz"),
    (FeatureId::Eq4kHz,  "4 kHz"),
    (FeatureId::Eq8kHz,  "8 kHz"),
    (FeatureId::Eq16kHz, "16 kHz"),
];

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("g6-gui")  // for Hyprland: windowrulev2 = float, class:^(g6-gui)$
            .with_title("Creative Sound BlasterX G6 Control")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Creative Sound BlasterX G6 Control",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)) as Box<dyn eframe::App>)),
    )
}

/// Locate the g6-cli binary that sits next to this g6-gui binary.
fn cli_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("g6-cli")))
        .unwrap_or_else(|| std::path::PathBuf::from("g6-cli"))
}

struct App {
    device:      Arc<Mutex<Option<Device>>>,
    values:      HashMap<FeatureId, f32>,
    profiles:    Vec<String>,
    selected:    Option<String>,
    new_name:    String,
    status:      String,
    last_output: String,
    pending:     Option<(String, Receiver<CliOutput>)>,
    last_theme:  egui::ThemePreference,
}

struct CliOutput {
    header:    String,
    stderr:    String,
    stdout:    String,
    success:   bool,
    exit_code: Option<i32>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        // Restore the theme preference saved on the previous run (if any).
        if let Some(t) = load_saved_theme() {
            ctx.options_mut(|opt| opt.theme_preference = t);
        }
        let last_theme = ctx.options(|opt| opt.theme_preference);

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
        let profiles  = list_profile_names();
        let selected  = if connected { find_unique_matching_profile(&profiles, &values) } else { None };
        let status = if !connected {
            "Device not connected. Run `g6-cli init` (or use the right panel), then reopen.".into()
        } else if let Some(name) = &selected {
            format!("Read {} features from device ({} matches current state)", values.len(), name)
        } else {
            format!("Read {} features from device", values.len())
        };
        Self {
            device: Arc::new(Mutex::new(device_opt)),
            values, profiles,
            selected,
            new_name: String::new(),
            status,
            last_output: String::new(),
            pending: None,
            last_theme,
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
        let result = self.device.lock().unwrap().as_ref()
            .map(|d| d.write_feature(id, value));
        match result {
            Some(Ok(()))  => {
                self.values.insert(id, value);
                self.status = format!("{id:?} = {value}");
                if id == FeatureId::Output {
                    // The G6 keeps separate DSP slots per output; re-read so
                    // the sliders show the new slot's stored values.
                    std::thread::sleep(Duration::from_millis(60));
                    self.refresh();
                }
            }
            Some(Err(e))  => self.status = format!("Write failed: {e}"),
            None          => self.status = "Device not connected".into(),
        }
    }

    fn refresh(&mut self) {
        let guard = self.device.lock().unwrap();
        let Some(d) = guard.as_ref() else { return; };
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
                Err(e) => { self.status = e.to_string(); return; }
            };
            match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => { self.status = format!("Read failed: {e}"); return; }
            }
        };
        let profile: Profile = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => { self.status = format!("Parse failed: {e}"); return; }
        };

        // If the profile flips Output, write Output FIRST (synchronously) and
        // refresh from the new slot before scheduling the rest -- otherwise the
        // DSP writes would land in the wrong firmware slot.
        let target_output = profile.features.iter()
            .find(|e| e.id == FeatureId::Output)
            .map(|e| e.value);
        if let Some(out_val) = target_output {
            if self.values.get(&FeatureId::Output).copied() != Some(out_val) {
                let wrote_ok = self.device.lock().unwrap().as_ref()
                    .map(|d| d.write_feature(FeatureId::Output, out_val).is_ok())
                    .unwrap_or(false);
                if wrote_ok {
                    std::thread::sleep(Duration::from_millis(60));
                    self.refresh();
                }
            }
        }

        // Remaining writes against the (possibly refreshed) local state, sans Output.
        let to_write: Vec<FeatureEntry> = profile.features.iter()
            .filter(|e| e.id != FeatureId::Output)
            .filter(|e| self.values.get(&e.id).copied() != Some(e.value))
            .copied()
            .collect();
        let total   = profile.features.len();
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
        let ctx2   = ctx.clone();
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
            Err(e) => { self.status = e.to_string(); return; }
        };
        if path.exists() {
            self.status = format!("{} already exists", path.display());
            return;
        }
        let features: Vec<FeatureEntry> = FeatureId::ALL.iter()
            .map(|&id| FeatureEntry { id, value: self.val(id) })
            .collect();
        let count = features.len();
        let mut json = match serde_json::to_string_pretty(&Profile { features }) {
            Ok(s) => s,
            Err(e) => { self.status = format!("Serialize failed: {e}"); return; }
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
            Err(e) => { self.status = e.to_string(); return; }
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

    fn spawn_cli(&mut self, args: &[&str]) {
        let path = cli_path();
        match Command::new(&path).args(args).spawn() {
            Ok(_)  => self.status = format!("Started: {} {}", path.display(), args.join(" ")),
            Err(e) => self.status = format!("Failed to spawn {}: {e}", path.display()),
        }
    }

    /// Spawn a g6-cli command on a background thread so the GUI stays responsive
    /// while pkexec/sudo waits for the password. Result is polled in `update()`.
    fn start_cli(&mut self, ctx: &egui::Context, args: &[&str]) {
        let path  = cli_path();
        let label = args.join(" ");
        let header = format!("$ {} {}\n", path.display(), &label);
        let args_owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let out = match Command::new(&path).args(&args_owned).output() {
                Ok(out) => CliOutput {
                    header,
                    stderr:    String::from_utf8_lossy(&out.stderr).into_owned(),
                    stdout:    String::from_utf8_lossy(&out.stdout).into_owned(),
                    success:   out.status.success(),
                    exit_code: out.status.code(),
                },
                Err(e) => CliOutput {
                    header,
                    stderr:    format!("error: {e}\n"),
                    stdout:    String::new(),
                    success:   false,
                    exit_code: None,
                },
            };
            let _ = tx.send(out);
            ctx2.request_repaint();
        });
        self.last_output = format!("$ g6-cli {label}\n(running... a polkit password prompt may appear)\n");
        self.status      = format!("running g6-cli {label}...");
        self.pending     = Some((label, rx));
    }

    /// Pick up a finished background command and surface its output.
    fn poll_pending(&mut self) {
        let Some((label, rx)) = &self.pending else { return; };
        match rx.try_recv() {
            Ok(out) => {
                self.last_output = format!("{}{}{}", out.header, out.stderr, out.stdout);
                self.status = if out.success {
                    format!("g6-cli {label} succeeded")
                } else {
                    format!("g6-cli {label} failed (exit {:?})", out.exit_code)
                };
                self.pending = None;
            }
            Err(TryRecvError::Empty)        => {}
            Err(TryRecvError::Disconnected) => {
                self.status = "subprocess thread vanished".into();
                self.pending = None;
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_pending();

        // Persist Light/Dark/System whenever the user toggles it.
        let current_theme = ctx.options(|opt| opt.theme_preference);
        if current_theme != self.last_theme {
            let _ = save_theme(current_theme);
            self.last_theme = current_theme;
        }

        egui::TopBottomPanel::top("toolbar")
            .exact_height(48.0)
            .show(ctx, |ui| {
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
        egui::SidePanel::left("profiles")
            .resizable(true)
            .default_width(220.0)
            .min_width(180.0)
            .max_width(280.0)
            .show(ctx, |ui| { self.ui_profiles(ui); });
        egui::SidePanel::right("actions")
            .resizable(true)
            .default_width(220.0)
            .min_width(180.0)
            .max_width(280.0)
            .show(ctx, |ui| { self.ui_actions(ui); });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.ui_features(ui);
            });
        });
    }
}

impl App {
    fn ui_profiles(&mut self, ui: &mut egui::Ui) {
        card(ui, "Profiles", |ui| {
            let profiles = self.profiles.clone();
            for name in &profiles {
                let star = if is_builtin(name) { "*" } else { " " };
                let selected = self.selected.as_deref() == Some(name.as_str());
                let mut text = egui::RichText::new(format!("{star} {name}"));
                if selected { text = text.strong(); }
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
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        let busy = self.pending.is_some();
        card(ui, "Setup", |ui| {
            if full_width_button_enabled(ui, !busy, "Audio Initialize")
                .on_hover_text("Set ALSA mic + PipeWire defaults; install udev rule if missing")
                .clicked()
            {
                self.start_cli(ui.ctx(), &["init", "--yes"]);
            }
            if full_width_button_enabled(ui, !busy, "Install Watch Service")
                .on_hover_text("Systemd user service that keeps External Mic across PulseAudio resyncs")
                .clicked()
            {
                self.start_cli(ui.ctx(), &["service", "install"]);
            }
            if full_width_button_enabled(ui, !busy, "Uninstall Watch Service").clicked() {
                self.start_cli(ui.ctx(), &["service", "uninstall"]);
            }
        });

        card(ui, "Output", |ui| {
            egui::ScrollArea::vertical()
                .max_height(160.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.last_output.is_empty() {
                        ui.weak("(no output yet)");
                    } else {
                        ui.label(egui::RichText::new(&self.last_output).monospace().size(11.0));
                    }
                });
        });

        card(ui, "Test", |ui| {
            if full_width_button(ui, "Test Speaker").clicked() {
                self.spawn_cli(&["test", "speaker"]);
            }
            if full_width_button(ui, "Test Mic (3s)").clicked() {
                self.spawn_cli(&["test", "mic", "-t", "3"]);
            }
        });
    }

    fn ui_features(&mut self, ui: &mut egui::Ui) {
        if !self.has_device() {
            ui.colored_label(egui::Color32::DARK_RED, "Device not connected.");
            ui.label("Use the Setup panel on the right, then reopen this window.");
            return;
        }

        // Sliders grow with the column width, but keep them compact-first.
        let avail = ui.available_width();
        ui.spacing_mut().slider_width = (avail - 240.0).clamp(140.0, 260.0);
        // Default interact_size.x is 40, which inflates the checkbox column to
        // 40 px even though the visible glyph is ~14 px. Tighten it so the box
        // sits right next to its label.
        ui.spacing_mut().interact_size.x = 18.0;

        card(ui, "Global",      |ui| { self.global_grid(ui); });
        card(ui, "SBX Effects", |ui| { self.sbx_grid(ui);    });
        card(ui, "Equalizer",   |ui| { self.eq_grid(ui);     });
        card(ui, "Notes", |ui| {
            ui.label("• Audio Initialize, Watch Service, and Test are safe for any output.");
            ui.label("• Profiles in this build have only been tested with headphones.");
            ui.label("• Loading or saving a profile while routed to speakers may behave unexpectedly.");
        });
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
                self.grid_combo(ui, FeatureId::Output,    "Output",     &["Speakers", "Headphones"]);
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
                self.grid_fx(ui, FeatureId::SurroundToggle,    FeatureId::SurroundLevel,    "Surround");
                ui.label(""); ui.end_row();

                // Distance lives right under Surround (it controls the same effect)
                // and is a label-only row (no toggle of its own).
                ui.label("");
                ui.label("Distance");
                let mut v = self.val(FeatureId::SurroundDistance);
                let resp = ui.add(
                    egui::Slider::new(&mut v, 10.0..=300.0)
                        .custom_formatter(|n, _| format!("{:>3.0} cm", n)),
                );
                if resp.changed() { self.values.insert(FeatureId::SurroundDistance, v); }
                if resp.drag_stopped() || resp.lost_focus() {
                    self.write(FeatureId::SurroundDistance, v);
                }
                ui.label(""); ui.end_row();

                self.grid_fx(ui, FeatureId::DialogPlusToggle,  FeatureId::DialogPlusLevel,  "Dialog+");
                ui.label(""); ui.end_row();

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
                if next != current { self.write(FeatureId::SmartVolMode, next as f32); }
                ui.end_row();

                self.grid_fx(ui, FeatureId::CrystalizerToggle, FeatureId::CrystalizerLevel, "Crystalizer");
                ui.label(""); ui.end_row();

                self.grid_fx(ui, FeatureId::BassToggle,        FeatureId::BassLevel,        "Bass");
                ui.label(""); ui.end_row();
            });
    }

    fn eq_grid(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("g_eq")
            .num_columns(3)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                self.grid_check(ui, FeatureId::EqToggle);
                ui.label("EQ Enable");
                ui.label(""); ui.end_row();

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
        if next != current { self.write(id, next as f32); }
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
                .custom_parser(|s| s.trim_end_matches('%').trim().parse::<f64>().ok().map(|n| n / 100.0)),
        );
        if resp.changed() { self.values.insert(id, v); }
        if resp.drag_stopped() || resp.lost_focus() { self.write(id, v); }
    }

    /// Columns 1-3: empty | label | dB slider. Fixed-width value (always 5 chars
    /// with sign) so column 3 never resizes between -12.0 / +0.0 / +12.0 etc.
    fn grid_band(&mut self, ui: &mut egui::Ui, id: FeatureId, label: &str, range: RangeInclusive<f32>) {
        ui.label("");
        ui.label(label);
        let mut v = self.val(id);
        let resp = ui.add(
            egui::Slider::new(&mut v, range)
                .custom_formatter(|n, _| format!("{:>+5.1} dB", n)),
        );
        if resp.changed() { self.values.insert(id, v); }
        if resp.drag_stopped() || resp.lost_focus() { self.write(id, v); }
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
        "light"  => Some(egui::ThemePreference::Light),
        "dark"   => Some(egui::ThemePreference::Dark),
        "system" => Some(egui::ThemePreference::System),
        _ => None,
    }
}

fn save_theme(theme: egui::ThemePreference) -> std::io::Result<()> {
    let dir = g6_core::ensure_profile_dir()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let name = match theme {
        egui::ThemePreference::Light  => "light",
        egui::ThemePreference::Dark   => "dark",
        egui::ThemePreference::System => "system",
    };
    std::fs::write(dir.join("theme"), format!("{name}\n"))
}

/// Scan saved profiles and return a name iff exactly one matches every device-read
/// value exactly. Called once at startup so the matching profile gets the same
/// strong/highlighted rendering as a manually selected one.
fn find_unique_matching_profile(
    profiles: &[String],
    values:   &HashMap<FeatureId, f32>,
) -> Option<String> {
    let mut hit: Option<String> = None;
    for name in profiles {
        let json = if let Some(s) = builtin_profile_json(name) {
            s.to_string()
        } else {
            let Ok(path) = profile_path(name) else { continue; };
            let Ok(s) = std::fs::read_to_string(&path) else { continue; };
            s
        };
        let Ok(profile) = serde_json::from_str::<Profile>(&json) else { continue; };
        let all_match = profile.features.iter()
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
