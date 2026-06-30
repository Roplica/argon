use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::StreamExt;
use std::sync::Arc;

use crate::stream::{capture::InputEvent, server::StreamState};

pub async fn ws_input(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<Arc<StreamState>>,
) -> actix_web::Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, payload)?;

    let token = state.token.clone();
    let input_tx = state.input_tx.clone();

    actix_web::rt::spawn(async move {
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

        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                actix_ws::Message::Text(text) => {
                    match serde_json::from_str::<InputEvent>(&text) {
                        Ok(event) => {
                            // Inject into Roblox window via X11 send_event (no focus needed)
                            if let Some(inj) = &state.injector {
                                inj.send(&event);
                            }
                            let _ = input_tx.send(event);
                        }
                        Err(e) => log::warn!("Bad input JSON: {e}"),
                    }
                }
                actix_ws::Message::Ping(data) => {
                    if session.pong(&data).await.is_err() {
                        break;
                    }
                }
                actix_ws::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    Ok(response)
}
