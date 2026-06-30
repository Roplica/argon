use bytes::Bytes;
use tokio::sync::watch;

pub fn start_capture(_tx: watch::Sender<Bytes>) -> anyhow::Result<()> {
    anyhow::bail!("Viewport capture is not implemented on this platform yet")
}
