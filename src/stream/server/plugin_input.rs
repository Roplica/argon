use actix_web::{web, HttpRequest, HttpResponse};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

use crate::stream::server::StreamState;

/// Long-poll endpoint for the Roblox plugin.
/// Returns a JSON array of queued InputEvents, waiting up to 30s if the queue is empty.
/// No auth required — bound to 127.0.0.1 only.
pub async fn plugin_input(
    _req: HttpRequest,
    state: web::Data<Arc<StreamState>>,
) -> actix_web::Result<HttpResponse> {
    let mut rx = state.input_rx.lock().await;

    // Drain any already-queued events immediately
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
        if events.len() >= 64 {
            break;
        }
    }

    // Nothing queued — wait up to 30s for the first event
    if events.is_empty() {
        match timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(ev)) => {
                events.push(ev);
                // Drain anything else that arrived during the wait
                while let Ok(ev) = rx.try_recv() {
                    events.push(ev);
                    if events.len() >= 64 {
                        break;
                    }
                }
            }
            _ => {} // timeout or channel closed → return []
        }
    }

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(serde_json::to_string(&events).unwrap_or_else(|_| "[]".into())))
}
