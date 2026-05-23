use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use g6_core::{
    Device, FeatureEntry, FeatureId, Profile,
    builtin_profile_json, ensure_profile_dir, is_builtin, list_profile_names, profile_path,
};
use std::io::{IsTerminal, Write};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name        = "g6-cli",
    version,
    author,
    about       = "Creative Sound Blaster X G6 CLI",
    long_about  = LONG_ABOUT,
    after_help  = AFTER_HELP,
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Creative Sound BlasterX G6 (USB 041e:3256) controller -- by Xuda Ye and Claude Code.

Talks to the device's vendor HID interface (interface 4) to read and write the
SBX DSP feature set, EQ, output mode, and DAC filter. Does NOT control playback
volume, mic monitoring, or RGB lighting -- those live in different protocol
layers (UAC for volume, kernel hidraw for buttons, vendor RGB for lighting).

Profiles are stored as <NAME>.json two directories above the g6-cli binary
(the repo root when running from target/release/). The three built-in profiles
(default, scout, sbx) are baked into the binary and always available via
`g6-cli load <PRESET>`.";

const AFTER_HELP: &str = "\
EXAMPLES:
  g6-cli init                   Run once on a new system (ALSA mic/output + udev rule)
  g6-cli status                 Show current device state as a table
  g6-cli status --json          Same, as JSON (for scripting/frontends)
  g6-cli load scout             Apply the stock Scout preset
  g6-cli set Eq1kHz 4.5         Boost 1 kHz by 4.5 dB live (no file changes)
  g6-cli save -n example         Snapshot current state to example.json
  g6-cli load -n example         Restore it later
  g6-cli list                   List all saved profiles
  g6-cli remove -n example      Delete example.json
  g6-cli watch                  Foreground: keep External Mic across PulseAudio resyncs (Ctrl-C to exit)
  g6-cli service install        Install + enable systemd user service that runs `watch` for you
  g6-cli test speaker           Play left/right voice samples through the default audio output
  g6-cli test mic -t 3          Record 3 seconds from the default source, then play it back

NOTE:
  If you do NOT use pavucontrol, `g6-cli init` is all you need -- External Mic
  stays selected until you replug or reboot.

  If you DO use pavucontrol (or anything else that causes PulseAudio to
  re-apply its card profile), the External Mic selection gets silently
  overwritten because PA's stock profile for the G6 only declares Line In as
  a capture port. Run `g6-cli service install` once to enable a systemd user
  service that keeps External Mic enforced across PA resyncs.

  Profiles (load / save / set / get / list / remove) have only been tested
  with headphones. Use with caution while routed to speakers -- the audio
  setup commands (init, service, test) are safe regardless of output.";

#[derive(Subcommand)]
enum Cmd {
    /// Verify the G6 is present and route mic/output through it. Offers to install the udev rule.
    Init(InitArgs),
    /// Apply a profile to the device.
    Load(LoadArgs),
    /// Set a single feature on the device. Takes effect immediately; does not touch any file.
    Set(SetArgs),
    /// Read a single feature from the device. Prints the raw f32 value to stdout.
    Get(GetArgs),
    /// Read the current device state and save it as <NAME>.json (aborts if it already exists).
    Save(SaveArgs),
    /// Delete a saved <NAME>.json profile. The built-ins (default/scout/sbx) are protected.
    Remove(RemoveArgs),
    /// List all saved profiles. * marks built-ins (protected from remove).
    List(ListArgs),
    /// Read every feature from the device and print as a table (or JSON with --json).
    Status(StatusArgs),
    /// Re-apply External Mic on every PulseAudio card/source event. Runs until Ctrl-C.
    Watch,
    /// Manage the g6-cli-watch systemd user service (recommended if you use pavucontrol).
    Service(ServiceArgs),
    /// Run audio diagnostics (speaker).
    Test(TestArgs),
}

#[derive(clap::Args)]
struct TestArgs {
    #[command(subcommand)]
    target: TestTarget,
}

#[derive(Subcommand)]
enum TestTarget {
    /// Play "front left" then "front right" through the default audio output.
    Speaker,
    /// Record N seconds from the default source, then play it back.
    Mic(MicArgs),
}

#[derive(clap::Args)]
struct MicArgs {
    /// Recording duration in seconds.
    #[arg(short = 't', long, default_value_t = 3)]
    seconds: u32,
}

