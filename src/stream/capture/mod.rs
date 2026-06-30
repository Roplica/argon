use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod stub;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputEvent {
    Keydown { key: String },
    Keyup { key: String },
    Mousemove { x: f32, y: f32 },
    Mousedelta { dx: f32, dy: f32 },
    Mousedown { button: u8, x: f32, y: f32 },
    Mouseup { button: u8, x: f32, y: f32 },
    /// Webview acquired pointer lock — plugin should use SendMouseDelta instead of SendMousePosition.
    Lock,
    /// Webview released pointer lock — plugin should use SendMousePosition.
    Unlock,
}

/// Start the capture loop in the calling thread (blocks until error).
pub fn start(tx: watch::Sender<Bytes>) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return linux::start_capture(tx);

    #[cfg(not(target_os = "linux"))]
    return stub::start_capture(tx);
}
