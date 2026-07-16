//! G6-only tray mixer popover for X11.
//!
//! This deliberately does not use an eframe child viewport.  LXQt's own
//! volume widget is an override-redirect popup attached to the panel, not a
//! managed application window.  Owning this small X11 surface directly gives
//! it the same stable lifecycle: one window is created at launch, then moved,
//! mapped and unmapped for tray activations without Openbox ever managing it.

use crate::VolumeHandle;
use anyhow::{Context as _, Result, anyhow};
use fontdue::{Font, FontSettings};
use std::borrow::Cow;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use x11rb::connection::Connection as _;
use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, KeyButMask, PropMode, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const LOGICAL_WIDTH: f32 = 200.0;
const LOGICAL_HEIGHT: f32 = 320.0;
const SPEAKER_X: f32 = 58.0;
const MIC_X: f32 = 142.0;
const POLL_INTERVAL: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SliderKind {
    Speaker,
    Mic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Show { x: i32, y: i32 },
    Hide,
    Exit,
}

#[derive(Clone)]
pub(crate) struct Handle {
    command_tx: Sender<Command>,
}

impl Handle {
    pub(crate) fn unavailable() -> Self {
        let (command_tx, command_rx) = channel();
        drop(command_rx);
        Self { command_tx }
    }

    pub(crate) fn show(&self, x: i32, y: i32) {
        let _ = self.command_tx.send(Command::Show { x, y });
    }

    pub(crate) fn hide(&self) {
        let _ = self.command_tx.send(Command::Hide);
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, Receiver<Command>) {
        let (command_tx, command_rx) = channel();
        (Self { command_tx }, command_rx)
    }
}

pub(crate) struct Controller {
    handle: Handle,
    join: Option<JoinHandle<()>>,
}

