//! G6 protocol layer: feature catalog, validation, USB HID reads/writes,
//! and the on-disk Profile/FeatureEntry types shared by the CLI and GUI.

use anyhow::{Context, Result, anyhow, bail};
use hidapi::{HidApi, HidDevice};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One feature's value as stored on disk and on the device.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct FeatureEntry {
    pub id:    FeatureId,
    pub value: f32,
}

/// A complete profile snapshot (28 features).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub features: Vec<FeatureEntry>,
}

/// Resolve a profile filename to its on-disk path under `profile_dir()`.
/// Note: for the three built-in names this path may not exist; callers that
/// want to *load* a built-in should consult [`builtin_profile_json`] first.
pub fn profile_path(name: &str) -> Result<std::path::PathBuf> {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    if stem.is_empty() || stem.starts_with('.') || stem.contains('/') {
        bail!("invalid name '{name}': must be a simple filename, no '/' or leading '.'");
    }
    Ok(profile_dir()?.join(format!("{stem}.json")))
}

/// User-saved profiles live in `$XDG_CONFIG_HOME/sound-blasterx-g6-control/`
/// (falling back to `$HOME/.config/sound-blasterx-g6-control/`). The directory
/// is not created here — call [`ensure_profile_dir`] before writing.
pub fn profile_dir() -> Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("cannot resolve profile dir: neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(base.join("sound-blasterx-g6-control"))
}

/// Lazy-create the profile dir. Safe to call repeatedly.
pub fn ensure_profile_dir() -> Result<std::path::PathBuf> {
    let dir = profile_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The three built-in profile names. Always available without touching disk;
/// reserved (callers must refuse to save or remove with these names).
pub const BUILTINS: &[&str] = &["default", "scout", "sbx"];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

// Baked into the binary so the GUI / CLI work even when no profile files exist
// on disk and from any install location (/usr/bin, target/release/, ...).
const DEFAULT_JSON: &str = include_str!("../../../default.json");
const SCOUT_JSON:   &str = include_str!("../../../scout.json");
const SBX_JSON:     &str = include_str!("../../../sbx.json");

/// Return the bundled JSON for a built-in name, or `None` for any other name.
pub fn builtin_profile_json(name: &str) -> Option<&'static str> {
    match name {
        "default" => Some(DEFAULT_JSON),
        "scout"   => Some(SCOUT_JSON),
        "sbx"     => Some(SBX_JSON),
        _ => None,
    }
}

