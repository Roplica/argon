pub mod capture;
pub mod inject;
pub mod server;

pub struct StreamDaemon;

impl StreamDaemon {
    pub fn run(token: String, host: String, port: u16) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::watch::channel(bytes::Bytes::new());

        let injector = match inject::Injector::new() {
            Ok(inj) => {
                log::info!("X11 input injector ready");
                Some(std::sync::Arc::new(inj))
            }
            Err(e) => {
                log::warn!("X11 input injector unavailable: {e}");
                None
            }
        };

        std::thread::spawn(move || {
            if let Err(e) = capture::start(tx) {
                log::error!("Capture thread exited: {e}");
            }
        });

        server::start(token, host, port, rx, injector)
    }
}