#[derive(clap::Args)]
struct ServiceArgs {
    #[command(subcommand)]
    action: ServiceAction,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write ~/.config/systemd/user/g6-cli-watch.service and enable+start it.
    Install,
    /// Stop, disable, and remove the systemd user service.
    Uninstall,
    /// Show the current state of the systemd user service.
    Status,
}

#[derive(clap::Args)]
struct InitArgs {
    /// Auto-install the udev rule via sudo if missing (no interactive prompt).
    #[arg(short = 'y', long, conflicts_with = "no")]
    yes: bool,
    /// Skip the udev rule install entirely (no interactive prompt).
    #[arg(long)]
    no: bool,
}

#[derive(clap::Args)]
struct StatusArgs {
    /// Print JSON to stdout instead of the human-readable table.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct LoadArgs {
    /// Built-in preset to apply.
    #[arg(value_enum, conflicts_with = "name")]
    preset: Option<Preset>,

    /// Load <NAME>.json from two directories above the g6-cli binary (e.g. the repo root when running from target/release/).
    #[arg(short = 'n', long, value_name = "NAME")]
    name: Option<String>,
}

#[derive(clap::Args)]
struct SaveArgs {
    /// Target file: <NAME>.json written two directories above the g6-cli binary.
    #[arg(short = 'n', long, value_name = "NAME")]
    name: String,
}

#[derive(clap::Args)]
struct RemoveArgs {
    /// Profile file to delete: <NAME>.json (e.g. -n example). No-op if the file does not exist.
    #[arg(short = 'n', long, value_name = "NAME")]
    name: String,
}

#[derive(clap::Args)]
struct SetArgs {
    /// Feature name as it appears in the JSON files (e.g. SbxMaster, Eq1kHz, SurroundLevel).
    label: String,
    /// New value (toggle: 0 or 1; level: 0.0-1.0; EQ band: dB; Output: 0=Speakers, 1=Headphones; DacFilter: 0-4).
    #[arg(allow_hyphen_values = true)]
    value: f32,
}

#[derive(clap::Args)]
struct GetArgs {
    /// Feature name as it appears in the JSON files.
    label: String,
}

#[derive(clap::Args)]
struct ListArgs {
    /// Print JSON array on stdout instead of the human-readable list.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, clap::ValueEnum)]
enum Preset {
    Default,
    Scout,
    Sbx,
}

const CARD: &str = "G6";
const UDEV_RULE_PATH:    &str = "/etc/udev/rules.d/91-soundblaster-g6.rules";
const UDEV_RULE_CONTENT: &str = include_str!("../../../udev/91-soundblaster-g6.rules");

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init(args)   => init(args),
        Cmd::Load(args)   => load(args),
        Cmd::Set(args)    => set(args),
        Cmd::Get(args)    => get(args),
        Cmd::Save(args)   => save(args),
        Cmd::Remove(args) => remove(args),
        Cmd::List(args)   => list(args),
        Cmd::Status(args) => status(args),
        Cmd::Watch        => watch(),
        Cmd::Service(args) => match args.action {
            ServiceAction::Install   => service_install(),
            ServiceAction::Uninstall => service_uninstall(),
            ServiceAction::Status    => service_status(),
        },
        Cmd::Test(args) => match args.target {
            TestTarget::Speaker  => test_speaker(),
            TestTarget::Mic(m)   => test_mic(m.seconds),
        },
    }
}

fn status(args: StatusArgs) -> Result<()> {
    let profile = read_device_state()?;
    if args.json {
        let mut json = serde_json::to_string_pretty(&profile)?;
        json.push('\n');
        print!("{}", json);
    } else {
        print_status(&profile);
    }
    Ok(())
}

