use eframe::egui;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const OPEN_REQUEST: &[u8] = b"OPEN\n";

pub(crate) enum AcquireOutcome {
    Primary(InstanceController),
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

/// Owns the per-user activation socket for the one running GUI instance.
///
/// A later `g6-gui` process connects to this socket, asks the primary process
/// to reveal its window, prints a short message, and exits before creating an
/// eframe window or a second tray item.
pub(crate) struct InstanceController {
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
    open_rx: Receiver<()>,
    egui_ctx: Arc<Mutex<Option<egui::Context>>>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl InstanceController {
    pub(crate) fn acquire() -> io::Result<AcquireOutcome> {
        Self::acquire_at(instance_socket_path()?)
    }

    fn acquire_at(socket_path: PathBuf) -> io::Result<AcquireOutcome> {
        // Binding a Unix socket is atomic. The identity check around stale
        // removal prevents two simultaneous launchers from unlinking a fresh
        // socket that the other one just created.
        for _ in 0..8 {
            let observed = socket_identity(&socket_path)?;
            if notify_running_instance(&socket_path)? {
                return Ok(AcquireOutcome::Secondary);
            }

            if let Some(observed) = observed {
                match socket_identity(&socket_path)? {
                    Some(current) if current == observed => {
                        fs::remove_file(&socket_path)?;
                    }
                    Some(_) => continue,
                    None => {}
                }
            }

            match UnixListener::bind(&socket_path) {
                Ok(listener) => return Self::from_listener(socket_path, listener),
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "could not settle the g6-gui single-instance socket",
        ))
    }

    fn from_listener(socket_path: PathBuf, listener: UnixListener) -> io::Result<AcquireOutcome> {
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let socket_identity = socket_identity(&socket_path)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "new instance socket disappeared")
        })?;

        let (open_tx, open_rx) = channel();
        let egui_ctx = Arc::new(Mutex::new(None::<egui::Context>));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_ctx = Arc::clone(&egui_ctx);
        let thread_stop = Arc::clone(&stop);
        let listener_thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                        let mut request = [0_u8; OPEN_REQUEST.len()];
                        if stream.read_exact(&mut request).is_ok() && request == OPEN_REQUEST {
                            if open_tx.send(()).is_err() {
                                return;
                            }
                            if let Ok(ctx) = thread_ctx.lock()
                                && let Some(ctx) = ctx.as_ref()
                            {
                                ctx.request_repaint();
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        });

        Ok(AcquireOutcome::Primary(Self {
            socket_path,
            socket_identity,
            open_rx,
            egui_ctx,
            stop,
            listener_thread: Some(listener_thread),
        }))
    }

    pub(crate) fn attach_context(&self, ctx: egui::Context) {
        if let Ok(mut current) = self.egui_ctx.lock() {
            *current = Some(ctx);
        }
    }

    pub(crate) fn try_recv_open(&self) -> bool {
        match self.open_rx.try_recv() {
            Ok(()) => true,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }

        // Only unlink the pathname if it still refers to our listener. This
        // avoids touching a replacement created during an unusual shutdown
        // race or after external socket cleanup.
        if socket_identity(&self.socket_path).ok().flatten() == Some(self.socket_identity) {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

impl Drop for InstanceController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn notify_running_instance(socket_path: &Path) -> io::Result<bool> {
    match UnixStream::connect(socket_path) {
        Ok(mut stream) => {
            stream.write_all(OPEN_REQUEST)?;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn instance_socket_path() -> io::Result<PathBuf> {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        return Ok(PathBuf::from(runtime_dir).join("g6-gui.sock"));
    }

    // Linux desktop sessions normally define XDG_RUNTIME_DIR. Keep a safe,
    // user-specific fallback for launches from stripped-down environments.
    let uid = fs::metadata("/proc/self")?.uid();
    Ok(std::env::temp_dir().join(format!("g6-gui-{uid}.sock")))
}

fn socket_identity(socket_path: &Path) -> io::Result<Option<SocketIdentity>> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists but is not a Unix socket", socket_path.display()),
        ));
    }

    let uid = fs::metadata("/proc/self")?.uid();
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another user", socket_path.display()),
        ));
    }

    Ok(Some(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    fn test_socket_path() -> PathBuf {
        let sequence = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "g6-gui-test-{}-{sequence}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn second_instance_activates_primary() {
        let socket_path = test_socket_path();
        let AcquireOutcome::Primary(mut primary) =
            InstanceController::acquire_at(socket_path.clone()).unwrap()
        else {
            panic!("first acquisition must be primary");
        };

        assert!(matches!(
            InstanceController::acquire_at(socket_path.clone()).unwrap(),
            AcquireOutcome::Secondary
        ));

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut opened = false;
        while std::time::Instant::now() < deadline {
            if primary.try_recv_open() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(opened, "open request timed out");

        primary.shutdown();
        assert!(!socket_path.exists());
    }

    #[test]
    fn stale_socket_is_replaced() {
        let socket_path = test_socket_path();
        let stale = UnixListener::bind(&socket_path).unwrap();
        drop(stale);

        let AcquireOutcome::Primary(mut primary) =
            InstanceController::acquire_at(socket_path.clone()).unwrap()
        else {
            panic!("stale socket must be replaced");
        };
        primary.shutdown();
        assert!(!socket_path.exists());
    }
}
