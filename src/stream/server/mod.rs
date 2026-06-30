use actix_web::{web, App, HttpServer};
use anyhow::Result;
use bytes::Bytes;
use std::{net::TcpListener, sync::Arc};
use tokio::sync::{mpsc, watch, Mutex};

use crate::stream::{capture::InputEvent, inject::Injector};

mod input;
mod plugin_input;
mod stream;

pub struct StreamState {
    pub token: String,
    pub rx: watch::Receiver<Bytes>,
    pub input_tx: mpsc::UnboundedSender<InputEvent>,
    pub input_rx: Arc<Mutex<mpsc::UnboundedReceiver<InputEvent>>>,
    pub injector: Option<Arc<Injector>>,
}

/// Bind the listener, print the port, then block running the actix server.
pub fn start(
    token: String,
    host: String,
    port: u16,
    rx: watch::Receiver<Bytes>,
    injector: Option<Arc<Injector>>,
) -> Result<()> {
    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    let bound_port = listener.local_addr()?.port();

    println!("stream_port:{bound_port}");

    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let state = Arc::new(StreamState {
        token,
        rx,
        input_tx,
        input_rx: Arc::new(Mutex::new(input_rx)),
        injector,
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        HttpServer::new(move || {
            let data = web::Data::new(state.clone());
            App::new()
                .app_data(data)
                .route("/stream", web::get().to(stream::ws_stream))
                .route("/input", web::get().to(input::ws_input))
                .route("/plugin_input", web::get().to(plugin_input::plugin_input))
        })
        .listen(listener)?
        .run()
        .await
        .map_err(anyhow::Error::from)
    })
}