fn print_status(p: &Profile) {
    let v = |id: FeatureId| -> f32 {
        p.features.iter().find(|f| f.id == id).map(|f| f.value).unwrap_or(f32::NAN)
    };
    let on_off  = |val: f32| if val > 0.5 { "on" } else { "off" };
    let pct     = |val: f32| format!("{:>3}%", (val * 100.0).round() as i32);
    let db      = |val: f32| format!("{:>+5.1} dB", val);
    let output  = |val: f32| match val as i32 {
        0 => "Speakers",
        1 => "Headphones",
        _ => "unknown",
    };
    let dac     = |val: f32| match val as i32 {
        0 => "Fast Roll-off, Minimum Phase",
        1 => "Slow Roll-off, Minimum Phase",
        2 => "NOS (Non-Oversampling)",
        3 => "Fast Roll-off, Linear Phase",
        4 => "Slow Roll-off, Linear Phase",
        _ => "unknown",
    };
    let sv_mode = |val: f32| match val as i32 {
        0 => "Normal",
        1 => "Loud",
        2 => "Night",
        _ => "unknown",
    };

    println!();
    println!("  SBX Master:   {}", on_off(v(FeatureId::SbxMaster)));
    println!("  Scout Mode:   {}", on_off(v(FeatureId::ScoutMode)));
    println!("  Output:       {}", output(v(FeatureId::Output)));
    println!("  DAC Filter:   {}", dac(v(FeatureId::DacFilter)));
    println!();
    println!("  Surround:     {:>3}  {}",
        on_off(v(FeatureId::SurroundToggle)),  pct(v(FeatureId::SurroundLevel)));
    println!("  Dialog+:      {:>3}  {}",
        on_off(v(FeatureId::DialogPlusToggle)), pct(v(FeatureId::DialogPlusLevel)));
    println!("  Smart Vol:    {:>3}  {}  ({})",
        on_off(v(FeatureId::SmartVolToggle)),
        pct(v(FeatureId::SmartVolLevel)),
        sv_mode(v(FeatureId::SmartVolMode)));
    println!("  Crystalizer:  {:>3}  {}",
        on_off(v(FeatureId::CrystalizerToggle)), pct(v(FeatureId::CrystalizerLevel)));
    println!("  Bass:         {:>3}  {}",
        on_off(v(FeatureId::BassToggle)),        pct(v(FeatureId::BassLevel)));
    println!("  Surround Distance: {:.0} cm", v(FeatureId::SurroundDistance));
    println!();
    println!("  Equalizer:    {}", on_off(v(FeatureId::EqToggle)));
    println!("    Pre-amp: {}", db(v(FeatureId::EqPreAmp)));
    for (id, label) in [
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
    ] {
        println!("    {:>6}: {}", label, db(v(id)));
    }
    println!();
}

fn set(args: SetArgs) -> Result<()> {
    let id = parse_feature_id(&args.label)?;
    id.validate_value(args.value)?;
    let device = Device::open()?;
    device.write_feature(id, args.value)?;
    eprintln!("set:      {} = {}", args.label, args.value);
    Ok(())
}

fn get(args: GetArgs) -> Result<()> {
    let id = parse_feature_id(&args.label)?;
    let mut device = Device::open()?;
    if !device.probe() {
        eprintln!("device:   waking via USB reset (~3s)...");
        drop(device);
        Device::reset_usb()?;
        device = Device::open()?;
    }
    let value = device.read_feature(id)?;
    println!("{value}");
    Ok(())
}

fn parse_feature_id(label: &str) -> Result<FeatureId> {
    serde_json::from_str(&format!("\"{label}\""))
        .with_context(|| format!("unknown feature '{label}'"))
}

fn save(args: SaveArgs) -> Result<()> {
    let stem = args.name.strip_suffix(".json").unwrap_or(&args.name);
    if is_builtin(stem) {
        bail!("{stem} is a reserved built-in profile name -- pick a different name");
    }
    ensure_profile_dir()?;
    let path = profile_path(&args.name)?;
    if path.exists() {
        bail!("{} already exists -- refusing to overwrite", path.display());
    }
    let profile = read_device_state()?;
    let mut json = serde_json::to_string_pretty(&profile)?;
    json.push('\n');
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("saved:    {} features -> {}", profile.features.len(), path.display());
    Ok(())
}

fn list(args: ListArgs) -> Result<()> {
    let names = list_profile_names();
    if args.json {
        let entries: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n, "builtin": is_builtin(n) }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for name in &names {
            let mark = if is_builtin(name) { "*" } else { " " };
            println!("  {} {}", mark, name);
        }
    }
    Ok(())
}

