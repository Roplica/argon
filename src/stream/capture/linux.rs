use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::watch;

pub fn start_capture(tx: watch::Sender<Bytes>) -> Result<()> {
    let node_id = get_portal_node()
        .context("Portal screencast failed. A window-picker will appear — select Roblox Studio.")?;
    log::info!("PipeWire screencast: node {node_id}");
    run_pipewire(node_id, tx)
}

// ── xdg-desktop-portal session ─────────────────────────────────────────────

fn get_portal_node() -> Result<u32> {
    use ashpd::desktop::screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::CreateSessionOptions;
    use ashpd::enumflags2::BitFlags;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session(CreateSessionOptions::default()).await?;

        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_sources(BitFlags::from(SourceType::Window))
                    .set_cursor_mode(CursorMode::Embedded),
            )
            .await?;

        let request = proxy
            .start(
                &session,
                None::<&ashpd::WindowIdentifier>,
                StartCastOptions::default(),
            )
            .await?;
        let streams = request.response()?;

        streams
            .streams()
            .first()
            .map(|s| s.pipe_wire_node_id())
            .context("portal returned no streams")
    })
}

// ── PipeWire capture ────────────────────────────────────────────────────────

struct CaptureState {
    tx: watch::Sender<Bytes>,
    width: u32,
    height: u32,
}

/// Convert BGRx pixels to RGBA at 2× downscale, then send as a raw frame.
///
/// Header (10 bytes): [orig_w:u16 BE][orig_h:u16 BE][fmt=1:u8][0:u8][ts_ms:u32 BE]
/// Payload: raw RGBA at (orig_w/2) × (orig_h/2), 4 bytes/pixel, no compression.
///
/// Sending uncompressed over loopback (~62 MB/s at 30 fps) is faster end-to-end
/// than software JPEG because it eliminates both encode (~8 ms) and JS decode
/// (~5 ms) while keeping the createImageBitmap async latency path entirely.
fn send_frame(raw: &[u8], w: usize, h: usize, stride: usize, tx: &watch::Sender<Bytes>) {
    let out_w = w / 2;
    let out_h = h / 2;
    if out_w == 0 || out_h == 0 {
        return;
    }

    let pixel_bytes = out_w * out_h * 4;
    let mut frame = Vec::with_capacity(10 + pixel_bytes);

    // Header — original window dims so JS can correctly scale mouse input
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    frame.extend_from_slice(&(w as u16).to_be_bytes());
    frame.extend_from_slice(&(h as u16).to_be_bytes());
    frame.push(1u8); // fmt = 1 (raw RGBA, not JPEG)
    frame.push(0u8);
    frame.extend_from_slice(&(ts_ms as u32).to_be_bytes());

    // BGRx → RGBA at 2× downsample (one pass, no intermediate alloc)
    for row in (0..h).step_by(2) {
        let row_off = row * stride;
        for col in (0..w).step_by(2) {
            let i = row_off + col * 4;
            if i + 2 >= raw.len() {
                break;
            }
            frame.push(raw[i + 2]); // R
            frame.push(raw[i + 1]); // G
            frame.push(raw[i]);     // B
            frame.push(255u8);      // A
        }
    }

    let _ = tx.send(Bytes::from(frame));
}

