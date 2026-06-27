use actix_web::{get, web, web::Data, HttpRequest, HttpResponse};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::StreamExt;
use log::{error, trace, warn};
use crate::{argon_warn};
use rbx_dom_weak::types::Ref;
use serde::{
	de::{self, Deserializer, Visitor},
	Deserialize, Serialize,
};
use std::sync::Arc;
use tokio::sync::mpsc;

// rmp_serde serializes Ref as 16 big-endian bytes (via serialize_u128 → serialize_bytes).
// The adjacently-tagged content proxy can't forward deserialize_u128 for bin8, so Ref's
// built-in deserializer fails. This custom deserializer handles the bytes form directly.
fn ref_from_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Ref, D::Error> {
	struct V;
	impl<'de> Visitor<'de> for V {
		type Value = Ref;
		fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
			write!(f, "16 big-endian bytes or u128 Roblox referent")
		}
		fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Ref, E> {
			let arr: [u8; 16] = v
				.try_into()
				.map_err(|_| E::custom(format!("expected 16 bytes, got {}", v.len())))?;
			let val = u128::from_be_bytes(arr);
			Ok(if val == 0 { Ref::none() } else { Ref::some(val) })
		}
		fn visit_borrowed_bytes<E: de::Error>(self, v: &[u8]) -> Result<Ref, E> {
			self.visit_bytes(v)
		}
		fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Ref, A::Error> {
			let mut bytes = [0u8; 16];
			for b in &mut bytes {
				*b = seq
					.next_element::<u8>()?
					.ok_or_else(|| de::Error::custom("seq too short for Ref"))?;
			}
			self.visit_bytes(&bytes)
		}
		fn visit_u64<E: de::Error>(self, v: u64) -> Result<Ref, E> {
			Ok(if v == 0 { Ref::none() } else { Ref::some(v as u128) })
		}
		fn visit_u128<E: de::Error>(self, v: u128) -> Result<Ref, E> {
			Ok(if v == 0 { Ref::none() } else { Ref::some(v) })
		}
	}
	d.deserialize_bytes(V)
}

use crate::{
	argon_info,
	core::{processor::WriteRequest, snapshot::AddedSnapshot, Core},
	project::ProjectDetails,
	server::{self, Message},
	studio,
};

// Wire protocol
//
// All frames: base64(msgpack(frame))
// Plugin sends text frames (Roblox WebSocketClient.Send is text-only).
// Encoding: Option A from WEBSOCKET_PORT.md, base64-wrap MsgPack.
// Switch to binary by removing base64 calls if Studio gains binary frame support.

#[derive(Debug, Deserialize)]
struct InFrame {
	id: u64,
	payload: InPayload,
}

