use actix_web::{web, HttpRequest, HttpResponse};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::Arc;

use crate::stream::server::StreamState;

pub async fn ws_stream(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<Arc<StreamState>>,
) -> actix_web::Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, payload)?;

    let token = state.token.clone();
    let mut rx = state.rx.clone();

    actix_web::rt::spawn(async move {
        // First message must be the auth token as JSON: {"token":"<uuid>"}
        let authed = match msg_stream.next().await {
            Some(Ok(actix_ws::Message::Text(t))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                v["token"].as_str() == Some(&token)
            }
            _ => false,
        };

        if !authed {
            let _ = session
                .close(Some(actix_ws::CloseReason {
                    code: actix_ws::CloseCode::Other(4001),
                    description: Some("Unauthorized".into()),
                }))
                .await;
            return;
        }

        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() { break; }
                    let frame: Bytes = rx.borrow().clone();
                    if frame.is_empty() { continue; }
                    if session.binary(frame).await.is_err() { break; }
                }
                msg = msg_stream.next() => {
                    match msg {
                        Some(Ok(actix_ws::Message::Ping(data))) => {
                            if session.pong(&data).await.is_err() { break; }
                        }
                        Some(Ok(actix_ws::Message::Close(_))) | None => break,
                        _ => {}
                    }
                }
            }
        }
    });

    Ok(response)
}