fn run_pipewire(node_id: u32, tx: watch::Sender<Bytes>) -> Result<()> {
    use pipewire::{
        context::ContextRc,
        main_loop::MainLoopRc,
        spa::{
            pod::{
                deserialize::PodDeserializer, serialize::PodSerializer, Object, Property, Value,
            },
            sys::{
                SPA_FORMAT_VIDEO_format, SPA_FORMAT_VIDEO_size, SPA_FORMAT_mediaSubtype,
                SPA_FORMAT_mediaType, SPA_MEDIA_SUBTYPE_raw, SPA_MEDIA_TYPE_video,
                SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_dataType, SPA_PARAM_EnumFormat,
                SPA_PARAM_Format, SPA_TYPE_OBJECT_Format, SPA_TYPE_OBJECT_ParamBuffers,
                SPA_VIDEO_FORMAT_BGRx, SPA_DATA_MemFd, SPA_DATA_MemPtr,
            },
            utils::{Direction, Id},
        },
        stream::{StreamFlags, StreamRc},
    };

    pipewire::init();

    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    // Format param: BGRx video
    let (cur, _) = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: SPA_TYPE_OBJECT_Format,
            id: SPA_PARAM_EnumFormat,
            properties: vec![
                Property::new(SPA_FORMAT_mediaType, Value::Id(Id(SPA_MEDIA_TYPE_video))),
                Property::new(SPA_FORMAT_mediaSubtype, Value::Id(Id(SPA_MEDIA_SUBTYPE_raw))),
                Property::new(SPA_FORMAT_VIDEO_format, Value::Id(Id(SPA_VIDEO_FORMAT_BGRx))),
            ],
        }),
    )?;
    let fmt_bytes = cur.into_inner();
    let fmt_pod = pipewire::spa::pod::Pod::from_bytes(&fmt_bytes)
        .ok_or_else(|| anyhow::anyhow!("failed to build SPA format pod"))?;

    // Buffer param: prefer CPU-mapped memory (bit mask: 1<<MemPtr | 1<<MemFd)
    let mem_mask = (1i32 << SPA_DATA_MemPtr) | (1i32 << SPA_DATA_MemFd);
    let (cur2, _) = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: SPA_TYPE_OBJECT_ParamBuffers,
            id: SPA_PARAM_Buffers,
            properties: vec![Property::new(
                SPA_PARAM_BUFFERS_dataType,
                Value::Int(mem_mask),
            )],
        }),
    )?;
    let buf_bytes = cur2.into_inner();
    let buf_pod = pipewire::spa::pod::Pod::from_bytes(&buf_bytes)
        .ok_or_else(|| anyhow::anyhow!("failed to build SPA buffers pod"))?;

    let stream = StreamRc::new(
        core,
        "argon-viewport",
        pipewire::properties::properties! {
            "media.type"     => "Video",
            "media.category" => "Capture",
            "media.role"     => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(CaptureState { tx, width: 0, height: 0 })
        .param_changed(|_stream, state, id, param| {
            if id != SPA_PARAM_Format {
                return;
            }
            let Some(pod) = param else { return };
            if let Ok((_, Value::Object(obj))) =
                PodDeserializer::deserialize_from::<Value>(pod.as_bytes())
            {
                for prop in obj.properties {
                    if prop.key == SPA_FORMAT_VIDEO_size {
                        if let Value::Rectangle(r) = prop.value {
                            state.width = r.width;
                            state.height = r.height;
                            log::info!("PipeWire video: {}x{}", r.width, r.height);
                        }
                    }
                }
            }
        })
        .process(|stream, state| {
            let Some(mut buf) = stream.dequeue_buffer() else { return };
            let datas = buf.datas_mut();
            let Some(data) = datas.first_mut() else { return };

            let stride = data.chunk().stride().unsigned_abs() as usize;
            let w = if state.width > 0 {
                state.width as usize
            } else if stride > 0 {
                stride / 4
            } else {
                return;
            };
            let h = if state.height > 0 {
                state.height as usize
            } else {
                return;
            };
            let needed = w * h * 4;

            // Direct slice (MemPtr or MAP_BUFFERS-mapped MemFd)
            if let Some(d) = data.data() {
                if d.len() >= needed {
                    send_frame(d, w, h, stride, &state.tx);
                }
                return;
            }

            // Fallback: mmap fd (DmaBuf or unmapped MemFd)
            let fd = data.fd();
            if fd < 0 {
                return;
            }
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    needed,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return;
            }
            let raw = unsafe { std::slice::from_raw_parts(ptr as *const u8, needed) };
            send_frame(raw, w, h, stride, &state.tx);
            unsafe { libc::munmap(ptr, needed) };
        })
        .register()?;

    stream.connect(
        Direction::Input,
        Some(node_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut [fmt_pod, buf_pod],
    )?;

    main_loop.run();
    Ok(())
}
