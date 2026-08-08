use crate::mixer_popup::Handle as MixerPopupHandle;
use eframe::egui;
use ksni::blocking::TrayMethods;
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(300);

const TRAY_ICON_16: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-16.png");
const TRAY_ICON_22: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-22.png");
const TRAY_ICON_24: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-24.png");
const TRAY_ICON_32: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-32.png");
const TRAY_ICON_48: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-48.png");
const TRAY_ICON_64: &[u8] = include_bytes!("../../../assets/icons/png/g6-gui-tray-64.png");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayAction {
    Initialize,
    OpenMainWindow,
    Exit,
}

struct G6Tray {
    action_tx: Sender<TrayAction>,
    egui_ctx: egui::Context,
    mixer_popup: MixerPopupHandle,
    last_click: Option<Instant>,
}

impl G6Tray {
    fn queue(&self, action: TrayAction) {
        if self.action_tx.send(action).is_ok() {
            // Tray callbacks run on the D-Bus service thread. Wake winit so
            // the eframe thread consumes the action even while the window is
            // hidden and no normal redraw events are arriving.
            self.egui_ctx.request_repaint();
        }
    }
}

impl ksni::Tray for G6Tray {
    fn id(&self) -> String {
        "g6-gui".into()
    }

    fn title(&self) -> String {
        "Sound BlasterX G6 Control".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        tray_icons().clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Sound BlasterX G6 Control".into(),
            description: "G6 audio and DSP controls".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, x: i32, y: i32) {
        // LXQt's StatusNotifier host can send (0, 0) instead of the icon's
        // global coordinates. Query the X11 root pointer synchronously while
        // it is still over the clicked tray icon, giving the popover a stable
        // anchor without moving or mapping any X11 windows ourselves.
        let (x, y) = activation_position(x, y);
        let now = Instant::now();
        let is_double_click = self
            .last_click
            .is_some_and(|last| now.duration_since(last) <= DOUBLE_CLICK_WINDOW);

        if is_double_click {
            self.last_click = None;
            self.mixer_popup.hide();
            self.queue(TrayAction::OpenMainWindow);
        } else {
            self.last_click = Some(now);
            // Bypass the eframe event loop: the persistent popup surface is
            // mapped directly from the tray callback, matching the response
            // time and anchoring behavior of LXQt's own volume widget.
            self.mixer_popup.show(x, y);
        }
    }