/// All available profile names: the three built-ins plus any user-saved
/// `*.json` in [`profile_dir`], sorted and deduplicated (in case a user-saved
/// file accidentally shares a built-in name — the built-in wins on load).
pub fn list_profile_names() -> Vec<String> {
    use std::collections::BTreeSet;
    let mut names: BTreeSet<String> = BUILTINS.iter().map(|s| (*s).to_string()).collect();
    if let Ok(dir) = profile_dir() {
        if let Ok(it) = std::fs::read_dir(&dir) {
            for entry in it.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

const VENDOR_ID:  u16 = 0x041e;
const PRODUCT_ID: u16 = 0x3256;
const INTERFACE:  i32 = 4;

const MAX_READ_ATTEMPTS: usize = 30;
const READ_TIMEOUT_MS:   i32   = 500;
const PROBE_TIMEOUT_MS:  i32   = 300;

/// Every device feature we know how to address.
/// Variant names match the JSON produced by `dump-state` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum FeatureId {
    // DSP family 0x96
    SurroundToggle,    SurroundLevel,    SurroundDistance,
    DialogPlusToggle,  DialogPlusLevel,
    SmartVolToggle,    SmartVolLevel,    SmartVolMode,
    CrystalizerToggle, CrystalizerLevel,
    BassToggle,        BassLevel,
    EqToggle,          EqPreAmp,
    Eq31Hz,  Eq62Hz,  Eq125Hz, Eq250Hz, Eq500Hz,
    Eq1kHz,  Eq2kHz,  Eq4kHz,  Eq8kHz,  Eq16kHz,

    // Other families
    SbxMaster, ScoutMode, Output, DacFilter,
}

impl FeatureId {
    /// All features in the order matching default.json / scout.json / sbx.json.
    pub const ALL: &'static [FeatureId] = &[
        Self::SurroundToggle,    Self::SurroundLevel,    Self::SurroundDistance,
        Self::DialogPlusToggle,  Self::DialogPlusLevel,
        Self::SmartVolToggle,    Self::SmartVolLevel,    Self::SmartVolMode,
        Self::CrystalizerToggle, Self::CrystalizerLevel,
        Self::BassToggle,        Self::BassLevel,
        Self::EqToggle,          Self::EqPreAmp,
        Self::Eq31Hz,  Self::Eq62Hz,  Self::Eq125Hz, Self::Eq250Hz, Self::Eq500Hz,
        Self::Eq1kHz,  Self::Eq2kHz,  Self::Eq4kHz,  Self::Eq8kHz,  Self::Eq16kHz,
        Self::SbxMaster, Self::ScoutMode, Self::Output, Self::DacFilter,
    ];

    /// Validate that `value` is in the documented range for this feature.
    pub fn validate_value(self, value: f32) -> Result<()> {
        use FeatureId::*;
        if value.is_nan() {
            bail!("{:?}: value is NaN", self);
        }
        let (ok, expected) = match self {
            SurroundToggle | DialogPlusToggle | SmartVolToggle | CrystalizerToggle
            | BassToggle | EqToggle | SbxMaster | ScoutMode | Output
                => (value == 0.0 || value == 1.0, "0 or 1"),
            SurroundLevel | DialogPlusLevel | SmartVolLevel | CrystalizerLevel | BassLevel
                => ((0.0..=1.0).contains(&value), "[0.0, 1.0]"),
            EqPreAmp
                => ((-6.0..=6.0).contains(&value), "[-6.0, 6.0] dB"),
            Eq31Hz | Eq62Hz | Eq125Hz | Eq250Hz | Eq500Hz
            | Eq1kHz | Eq2kHz | Eq4kHz | Eq8kHz | Eq16kHz
                => ((-12.0..=12.0).contains(&value), "[-12.0, 12.0] dB"),
            SurroundDistance
                => ((10.0..=300.0).contains(&value), "[10, 300] cm"),
            SmartVolMode
                => (matches!(value, 0.0 | 1.0 | 2.0), "0 (Normal), 1 (Loud), or 2 (Night)"),
            DacFilter
                => (matches!(value, 0.0 | 1.0 | 2.0 | 3.0 | 4.0), "0..=4 (filter index)"),
        };
        if !ok {
            bail!("{:?}: value {} out of range, expected {}", self, value, expected);
        }
        Ok(())
    }

    /// `(family, feature_id)` for features in the 0x96 DSP family.
    fn dsp_address(self) -> Option<(u8, u8)> {
        use FeatureId::*;
        let pair = match self {
            SurroundToggle    => (0x96, 0x00),
            SurroundLevel     => (0x96, 0x01),
            DialogPlusToggle  => (0x96, 0x02),
            DialogPlusLevel   => (0x96, 0x03),
            SmartVolToggle    => (0x96, 0x04),
            SmartVolLevel     => (0x96, 0x05),
            SmartVolMode      => (0x96, 0x06),
            CrystalizerToggle => (0x96, 0x07),
            CrystalizerLevel  => (0x96, 0x08),
            EqToggle          => (0x96, 0x09),
            EqPreAmp          => (0x96, 0x0a),
            Eq31Hz            => (0x96, 0x0b),
            Eq62Hz            => (0x96, 0x0c),
            Eq125Hz           => (0x96, 0x0d),
            Eq250Hz           => (0x96, 0x0e),
            Eq500Hz           => (0x96, 0x0f),
            Eq1kHz            => (0x96, 0x10),
            Eq2kHz            => (0x96, 0x11),
            Eq4kHz            => (0x96, 0x12),
            Eq8kHz            => (0x96, 0x13),
            Eq16kHz           => (0x96, 0x14),
            SurroundDistance  => (0x96, 0x17),
            BassToggle        => (0x96, 0x18),
            BassLevel         => (0x96, 0x19),
            _ => return None,
        };
        Some(pair)
    }
}

pub struct Device {
    handle: HidDevice,
}

impl Device {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("initialising hidapi")?;
        let info = api
            .device_list()
            .find(|d| {
                d.vendor_id() == VENDOR_ID
                    && d.product_id() == PRODUCT_ID
                    && d.interface_number() == INTERFACE
            })
            .ok_or_else(|| anyhow!("Sound BlasterX G6 interface {INTERFACE} not found on USB"))?
            .clone();
        let handle = info.open_device(&api).context(
            "opening G6 HID interface -- permission denied?\n\
             Run with sudo, or install a hidraw udev rule:\n  \
             SUBSYSTEM==\"hidraw\", ATTRS{idVendor}==\"041e\", ATTRS{idProduct}==\"3256\", MODE=\"0666\"",
        )?;
        Ok(Self { handle })
    }

    /// Send one write packet for a feature. Does not read back ACK.
    pub fn write_feature(&self, id: FeatureId, value: f32) -> Result<()> {
        let mut p = [0u8; 65];
        p[1] = 0x5a;

        if let Some((family, feature_id)) = id.dsp_address() {
            // 0x12 = WriteSingleFeature
            p[2] = 0x12;
            p[3] = 0x07;
            p[4] = 0x01;
            p[5] = family;
            p[6] = feature_id;
            p[7..11].copy_from_slice(&value.to_le_bytes());
        } else {
            match id {
                FeatureId::SbxMaster | FeatureId::ScoutMode => {
                    p[2] = 0x26;
                    p[3] = 0x05;
                    p[4] = 0x07;
                    p[5] = if id == FeatureId::SbxMaster { 0x01 } else { 0x02 };
                    p[7] = if value > 0.0 { 0x01 } else { 0x00 };
                }
                FeatureId::Output => {
                    p[2] = 0x2c;
                    p[3] = 0x05;
                    p[5] = if value > 0.0 { 0x04 } else { 0x02 };
                }
                FeatureId::DacFilter => {
                    p[2] = 0x6c;
                    p[3] = 0x03;
                    p[5] = (value as u8).saturating_add(1).clamp(0x01, 0x05);
                }
                _ => unreachable!("dsp_address covers every other variant"),
            }
        }

        self.handle.write(&p).context("writing HID report")?;
        Ok(())
    }

    /// Query the device for the current value of `id`.
    pub fn read_feature(&self, id: FeatureId) -> Result<f32> {
        let mut req = [0u8; 65];
        req[1] = 0x5a;

        if let Some((family, feature_id)) = id.dsp_address() {
            req[2] = 0x11;
            req[3] = 0x03;
            req[4] = 0x01;
            req[5] = family;
            req[6] = feature_id;
            self.handle.write(&req).context("writing read request")?;
            return self.recv_dsp(family, feature_id);
        }

        match id {
            FeatureId::SbxMaster | FeatureId::ScoutMode => {
                req[2] = 0x26;
                req[3] = 0x03;
                req[4] = 0x08;
                req[5] = 0xff;
                req[6] = 0xff;
                self.handle.write(&req).context("writing read request")?;
                let buf = self.recv(0x26)?;
                let bit = if id == FeatureId::SbxMaster { 0x01 } else { 0x02 };
                Ok(if (buf[6] & bit) != 0 { 1.0 } else { 0.0 })
            }
            FeatureId::Output => {
                req[2] = 0x2c;
                req[3] = 0x01;
                req[4] = 0x01;
                self.handle.write(&req).context("writing read request")?;
                let buf = self.recv(0x2c)?;
                match buf[4] {
                    0x02 => Ok(0.0),
                    0x04 => Ok(1.0),
                    other => bail!("unknown output mode 0x{:02x}", other),
                }
            }
            FeatureId::DacFilter => {
                req[2] = 0x6c;
                req[3] = 0x01;
                req[4] = 0x01;
                self.handle.write(&req).context("writing read request")?;
                let buf = self.recv(0x6c)?;
                Ok(buf[4].saturating_sub(1) as f32)
            }
            _ => unreachable!("dsp_address covers every other variant"),
        }
    }

    /// Send one cheap read; return `true` if the interrupt endpoint answered.
    /// If `false`, the device needs a USB reset to wake the interrupt-IN pipe.
    pub fn probe(&self) -> bool {
        let mut req = [0u8; 65];
        req[1] = 0x5a;
        req[2] = 0x2c;
        req[3] = 0x01;
        req[4] = 0x01;
        if self.handle.write(&req).is_err() {
            return false;
        }
        let mut buf = [0u8; 64];
        matches!(self.handle.read_timeout(&mut buf, PROBE_TIMEOUT_MS), Ok(n) if n > 0)
    }

    /// Reset the USB device via libusb. Sleeps 3s for the kernel to re-enumerate.
    pub fn reset_usb() -> Result<()> {
        let handle = rusb::open_device_with_vid_pid(VENDOR_ID, PRODUCT_ID)
            .ok_or_else(|| anyhow!("rusb could not open G6 for reset"))?;
        handle.reset().context("USB reset")?;
        drop(handle);
        std::thread::sleep(Duration::from_secs(3));
        Ok(())
    }

    fn recv(&self, cmd: u8) -> Result<[u8; 64]> {
        for _ in 0..MAX_READ_ATTEMPTS {
            let mut buf = [0u8; 64];
            match self.handle.read_timeout(&mut buf, READ_TIMEOUT_MS) {
                Ok(0) => continue,
                Ok(_) if buf[0] == 0x5a && buf[1] == cmd => return Ok(buf),
                Ok(_) => continue,
                Err(e) => bail!("hidapi read: {e}"),
            }
        }
        bail!("no response with cmd 0x{:02x}", cmd);
    }

    fn recv_dsp(&self, family: u8, feature_id: u8) -> Result<f32> {
        for _ in 0..MAX_READ_ATTEMPTS {
            let mut buf = [0u8; 64];
            match self.handle.read_timeout(&mut buf, READ_TIMEOUT_MS) {
                Ok(0) => continue,
                Ok(_)
                    if buf[0] == 0x5a
                        && buf[1] == 0x11
                        && buf[5] == family
                        && buf[6] == feature_id =>
                {
                    return Ok(f32::from_le_bytes(buf[7..11].try_into().unwrap()));
                }
                Ok(_) => continue,
                Err(e) => bail!("hidapi read: {e}"),
            }
        }
        bail!("no DSP response for 0x{:02x}:0x{:02x}", family, feature_id);
    }
}