fn remove(args: RemoveArgs) -> Result<()> {
    let stem = args.name.strip_suffix(".json").unwrap_or(&args.name);
    if is_builtin(stem) {
        bail!("{} is a built-in profile and cannot be removed", stem);
    }
    let path = profile_path(&args.name)?;
    if !path.exists() {
        return Ok(());
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    eprintln!("removed:  {}", path.display());
    Ok(())
}

fn read_device_state() -> Result<Profile> {
    let mut device = Device::open()?;
    if !device.probe() {
        eprintln!("device:   waking via USB reset (~3s)...");
        drop(device);
        Device::reset_usb()?;
        device = Device::open()?;
    }
    let features: Vec<FeatureEntry> = FeatureId::ALL
        .iter()
        .map(|&id| device.read_feature(id).map(|value| FeatureEntry { id, value }))
        .collect::<Result<_>>()?;
    Ok(Profile { features })
}

fn load(args: LoadArgs) -> Result<()> {
    if let Some(name) = args.name {
        return apply_by_name(&name);
    }
    let Some(preset) = args.preset else {
        bail!("specify a preset (default/scout/sbx) or -n <name>");
    };
    let stem = match preset {
        Preset::Default => "default",
        Preset::Scout   => "scout",
        Preset::Sbx     => "sbx",
    };
    apply(stem, builtin_profile_json(stem).expect("preset variant maps to a built-in"))
}

fn apply_by_name(name: &str) -> Result<()> {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    if let Some(json) = builtin_profile_json(stem) {
        return apply(stem, json);
    }
    let path = profile_path(name)?;
    if !path.exists() {
        bail!("{} not found", path.display());
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    apply(stem, &json)
}

// ─── init ────────────────────────────────────────────────────────────────────

enum UdevChoice { Yes, No, Ask }

fn init(args: InitArgs) -> Result<()> {
    let choice = if args.yes { UdevChoice::Yes }
                 else if args.no { UdevChoice::No }
                 else { UdevChoice::Ask };
    require_card()?;
    set_capture_source()?;
    eprintln!("alsa:     capture source = External Mic @ 100%");
    set_pipewire_default("sinks",   "alsa_output.", "sink",   "set-default-sink")?;
    set_pipewire_default("sources", "alsa_input.",  "source", "set-default-source")?;
    ensure_udev_rule(choice)?;
    eprintln!("hint:     if you don't use pavucontrol, you're done.");
    eprintln!("          if you do, run `g6-cli service install` once to keep External Mic across PA resyncs.");
    Ok(())
}

fn require_card() -> Result<()> {
    let cards = std::fs::read_to_string("/proc/asound/cards")
        .context("reading /proc/asound/cards")?;
    if !cards.contains("Sound BlasterX G6") {
        bail!("Sound BlasterX G6 not detected -- is it plugged in?");
    }
    eprintln!("device:   Sound BlasterX G6 detected");
    Ok(())
}

fn set_capture_source() -> Result<()> {
    amixer(&["sset", "PCM Capture Source", "External Mic"])?;
    amixer(&["sset", "External Mic", "Capture", "100%", "cap"])?;
    Ok(())
}

// ─── systemd user service ────────────────────────────────────────────────────

const SERVICE_NAME: &str = "g6-cli-watch.service";

fn service_file_path() -> Result<std::path::PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        format!("{}/.config", std::env::var("HOME").unwrap_or_default())
    });
    Ok(std::path::PathBuf::from(base).join("systemd/user").join(SERVICE_NAME))
}