impl Controller {
    pub(crate) fn start(volume: VolumeHandle, scale: f32) -> Result<Self> {
        let (command_tx, command_rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let join = thread::Builder::new()
            .name("g6-mixer-popup".into())
            .spawn(move || match Popup::new(volume, scale) {
                Ok(mut popup) => {
                    let _ = ready_tx.send(Ok(()));
                    popup.run(command_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            })
            .context("failed to start G6 mixer popup thread")?;

        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                handle: Handle { command_tx },
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(anyhow!(error))
            }
            Err(error) => Err(anyhow!("G6 mixer popup did not start: {error}")),
        }
    }

    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub(crate) fn hide(&self) {
        self.handle.hide();
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.handle.command_tx.send(Command::Exit);
            let _ = join.join();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Atoms {
    net_wm_name: Atom,
    utf8_string: Atom,
    net_wm_window_type: Atom,
    popup_menu: Atom,
    kde_override: Atom,
    net_workarea: Atom,
    net_current_desktop: Atom,
}

impl Atoms {
    fn new(connection: &RustConnection) -> Result<Self> {
        Ok(Self {
            net_wm_name: intern(connection, b"_NET_WM_NAME")?,
            utf8_string: intern(connection, b"UTF8_STRING")?,
            net_wm_window_type: intern(connection, b"_NET_WM_WINDOW_TYPE")?,
            popup_menu: intern(connection, b"_NET_WM_WINDOW_TYPE_POPUP_MENU")?,
            kde_override: intern(connection, b"_KDE_NET_WM_WINDOW_TYPE_OVERRIDE")?,
            net_workarea: intern(connection, b"_NET_WORKAREA")?,
            net_current_desktop: intern(connection, b"_NET_CURRENT_DESKTOP")?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkArea {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct Popup {
    connection: RustConnection,
    root: u32,
    window: u32,
    gc: u32,
    depth: u8,
    atoms: Atoms,
    font: Font,
    volume: VolumeHandle,
    scale: f32,
    width: u16,
    height: u16,
    origin: (i32, i32),
    visible: bool,
    buttons: u16,
    dragging: Option<SliderKind>,
    hovered: Option<SliderKind>,
    last_sink_bits: u32,
    last_source_bits: u32,
}

impl Popup {
    fn new(volume: VolumeHandle, scale: f32) -> Result<Self> {
        let (connection, screen_number) = x11rb::connect(None).context("X11 is unavailable")?;
        let screen = &connection.setup().roots[screen_number];
        let root = screen.root;
        let depth = screen.root_depth;
        if depth != 24 {
            return Err(anyhow!(
                "unsupported X11 root depth {depth}; expected 24-bit TrueColor"
            ));
        }

        let scale = scale.clamp(1.0, 3.0);
        let width = (LOGICAL_WIDTH * scale).round() as u16;
        let height = (LOGICAL_HEIGHT * scale).round() as u16;
        let window = connection.generate_id()?;
        connection
            .create_window(
                depth,
                window,
                root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new()
                    .background_pixel(screen.white_pixel)
                    .border_pixel(screen.black_pixel)
                    .override_redirect(1)
                    .save_under(1)
                    .event_mask(EventMask::EXPOSURE),
            )?
            .check()?;

        let gc = connection.generate_id()?;
        connection
            .create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?
            .check()?;

        let atoms = Atoms::new(&connection)?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"g6-gui-mixer\0g6-gui-mixer\0",
            )?
            .check()?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                atoms.net_wm_name,
                atoms.utf8_string,
                b"G6 Mixer",
            )?
            .check()?;
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                atoms.net_wm_window_type,
                AtomEnum::ATOM,
                &[atoms.kde_override, atoms.popup_menu],
            )?
            .check()?;
        connection.flush()?;

        let font = Font::from_bytes(epaint_default_fonts::UBUNTU_LIGHT, FontSettings::default())
            .map_err(|error| anyhow!("failed to load popup font: {error}"))?;
        let last_sink_bits = volume.sink().to_bits();
        let last_source_bits = volume.source().to_bits();

        Ok(Self {
            connection,
            root,
            window,
            gc,
            depth,
            atoms,
            font,
            volume,
            scale,
            width,
            height,
            origin: (0, 0),
            visible: false,
            buttons: 0,
            dragging: None,
            hovered: None,
            last_sink_bits,
            last_source_bits,
        })
    }

    fn run(&mut self, command_rx: Receiver<Command>) {
        let mut exit = false;
        while !exit {
            let timeout = if self.visible {
                POLL_INTERVAL
            } else {
                Duration::from_millis(250)
            };

            match command_rx.recv_timeout(timeout) {
                Ok(command) => exit = self.handle_command(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => exit = true,
            }
            while !exit {
                match command_rx.try_recv() {
                    Ok(command) => exit = self.handle_command(command),
                    Err(_) => break,
                }
            }

            while let Ok(Some(event)) = self.connection.poll_for_event() {
                if matches!(event, Event::Expose(_)) && self.visible {
                    let _ = self.draw();
                }
            }

            if self.visible {
                let _ = self.poll_pointer();
            }
        }

        let _ = self.hide();
        let _ = self.connection.free_gc(self.gc);
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Show { x, y } => {
                let _ = self.show(x, y);
                false
            }
            Command::Hide => {
                let _ = self.hide();
                false
            }
            Command::Exit => true,
        }
    }

    fn show(&mut self, _anchor_x: i32, _anchor_y: i32) -> Result<()> {
        let area = self.work_area();
        self.origin = popup_origin(area, i32::from(self.width), i32::from(self.height));
        self.connection
            .configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .x(self.origin.0)
                    .y(self.origin.1)
                    .width(u32::from(self.width))
                    .height(u32::from(self.height))
                    .stack_mode(StackMode::ABOVE),
            )?
            .check()?;

        self.dragging = None;
        self.hovered = None;
        self.buttons = self
            .pointer()
            .map_or(0, |pointer| button_bits(pointer.mask));
        self.visible = true;
        self.draw()?;
        self.connection.map_window(self.window)?.check()?;
        self.connection
            .configure_window(
                self.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    fn hide(&mut self) -> Result<()> {
        if self.visible {
            self.connection.unmap_window(self.window)?.check()?;
            self.connection.flush()?;
        }
        self.visible = false;
        self.dragging = None;
        self.hovered = None;
        Ok(())
    }

    fn pointer(&self) -> Option<x11rb::protocol::xproto::QueryPointerReply> {
        self.connection.query_pointer(self.root).ok()?.reply().ok()
    }

    fn poll_pointer(&mut self) -> Result<()> {
        let Some(pointer) = self.pointer() else {
            return Ok(());
        };
        let buttons = button_bits(pointer.mask);
        let pressed = buttons & !self.buttons;
        let button1_down = buttons & u16::from(KeyButMask::BUTTON1) != 0;
        let root_x = i32::from(pointer.root_x);
        let root_y = i32::from(pointer.root_y);
        let local_x = root_x - self.origin.0;
        let local_y = root_y - self.origin.1;
        let inside = local_x >= 0
            && local_y >= 0
            && local_x < i32::from(self.width)
            && local_y < i32::from(self.height);

        if pressed != 0 && !inside {
            self.buttons = buttons;
            return self.hide();
        }

        let hovered = if inside {
            self.slider_at(local_x, local_y)
        } else {
            None
        };
        let mut dirty = hovered != self.hovered;
        self.hovered = hovered;

        if pressed & u16::from(KeyButMask::BUTTON1) != 0 {
            self.dragging = hovered;
            if let Some(kind) = self.dragging {
                self.set_volume_from_y(kind, local_y);
                dirty = true;
            }
        } else if button1_down {
            if let Some(kind) = self.dragging {
                self.set_volume_from_y(kind, local_y);
                self.hovered = Some(kind);
                dirty = true;
            }
        } else if let Some(kind) = self.dragging.take() {
            // Apply the release coordinate as well, so a fast final pointer
            // movement between polling ticks is not lost.
            self.set_volume_from_y(kind, local_y);
            dirty = true;
        }

        let sink_bits = self.volume.sink().to_bits();
        let source_bits = self.volume.source().to_bits();
        if sink_bits != self.last_sink_bits || source_bits != self.last_source_bits {
            self.last_sink_bits = sink_bits;
            self.last_source_bits = source_bits;
            dirty = true;
        }
        self.buttons = buttons;

        if dirty {
            self.draw()?;
        }
        Ok(())
    }

    fn slider_at(&self, x: i32, y: i32) -> Option<SliderKind> {
        let (track_top, track_bottom) = self.track_bounds();
        if y < track_top - self.px(12.0) || y > track_bottom + self.px(12.0) {
            return None;
        }
        let radius = self.px(26.0);
        let speaker_x = self.slider_x(SliderKind::Speaker);
        let mic_x = self.slider_x(SliderKind::Mic);
        if (x - speaker_x).abs() <= radius {
            Some(SliderKind::Speaker)
        } else if (x - mic_x).abs() <= radius {
            Some(SliderKind::Mic)
        } else {
            None
        }
    }

    fn set_volume_from_y(&self, kind: SliderKind, y: i32) {
        let (top, bottom) = self.track_bounds();
        let value = volume_from_y(y, top, bottom);
        match kind {
            SliderKind::Speaker => self.volume.set_sink(value),
            SliderKind::Mic => self.volume.set_source(value),
        }
    }

    fn draw(&mut self) -> Result<()> {
        let canvas = render_popup(
            &self.font,
            self.scale,
            self.width,
            self.height,
            self.volume.sink(),
            self.volume.source(),
            self.hovered,
        );

        let image = Image::new(
            self.width,
            self.height,
            ScanlinePad::Pad32,
            self.depth,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&canvas.pixels),
        )?;
        image.put(&self.connection, self.window, self.gc, 0, 0)?;
        self.connection.flush()?;
        Ok(())
    }

    fn work_area(&self) -> WorkArea {
        let fallback = WorkArea {
            x: 0,
            y: 0,
            width: i32::from(self.connection.setup().roots[0].width_in_pixels),
            height: i32::from(self.connection.setup().roots[0].height_in_pixels),
        };
        let desktop = self
            .connection
            .get_property(
                false,
                self.root,
                self.atoms.net_current_desktop,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32()?.next())
            .unwrap_or(0) as usize;
        let values: Vec<u32> = self
            .connection
            .get_property(
                false,
                self.root,
                self.atoms.net_workarea,
                AtomEnum::CARDINAL,
                0,
                u32::MAX,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().map(Iterator::collect))
            .unwrap_or_default();
        let offset = desktop.saturating_mul(4);
        if values.len() < offset + 4 {
            return fallback;
        }
        WorkArea {
            x: values[offset] as i32,
            y: values[offset + 1] as i32,
            width: values[offset + 2] as i32,
            height: values[offset + 3] as i32,
        }
    }

    fn slider_x(&self, kind: SliderKind) -> i32 {
        match kind {
            SliderKind::Speaker => self.px(SPEAKER_X),
            SliderKind::Mic => self.px(MIC_X),
        }
    }

    fn track_bounds(&self) -> (i32, i32) {
        (self.px(88.0), self.px(276.0))
    }

    fn px(&self, logical: f32) -> i32 {
        (logical * self.scale).round() as i32
    }
}

fn render_popup(
    font: &Font,
    scale: f32,
    width: u16,
    height: u16,
    speaker: f32,
    mic: f32,
    hovered: Option<SliderKind>,
) -> Canvas {
    let px = |logical: f32| (logical * scale).round() as i32;
    let slider_x = |kind| match kind {
        SliderKind::Speaker => px(SPEAKER_X),
        SliderKind::Mic => px(MIC_X),
    };
    let track_bounds = (px(88.0), px(276.0));
    let mut canvas = Canvas::new(width, height, Color::rgb(244, 244, 244));
    let border = Color::rgb(150, 150, 150);
    let text = Color::rgb(32, 32, 32);
    let blue = Color::rgb(72, 163, 222);
    let rail = Color::rgb(208, 208, 208);

    canvas.stroke_rect(0, 0, i32::from(width), i32::from(height), px(1.0), border);

    // Small centered tab, matching the silhouette of LXQt's Mixer popup.
    let tab_width = px(58.0);
    let tab_height = px(22.0);
    let tab_x = (i32::from(width) - tab_width) / 2;
    canvas.hline(0, tab_x, tab_height, border);
    canvas.hline(tab_x + tab_width, i32::from(width), tab_height, border);
    canvas.fill_rect(
        tab_x,
        0,
        tab_width,
        tab_height + 1,
        Color::rgb(244, 244, 244),
    );
    canvas.stroke_rect(tab_x, 0, tab_width, tab_height + 1, px(1.0), border);
    canvas.text_centered(
        font,
        "G6",
        15.0 * scale,
        tab_x + tab_width / 2,
        tab_height / 2,
        text,
    );

    for (kind, label, value) in [
        (SliderKind::Speaker, "Speaker", speaker),
        (SliderKind::Mic, "Mic", mic),
    ] {
        let x = slider_x(kind);
        canvas.text_centered(font, label, 18.0 * scale, x, px(43.0), text);
        canvas.text_centered(font, "150%", 16.0 * scale, x, px(68.0), text);
        canvas.text_centered(font, "0%", 16.0 * scale, x, px(299.0), text);

        let value = value.clamp(0.0, 1.5);
        let (top, bottom) = track_bounds;
        let handle_y = value_y(value, top, bottom);
        let rail_width = px(10.0).max(2);
        canvas.fill_rect(x - rail_width / 2, top, rail_width, bottom - top, rail);
        canvas.fill_rect(
            x - rail_width / 2,
            handle_y,
            rail_width,
            bottom - handle_y,
            blue,
        );

        let handle_width = px(28.0);
        let handle_height = px(17.0);
        canvas.fill_rect(
            x - handle_width / 2,
            handle_y - handle_height / 2,
            handle_width,
            handle_height,
            Color::rgb(238, 238, 238),
        );
        canvas.stroke_rect(
            x - handle_width / 2,
            handle_y - handle_height / 2,
            handle_width,
            handle_height,
            px(1.0),
            Color::rgb(55, 55, 55),
        );

        if hovered == Some(kind) {
            let bubble_width = px(64.0);
            let bubble_height = px(32.0);
            let bubble_x = match kind {
                SliderKind::Speaker => x + px(20.0),
                SliderKind::Mic => x - px(20.0) - bubble_width,
            }
            .clamp(px(4.0), i32::from(width) - bubble_width - px(4.0));
            let bubble_y = (handle_y - bubble_height / 2)
                .clamp(px(76.0), i32::from(height) - bubble_height - px(4.0));
            canvas.fill_rect(
                bubble_x,
                bubble_y,
                bubble_width,
                bubble_height,
                Color::rgb(252, 252, 252),
            );
            canvas.stroke_rect(
                bubble_x,
                bubble_y,
                bubble_width,
                bubble_height,
                px(1.0),
                border,
            );
            canvas.text_centered(
                font,
                &format!("{:.0}%", value * 100.0),
                18.0 * scale,
                bubble_x + bubble_width / 2,
                bubble_y + bubble_height / 2,
                text,
            );
        }
    }

    canvas
}

fn intern(connection: &RustConnection, name: &[u8]) -> Result<Atom> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

fn button_bits(mask: KeyButMask) -> u16 {
    u16::from(mask)
        & (u16::from(KeyButMask::BUTTON1)
            | u16::from(KeyButMask::BUTTON2)
            | u16::from(KeyButMask::BUTTON3))
}

fn popup_origin(area: WorkArea, width: i32, height: i32) -> (i32, i32) {
    let margin = 0;
    // This is a system-style volume panel, not a context menu. Keep it fixed
    // to the bottom-right corner of the usable desktop regardless of which
    // coordinates the StatusNotifier host supplies for the activation event.
    let x = (area.x + area.width - width - margin).max(area.x + margin);
    let y = (area.y + area.height - height - margin).max(area.y + margin);
    (x, y)
}

fn value_y(value: f32, top: i32, bottom: i32) -> i32 {
    let fraction = (value / 1.5).clamp(0.0, 1.0);
    bottom - ((bottom - top) as f32 * fraction).round() as i32
}

fn volume_from_y(y: i32, top: i32, bottom: i32) -> f32 {
    let fraction = ((bottom - y) as f32 / (bottom - top) as f32).clamp(0.0, 1.0);
    (fraction * 1.5 * 100.0).round() / 100.0
}

#[derive(Clone, Copy)]
struct Color {
    red: u8,
    green: u8,
    blue: u8,
}

impl Color {
    const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

struct Canvas {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: u16, height: u16, background: Color) -> Self {
        let mut canvas = Self {
            width: i32::from(width),
            height: i32::from(height),
            pixels: vec![0; usize::from(width) * usize::from(height) * 4],
        };
        canvas.fill_rect(0, 0, canvas.width, canvas.height, background);
        canvas
    }

    fn fill_rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: Color) {
        let x0 = x.clamp(0, self.width);
        let y0 = y.clamp(0, self.height);
        let x1 = (x + width).clamp(0, self.width);
        let y1 = (y + height).clamp(0, self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                self.set_pixel(px, py, color);
            }
        }
    }

    fn stroke_rect(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        thickness: i32,
        color: Color,
    ) {
        let thickness = thickness.max(1);
        self.fill_rect(x, y, width, thickness, color);
        self.fill_rect(x, y + height - thickness, width, thickness, color);
        self.fill_rect(x, y, thickness, height, color);
        self.fill_rect(x + width - thickness, y, thickness, height, color);
    }

    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        self.fill_rect(x0, y, x1 - x0, 1, color);
    }

    fn text_centered(
        &mut self,
        font: &Font,
        text: &str,
        size: f32,
        center_x: i32,
        center_y: i32,
        color: Color,
    ) {
        struct Glyph {
            bitmap: Vec<u8>,
            x: f32,
            top: i32,
            width: usize,
            height: usize,
        }

        let mut glyphs = Vec::new();
        let mut pen_x = 0.0_f32;
        let mut previous = None;
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = i32::MAX;
        let mut max_y = i32::MIN;

        for character in text.chars() {
            if let Some(previous) = previous {
                pen_x += font
                    .horizontal_kern(previous, character, size)
                    .unwrap_or(0.0);
            }
            let (metrics, bitmap) = font.rasterize(character, size);
            let glyph_x = pen_x + metrics.xmin as f32;
            let top = -metrics.ymin - metrics.height as i32;
            min_x = min_x.min(glyph_x);
            max_x = max_x.max(glyph_x + metrics.width as f32);
            min_y = min_y.min(top);
            max_y = max_y.max(top + metrics.height as i32);
            glyphs.push(Glyph {
                bitmap,
                x: glyph_x,
                top,
                width: metrics.width,
                height: metrics.height,
            });
            pen_x += metrics.advance_width;
            previous = Some(character);
        }

        if glyphs.is_empty() {
            return;
        }
        let offset_x = center_x as f32 - (min_x + max_x) / 2.0;
        let offset_y = center_y - (min_y + max_y) / 2;
        for glyph in glyphs {
            let start_x = (offset_x + glyph.x).round() as i32;
            let start_y = offset_y + glyph.top;
            for y in 0..glyph.height {
                for x in 0..glyph.width {
                    let alpha = glyph.bitmap[y * glyph.width + x];
                    if alpha != 0 {
                        self.blend_pixel(start_x + x as i32, start_y + y as i32, color, alpha);
                    }
                }
            }
        }
    }

    fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let index = ((y * self.width + x) * 4) as usize;
        self.pixels[index] = color.blue;
        self.pixels[index + 1] = color.green;
        self.pixels[index + 2] = color.red;
        self.pixels[index + 3] = 0;
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let index = ((y * self.width + x) * 4) as usize;
        let alpha = u16::from(alpha);
        let inverse = 255 - alpha;
        self.pixels[index] =
            ((u16::from(self.pixels[index]) * inverse + u16::from(color.blue) * alpha) / 255) as u8;
        self.pixels[index + 1] = ((u16::from(self.pixels[index + 1]) * inverse
            + u16::from(color.green) * alpha)
            / 255) as u8;
        self.pixels[index + 2] = ((u16::from(self.pixels[index + 2]) * inverse
            + u16::from(color.red) * alpha)
            / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_font() -> Font {
        Font::from_bytes(epaint_default_fonts::UBUNTU_LIGHT, FontSettings::default())
            .expect("embedded popup font")
    }

    #[test]
    fn popup_is_flush_with_bottom_right_of_work_area() {
        let area = WorkArea {
            x: 2,
            y: 2,
            width: 3836,
            height: 2086,
        };
        assert_eq!(popup_origin(area, 432, 576), (3406, 1512));
    }

    #[test]
    fn popup_clamps_to_work_area_edges() {
        let area = WorkArea {
            x: 0,
            y: 0,
            width: 1000,
            height: 700,
        };
        assert_eq!(popup_origin(area, 240, 320), (760, 380));
    }

    #[test]
    fn vertical_slider_maps_endpoints_to_zero_and_one_fifty() {
        assert_eq!(volume_from_y(276, 88, 276), 0.0);
        assert_eq!(volume_from_y(88, 88, 276), 1.5);
        assert_eq!(value_y(0.0, 88, 276), 276);
        assert_eq!(value_y(1.5, 88, 276), 88);
    }

    #[test]
    fn production_renderer_draws_both_active_volume_rails() {
        let scale = 1.8;
        let width = (LOGICAL_WIDTH * scale).round() as u16;
        let height = (LOGICAL_HEIGHT * scale).round() as u16;
        let canvas = render_popup(
            &preview_font(),
            scale,
            width,
            height,
            0.75,
            1.5,
            Some(SliderKind::Speaker),
        );
        assert_eq!(canvas.pixels.len(), width as usize * height as usize * 4);

        let pixel = |x: usize, y: usize| {
            let offset = (y * width as usize + x) * 4;
            &canvas.pixels[offset..offset + 3]
        };
        assert_eq!(pixel(10, 100), [244, 244, 244]);
        assert_eq!(
            pixel((SPEAKER_X * scale).round() as usize, 480),
            [222, 163, 72]
        );
        assert_eq!(pixel((MIC_X * scale).round() as usize, 300), [222, 163, 72]);
    }

    #[test]
    #[ignore = "writes an off-screen visual-review artifact; never maps an X11 window"]
    fn write_offscreen_preview() {
        let scale = 1.8;
        let canvas = render_popup(
            &preview_font(),
            scale,
            (LOGICAL_WIDTH * scale).round() as u16,
            (LOGICAL_HEIGHT * scale).round() as u16,
            0.94,
            1.5,
            Some(SliderKind::Speaker),
        );
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.width, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        std::fs::write("/tmp/g6-mixer-offscreen.ppm", ppm).expect("write preview");
    }
}