/// Requests from plugin → server.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
enum InPayload {
	Subscribe { #[serde(rename = "clientId")] client_id: u32, name: String },
	Unsubscribe,
	Write(WriteRequest),
	Snapshot { #[serde(deserialize_with = "ref_from_bytes")] instance: Ref },
	Details,
	Open { #[serde(deserialize_with = "ref_from_bytes")] instance: Ref, #[serde(rename = "line")] _line: u32 },
	Exec { code: String, focus: bool },
}

#[derive(Serialize)]
struct OutFrame {
	id: u64,
	payload: OutPayload,
}

/// Responses + server-push frames to plugin.
#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
enum OutPayload {
	// Push (id = 0; no correlation)
	SyncChanges(server::SyncChanges),
	SyncbackChanges,
	SyncDetails(server::SyncDetails),
	ExecuteCode(server::ExecuteCode),
	Disconnect(server::Disconnect),
	// Responses (id echoes request)
	Ok,
	SnapshotData(Option<AddedSnapshot>),
	DetailsData(ProjectDetails),
	Err { message: String },
}

impl From<Message> for OutPayload {
	fn from(msg: Message) -> Self {
		match msg {
			Message::SyncChanges(v) => OutPayload::SyncChanges(v),
			Message::SyncbackChanges(_) => OutPayload::SyncbackChanges,
			Message::SyncDetails(v) => OutPayload::SyncDetails(v),
			Message::ExecuteCode(v) => OutPayload::ExecuteCode(v),
			Message::Disconnect(v) => OutPayload::Disconnect(v),
		}
	}
}

fn encode(id: u64, payload: OutPayload) -> anyhow::Result<String> {
	let bytes = rmp_serde::to_vec_named(&OutFrame { id, payload })?;
	Ok(BASE64.encode(bytes))
}

fn decode_frame(text: &str) -> anyhow::Result<InFrame> {
	let bytes = BASE64.decode(text)?;
	Ok(rmp_serde::from_slice(&bytes)?)
}

// Upgrade handler

#[get("/ws")]
pub async fn upgrade(
	req: HttpRequest,
	stream: web::Payload,
	core: Data<Arc<Core>>,
) -> Result<HttpResponse, actix_web::Error> {
	let (res, session, msg_stream) = actix_ws::handle(&req, stream)?;
	argon_info!("WS client TCP connected from {:?}", req.peer_addr());
	actix_web::rt::spawn(run(session, msg_stream, Arc::clone(&*core)));
	Ok(res)
}

// Session lifecycle

async fn run(
	mut session: actix_ws::Session,
	mut stream: actix_ws::MessageStream,
	core: Arc<Core>,
) {
	let Some((client_id, name, handshake_id)) = handshake(&mut stream).await else {
		return;
	};

	if let Err(e) = core.queue().subscribe(client_id, &name) {
		error!("WS subscribe failed for client {client_id}: {e}");
		return;
	}
	argon_info!("WS client {} subscribed: {}", client_id, name);

	send(&mut session, handshake_id, OutPayload::Ok).await;

	// Bridge blocking queue drain -> async push
	let (push_tx, mut push_rx) = mpsc::unbounded_channel::<String>();
	let queue_core = core.clone();
	tokio::task::spawn_blocking(move || drain_queue(client_id, queue_core, push_tx));

	loop {
		tokio::select! {
			msg = stream.next() => {
				let close = match msg {
					None => true,
					Some(Ok(actix_ws::Message::Text(ref text))) => {
						!dispatch(&mut session, &core, client_id, text).await
					}
					Some(Ok(actix_ws::Message::Ping(ref b))) => session.pong(b).await.is_err(),
					Some(Ok(actix_ws::Message::Close(_))) | Some(Err(_)) => true,
					_ => false,
				};
				if close { break; }
			}
			encoded = push_rx.recv() => {
				match encoded {
					None => break,
					Some(text) => {
						if session.text(text).await.is_err() { break; }
					}
				}
			}
		}
	}

	// Removing the subscription causes the drain task to exit on its next wake up
	core.queue().unsubscribe(client_id).ok();
	session.close(None).await.ok();
	argon_info!("WS client {} disconnected", client_id);
}

async fn handshake(stream: &mut actix_ws::MessageStream) -> Option<(u32, String, u64)> {
	while let Some(msg) = stream.next().await {
		if let Ok(actix_ws::Message::Text(ref text)) = msg {
			match decode_frame(text) {
				Ok(InFrame {
					id,
					payload: InPayload::Subscribe { client_id, name },
				}) => {
					argon_info!("WS handshake from client {} name={}", client_id, name);
					return Some((client_id, name, id));
				}
				Ok(frame) => {
					argon_warn!("WS handshake: unexpected frame (expected Subscribe), got id={}", frame.id);
				}
				Err(e) => {
					argon_warn!("WS handshake decode error: {e}");
				}
			}
		}
	}
	None
}

async fn dispatch(
	session: &mut actix_ws::Session,
	core: &Core,
	client_id: u32,
	text: &str,
) -> bool {
	let frame = match decode_frame(text) {
		Ok(f) => f,
		Err(e) => {
			argon_warn!("WS dispatch decode error: {e}");
			return true;
		}
	};

	let id = frame.id;
	trace!("WS dispatch id={id} client={client_id}");

	match frame.payload {
		InPayload::Subscribe { .. } => {
			warn!("WS duplicate Subscribe from client {client_id} — ignored");
		}
		InPayload::Unsubscribe => {
			core.queue().unsubscribe(client_id).ok();
			send(session, id, OutPayload::Ok).await;
			return false;
		}
		InPayload::Write(req) => {
			core.processor().write(req);
			send(session, id, OutPayload::Ok).await;
		}
		InPayload::Snapshot { instance } => {
			argon_warn!("WS snapshot request from client {client_id}");
			send(session, id, OutPayload::SnapshotData(core.snapshot(instance))).await;
		}
		InPayload::Details => {
			let details = ProjectDetails::from_project(&core.project(), &core.tree());
			send(session, id, OutPayload::DetailsData(details)).await;
		}
		InPayload::Open { instance, .. } => {
			let payload = match core.open(instance) {
				Ok(()) => OutPayload::Ok,
				Err(e) => OutPayload::Err { message: e.to_string() },
			};
			send(session, id, payload).await;
		}
		InPayload::Exec { code, focus } => {
			let queue = core.queue();
			let pushed = queue.push(server::ExecuteCode { code }, None);
			if focus {
				if let Some(name) = queue.get_first_non_internal_listener_name() {
					if let Err(e) = studio::focus(Some(name)) {
						error!("WS exec focus: {e}");
					}
				}
			}
			let payload = match pushed {
				Ok(()) => OutPayload::Ok,
				Err(e) => OutPayload::Err { message: e.to_string() },
			};
			send(session, id, payload).await;
		}
	}

	true
}

async fn send(session: &mut actix_ws::Session, id: u64, payload: OutPayload) {
	match encode(id, payload) {
		Ok(text) => {
			session.text(text).await.ok();
		}
		Err(e) => error!("WS encode error: {e}"),
	}
}

// Runs on a blocking thread (crossbeam recv blocks; not async-safe).
// Exits when the subscription is removed (causes get_timeout to return Err).
fn drain_queue(client_id: u32, core: Arc<Core>, tx: mpsc::UnboundedSender<String>) {
	let queue = core.queue();
	loop {
		match queue.get_timeout(client_id) {
			Ok(Some(msg)) => match encode(0, OutPayload::from(msg)) {
				Ok(text) => {
					if tx.send(text).is_err() {
						break; // session gone
					}
				}
				Err(e) => error!("WS push encode error: {e}"),
			},
			Ok(None) => {} // 60s timeout, loop
			Err(_) => break, // unsubscribed or queue dropped
		}
	}
}