fn service_install() -> Result<()> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let path = service_file_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating systemd user dir")?;
    }
    let content = format!("\
[Unit]
Description=Sound BlasterX G6 -- keep capture source on External Mic
After=pipewire-pulse.service pulseaudio.service

[Service]
Type=simple
ExecStart={} watch
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
", exe.display());
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("service:  wrote {}", path.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", SERVICE_NAME])?;
    eprintln!("service:  enabled and started ({SERVICE_NAME})");
    Ok(())
}

fn service_uninstall() -> Result<()> {
    let path = service_file_path()?;
    if !path.exists() {
        eprintln!("service:  not installed");
        return Ok(());
    }
    // disable+stop best-effort; ignore errors so we still remove the file
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .status();
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    eprintln!("service:  removed {}", path.display());
    Ok(())
}

fn service_status() -> Result<()> {
    std::process::Command::new("systemctl")
        .args(["--user", "status", SERVICE_NAME, "--no-pager"])
        .status()
        .context("running systemctl --user status")?;
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let ok = std::process::Command::new("systemctl")
        .args(["--user"])
        .args(args)
        .status()
        .context("running systemctl --user (is systemd available?)")?
        .success();
    if !ok {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

fn test_mic(seconds: u32) -> Result<()> {
    let path = std::env::temp_dir().join("g6-cli-mic-test.wav");
    eprintln!("test:     recording {seconds}s from default source...");
    let ok = Command::new("arecord")
        .args(["-d", &seconds.to_string(), "-f", "cd", "-q"])
        .arg(&path)
        .status()
        .context("running arecord (is alsa-utils installed?)")?
        .success();
    if !ok {
        let _ = std::fs::remove_file(&path);
        bail!("arecord failed");
    }
    eprintln!("test:     playing back...");
    let play_ok = Command::new("paplay")
        .arg(&path)
        .status()
        .context("running paplay (is pipewire-pulse installed?)")?
        .success();
    let _ = std::fs::remove_file(&path);
    if !play_ok {
        bail!("paplay failed");
    }
    Ok(())
}

fn test_speaker() -> Result<()> {
    for file in ["Front_Left.wav", "Front_Right.wav"] {
        let path = format!("/usr/share/sounds/alsa/{file}");
        if !std::path::Path::new(&path).exists() {
            bail!("{} not found (install alsa-utils)", path);
        }
        eprintln!("test:     playing {file}");
        let ok = Command::new("paplay")
            .arg(&path)
            .status()
            .context("running paplay (is pipewire-pulse installed?)")?
            .success();
        if !ok {
            bail!("paplay {} failed", path);
        }
    }
    Ok(())
}

fn watch() -> Result<()> {
    use std::io::{BufRead, BufReader};
    set_capture_source()?;
    eprintln!("watch:    External Mic @ 100% enforced; listening for PulseAudio events (Ctrl-C to exit)");
    let mut child = std::process::Command::new("pactl")
        .arg("subscribe")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("running pactl subscribe (is pipewire-pulse installed?)")?;
    let stdout = child.stdout.take().context("pactl subscribe has no stdout")?;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        // pactl emits e.g. "Event 'change' on card #56" / "Event 'change' on source #134"
        if line.starts_with("Event ") && (line.contains("on card") || line.contains("on source")) {
            let _ = set_capture_source();
        }
    }
    Ok(())
}

fn amixer(args: &[&str]) -> Result<()> {
    let ok = Command::new("amixer")
        .args(["-c", CARD, "-q"])
        .args(args)
        .status()
        .context("running amixer (is alsa-utils installed?)")?
        .success();
    if !ok {
        bail!("amixer {:?} failed", args);
    }
    Ok(())
}

fn set_pipewire_default(
    kind:   &str,
    prefix: &str,
    label:  &str,
    verb:   &str,
) -> Result<()> {
    let Some(name) = find_g6_node(kind, prefix)? else {
        eprintln!("pipewire: no G6 {label} found, skipping");
        return Ok(());
    };
    let ok = Command::new("pactl")
        .args([verb, &name])
        .status()
        .context("running pactl (is pipewire-pulse installed?)")?
        .success();
    if !ok {
        bail!("pactl {} {} failed", verb, name);
    }
    eprintln!("pipewire: {label} -> {name}");
    Ok(())
}

fn ensure_udev_rule(choice: UdevChoice) -> Result<()> {
    if std::path::Path::new(UDEV_RULE_PATH).exists() {
        eprintln!("udev:     rule already installed at {UDEV_RULE_PATH}");
        return Ok(());
    }
    eprintln!("udev:     not installed at {UDEV_RULE_PATH}");
    let install = match choice {
        UdevChoice::Yes => true,
        UdevChoice::No  => false,
        UdevChoice::Ask => {
            eprintln!("          Without it, every `g6-cli set/load/save/status` will require sudo.");
            eprint!  ("          Install now via sudo? [y/N] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }
    };
    if !install {
        eprintln!("udev:     skipped -- rerun `g6-cli init` (optionally with --yes) to install later");
        return Ok(());
    }
    install_udev_rule()
}

fn install_udev_rule() -> Result<()> {
    let temp = std::env::temp_dir().join("91-soundblaster-g6.rules");
    std::fs::write(&temp, UDEV_RULE_CONTENT).context("writing temp rule file")?;
    // One elevated shell runs all three steps so the user is prompted for the password once.
    let script = format!(
        "/usr/bin/install -m644 '{temp_path}' '{rule_path}' && \
         /usr/bin/udevadm control --reload-rules && \
         /usr/bin/udevadm trigger",
        temp_path = temp.to_str().unwrap(),
        rule_path = UDEV_RULE_PATH,
    );
    elevate(&["/bin/sh", "-c", &script])?;
    let _ = std::fs::remove_file(&temp);
    eprintln!("udev:     rule installed at {UDEV_RULE_PATH}; replug device if the next set command fails");
    Ok(())
}

/// Run a command with root privileges. Picks `pkexec` if a polkit authentication
/// agent is actually running, falls back to `sudo` when a TTY is available, and
/// bails with a clear message if neither is usable.
fn elevate(args: &[&str]) -> Result<()> {
    let graphical = std::env::var_os("WAYLAND_DISPLAY").is_some()
                 || std::env::var_os("DISPLAY").is_some();
    let tty = std::io::stdin().is_terminal();

    let tool = if graphical && polkit_agent_running() {
        "pkexec"
    } else if tty {
        "sudo"
    } else {
        bail!(
            "cannot prompt for elevation: no polkit authentication agent detected and no terminal.\n\
             Either run `g6-cli init` from a terminal (uses sudo), or install/start one of:\n\
             hyprpolkitagent, polkit-gnome, polkit-kde-agent-1, lxqt-policykit, mate-polkit, lxpolkit"
        );
    };
    let ok = Command::new(tool)
        .args(args)
        .status()
        .with_context(|| format!("running {tool}"))?
        .success();
    if !ok {
        bail!("{tool} {} failed", args.join(" "));
    }
    Ok(())
}

/// Scan `/proc/*/comm` for a known polkit authentication agent. The kernel
/// truncates `comm` to 15 chars (TASK_COMM_LEN-1), so the patterns below are
/// the truncated forms of each agent's process name.
fn polkit_agent_running() -> bool {
    const AGENT_COMMS: &[&str] = &[
        "polkit-gnome-au", // polkit-gnome-authentication-agent-1 (GNOME, XFCE default)
        "polkit-kde-auth", // polkit-kde-authentication-agent-1   (KDE)
        "polkit-mate-aut", // polkit-mate-authentication-agent-1  (MATE)
        "hyprpolkitagent", // Hyprland (15 chars, fits exactly)
        "lxqt-policykit-", // lxqt-policykit-agent                (LXQt)
        "lxpolkit",        // generic / LXDE
    ];
    let Ok(entries) = std::fs::read_dir("/proc") else { return false; };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
        if !name.bytes().all(|b| b.is_ascii_digit()) { continue; }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue; };
        if AGENT_COMMS.contains(&comm.trim()) {
            return true;
        }
    }
    false
}

fn find_g6_node(kind: &str, prefix: &str) -> Result<Option<String>> {
    let out = Command::new("pactl")
        .args(["list", "short", kind])
        .output()
        .context("running pactl")?;
    if !out.status.success() {
        return Ok(None);
    }
    for line in std::str::from_utf8(&out.stdout)?.lines() {
        let Some(name) = line.split('\t').nth(1) else { continue };
        if name.starts_with(prefix)
            && name.contains("Sound_BlasterX_G6")
            && name.ends_with("analog-stereo")
        {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

// ─── set ─────────────────────────────────────────────────────────────────────

fn apply(label: &str, json: &str) -> Result<()> {
    let profile: Profile = serde_json::from_str(json).context("parsing profile JSON")?;
    for entry in &profile.features {
        entry.id.validate_value(entry.value)
            .with_context(|| format!("invalid value in profile {label}"))?;
    }
    // The G6 keeps separate DSP slots per Output. Sort so any Output change
    // happens first, then all the DSP/global writes land in the right slot.
    let mut entries = profile.features.clone();
    entries.sort_by_key(|e| if e.id == FeatureId::Output { 0 } else { 1 });
    let device = Device::open()?;
    eprintln!("device:   opened G6 HID interface 4");
    eprintln!("profile:  {} ({} features)", label, entries.len());
    for entry in &entries {
        device.write_feature(entry.id, entry.value)?;
        let wait = if entry.id == FeatureId::Output { 60 } else { 20 };
        sleep(Duration::from_millis(wait));
    }
    eprintln!("status:   {} features written", entries.len());
    Ok(())
}
