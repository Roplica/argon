/// Probe binary: verify capture approach for Wine/Roblox on Linux.
///
/// Tests: Capture — direct GetImage on the window XID.
/// XComposite redirect is not used (races with Wine's DX thread and crashes).
/// Input injection is handled by the Roblox plugin via VirtualInput, not the daemon.
///
/// Run: cd argon && cargo run --bin stream_probe

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("stream_probe is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use x11rb::{
        connection::Connection,
        protocol::xproto::{AtomEnum, ConnectionExt as XprotoExt, ImageFormat},
        rust_connection::RustConnection,
    };

    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;

    // --- Find Roblox window ---
    let net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
    let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;

    let list =
        conn.get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, 4096)?.reply()?;
    let windows: Vec<u32> = list.value32().map(|v| v.collect()).unwrap_or_default();

    println!("Scanning {} open windows for Roblox...", windows.len());

    let mut roblox_xid: Option<u32> = None;
    for win in &windows {
        let name =
            conn.get_property(false, *win, net_wm_name, utf8_string, 0, 512)?.reply()?;
        let title = String::from_utf8_lossy(&name.value).to_string();
        if !title.is_empty() {
            println!("  0x{:x} {:?}", win, title);
        }
        if title.contains("Roblox") {
            roblox_xid = Some(*win);
        }
    }

    let xid = match roblox_xid {
        Some(x) => {
            println!("\n✓ Found Roblox window XID=0x{:x}", x);
            x
        }
        None => {
            eprintln!("\n✗ No Roblox window found — launch Roblox Studio first");
            std::process::exit(1);
        }
    };

    // --- Capture probe: direct GetImage (no XComposite redirect — that crashes Wine's DX thread) ---
    println!("\n[Capture probe] Direct GetImage on window XID=0x{:x}", xid);
    let geo = conn.get_geometry(xid)?.reply()?;
    println!("  Window geometry: {}x{}", geo.width, geo.height);
    let trans = conn.translate_coordinates(xid, root, 0, 0)?.reply()?;
    println!("  Window position: {},{}", trans.dst_x, trans.dst_y);

    let img = conn
        .get_image(ImageFormat::Z_PIXMAP, xid, 0, 0, geo.width, geo.height, !0u32)?
        .reply()?;

    let expected = geo.width as usize * geo.height as usize * 4;
    anyhow::ensure!(
        img.data.len() == expected,
        "unexpected size: got {} expected {}",
        img.data.len(),
        expected
    );

    // Sample center pixel (BGRA → RGB)
    let center = (geo.height as usize / 2 * geo.width as usize + geo.width as usize / 2) * 4;
    let (b, g, r) = (img.data[center], img.data[center + 1], img.data[center + 2]);
    println!("  Center pixel RGB=({r},{g},{b})");
    if r == 0 && g == 0 && b == 0 {
        println!("  WARNING: all-zero pixel — Wine may be using hardware accel (try LIBGL_ALWAYS_SOFTWARE=1)");
    } else {
        println!("  PASS capture (direct GetImage works)");
    }
    println!("  Image data: {} bytes", img.data.len());

    println!("\nNote: input injection is handled by the Roblox plugin via VirtualInput, not the daemon.");

    Ok(())
}
