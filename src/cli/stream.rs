use anyhow::Result;
use clap::Parser;

use crate::stream::StreamDaemon;

/// Start the viewport streaming daemon (spawned by the VS Code extension)
#[derive(Parser)]
pub struct Stream {
    /// Host to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    /// Port to listen on (printed to stdout as stream_port:<N> when ready)
    #[arg(short = 'P', long, default_value = "7286")]
    port: u16,

    /// Auth token for /stream and /input WebSocket endpoints (used by VS Code webview)
    #[arg(long)]
    stream_token: String,
}

impl Stream {
    pub fn main(self) -> Result<()> {
        StreamDaemon::run(self.stream_token, self.host, self.port)
    }
}