    fn menu_about_to_show(&mut self) {
        self.mixer_popup.hide();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;

        vec![
            StandardItem {
                label: "Initialize".into(),
                icon_name: "audio-card".into(),
                activate: Box::new(|tray: &mut Self| {
                    tray.mixer_popup.hide();
                    tray.queue(TrayAction::Initialize);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Main Window".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|tray: &mut Self| {
                    tray.mixer_popup.hide();
                    tray.queue(TrayAction::OpenMainWindow);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Exit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    tray.mixer_popup.hide();
                    tray.queue(TrayAction::Exit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn activation_position(x: i32, y: i32) -> (i32, i32) {
    if x != 0 || y != 0 {
        return (x, y);
    }

    let Ok((connection, screen_number)) = x11rb::connect(None) else {
        return (x, y);
    };
    let root = connection.setup().roots[screen_number].root;
    connection
        .query_pointer(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map_or((x, y), |reply| {
            (i32::from(reply.root_x), i32::from(reply.root_y))
        })
}

/// Owns the SNI service and the receiving half of its action queue.
///
/// Dropping this value unregisters the tray item and waits for its D-Bus
/// service thread, so the icon cannot linger after the GUI exits.
pub(crate) struct TrayController {
    action_rx: Receiver<TrayAction>,
    handle: Option<ksni::blocking::Handle<G6Tray>>,
}

impl TrayController {
    pub(crate) fn start(
        egui_ctx: egui::Context,
        mixer_popup: MixerPopupHandle,
    ) -> Result<Self, ksni::Error> {
        let (action_tx, action_rx) = channel();
        let handle = G6Tray {
            action_tx,
            egui_ctx,
            mixer_popup,
            last_click: None,
        }
        // LXQt starts XDG autostart applications alongside the panel. Keep
        // the service alive when its StatusNotifierWatcher is not ready yet;
        // ksni will register this item as soon as the watcher appears.
        .assume_sni_available(true)
        .spawn()?;

        Ok(Self {
            action_rx,
            handle: Some(handle),
        })
    }

    pub(crate) fn try_recv(&self) -> Option<TrayAction> {
        match self.action_rx.try_recv() {
            Ok(action) => Some(action),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().wait();
        }
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn tray_icons() -> &'static Vec<ksni::Icon> {
    static ICONS: LazyLock<Vec<ksni::Icon>> = LazyLock::new(|| {
        [
            TRAY_ICON_16,
            TRAY_ICON_22,
            TRAY_ICON_24,
            TRAY_ICON_32,
            TRAY_ICON_48,
            TRAY_ICON_64,
        ]
        .into_iter()
        .map(icon_from_png)
        .collect()
    });
    &ICONS
}

fn icon_from_png(png: &[u8]) -> ksni::Icon {
    let icon =
        eframe::icon_data::from_png_bytes(png).expect("embedded tray icon must be valid PNG");
    let mut data = icon.rgba;
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1); // RGBA to network-order ARGB32
    }
    ksni::Icon {
        width: icon.width as i32,
        height: icon.height as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer_popup::Command as MixerCommand;

    fn test_tray() -> (G6Tray, Receiver<TrayAction>, Receiver<MixerCommand>) {
        let (action_tx, action_rx) = channel();
        let (mixer_popup, mixer_rx) = MixerPopupHandle::test_channel();
        (
            G6Tray {
                action_tx,
                egui_ctx: egui::Context::default(),
                mixer_popup,
                last_click: None,
            },
            action_rx,
            mixer_rx,
        )
    }

    #[test]
    fn single_click_opens_mixer_immediately() {
        let (mut tray, action_rx, mixer_rx) = test_tray();
        ksni::Tray::activate(&mut tray, 41, 73);
        assert_eq!(mixer_rx.try_recv(), Ok(MixerCommand::Show { x: 41, y: 73 }));
        assert_eq!(action_rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn activation_keeps_coordinates_supplied_by_the_host() {
        assert_eq!(activation_position(41, 73), (41, 73));
    }

    #[test]
    fn second_click_immediately_opens_main_window() {
        let (mut tray, action_rx, mixer_rx) = test_tray();
        ksni::Tray::activate(&mut tray, 41, 73);
        ksni::Tray::activate(&mut tray, 41, 73);
        assert_eq!(mixer_rx.try_recv(), Ok(MixerCommand::Show { x: 41, y: 73 }));
        assert_eq!(mixer_rx.try_recv(), Ok(MixerCommand::Hide));
        assert_eq!(action_rx.try_recv(), Ok(TrayAction::OpenMainWindow));
        assert_eq!(action_rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn menu_items_queue_the_three_actions() {
        let (mut tray, action_rx, _mixer_rx) = test_tray();
        let mut labels = Vec::new();

        for item in ksni::Tray::menu(&tray) {
            if let ksni::MenuItem::Standard(item) = item {
                labels.push(item.label.clone());
                (item.activate)(&mut tray);
            }
        }

        assert_eq!(labels, ["Initialize", "Open Main Window", "Exit"]);
        assert_eq!(
            action_rx.try_iter().collect::<Vec<_>>(),
            [
                TrayAction::Initialize,
                TrayAction::OpenMainWindow,
                TrayAction::Exit,
            ]
        );
    }

    #[test]
    fn tray_pixmaps_are_valid_argb_images() {
        let sizes: Vec<i32> = tray_icons().iter().map(|icon| icon.width).collect();
        assert_eq!(sizes, [16, 22, 24, 32, 48, 64]);
        for icon in tray_icons() {
            assert_eq!(icon.width, icon.height);
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }
}
