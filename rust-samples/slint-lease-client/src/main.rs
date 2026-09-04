// Demo: acquire a DRM lease from the simple-graphics-controller daemon (@sgc)
// and render a Slint UI on the granted lease fd via the linuxkms backend with an
// injected device fd (BackendBuilder::with_drm_device, sgc-lease branch work).
//
// Renderer chosen by cargo feature: default = software (musl static builds),
// `--features femtovg` = OpenGL over gbm/EGL (gnu dynamic builds).
//
// The SgcClient is deliberately kept alive (not dropped) for the whole run so
// the daemon keeps the lease granted. Pumping for Revoke/regrant is phase 2.

use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::Context;
use libsgc_rs::{Resource, SgcClient};
slint::slint! {
    export component MainWindow inherits Window {
        background: #10131f;
        in-out property <length> anim-x: 0px;

        Text {
            x: 24px;
            y: 20px;
            color: #ffffff;
            font-size: 34px;
            font-family: "DejaVu Sans";
            text: "Slint on an sgc DRM lease";
        }
        Text {
            x: 24px;
            y: 72px;
            color: #9aa0b5;
            font-size: 20px;
            font-family: "DejaVu Sans";
            text: "display fd came from the @sgc daemon, not /dev/dri";
        }
        Rectangle {
            y: parent.height * 0.5 - 70px;
            x: anim-x;
            width: 140px;
            height: 140px;
            border-radius: 12px;
            background: #e94560;
            Rectangle {
                x: 20px;
                y: 20px;
                width: 30px;
                height: 30px;
                border-radius: 15px;
                background: #ffe8ec;
            }
        }
        Text {
            x: 24px;
            y: parent.height - 56px;
            color: #5a6485;
            font-size: 16px;
            font-family: "DejaVu Sans";
            text: "move along = live page flips on the lease";
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Phase 1: acquire once, render, hold. No pumping yet — nothing else
    // contends for the card, so no Revoke arrives.
    let (client, advertised) =
        SgcClient::connect().context("connecting to @sgc (is the daemon running?)")?;
    let mut client = client;

    let (card, resource) = advertised
        .iter()
        .find_map(|r| match r {
            Resource::Drm { card } => Some((*card, r.clone())),
            _ => None,
        })
        .context("the daemon advertised no DRM card")?;
    println!("advertised resources: {advertised:?}");
    println!("acquiring Drm{{ card: {card} }}...");

    client.acquire(resource.clone()).context("acquire denied or failed")?;
    let lease_fd = client.fd(&resource).context("granted fd not held")?;
    let fl = nix::fcntl::fcntl(&lease_fd, nix::fcntl::FcntlArg::F_GETFL)
        .map_err(|e| anyhow::anyhow!("F_GETFL: {e}"))?;
    println!(
        "lease fd {} granted (oflags 0x{fl:x}, O_NONBLOCK={})",
        lease_fd.as_raw_fd(),
        fl & nix::fcntl::OFlag::O_NONBLOCK.bits() != 0
    );

    let backend = i_slint_backend_linuxsgc::BackendBuilder::default()
        .with_drm_device(card, lease_fd)
        .build()
        .context("linuxkms backend init")?;
    slint::platform::set_platform(Box::new(backend)).context("set_platform")?;

    // Register fonts WITHOUT fontconfig: a fully static musl binary cannot
    // dlopen the board's glibc libfontconfig, so the fontique system source is
    // empty. Loading the font file into the shared collection gives the text
    // pipeline its fonts directly.
    register_font("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
        .context("registering DejaVuSans.ttf")?;

    let ui = MainWindow::new().context("creating the UI window")?;

    // Keep the sgc connection alive for the backend's lifetime; dropping it at
    // exit lets the daemon reclaim (revoke) the lease.
    let _connection = client;

    let ui_weak = ui.as_weak();
    let timer = slint::Timer::default();
    let mut phase: f32 = 0.0;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(33), move || {
        phase += 0.07;
        // Bounce the square across ~90% of a 1920-wide screen.
        let x = 1720.0 * (0.5 + 0.5 * phase.sin());
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_anim_x(x);
        }
    });

    println!("running event loop...");
    ui.run().context("event loop failed")?;
    Ok(())
}

/// Load a font file into the process-global fontique collection used by the
/// text pipeline. Returns the number of fonts registered.
fn register_font(path: &str) -> anyhow::Result<usize> {
    use slint::fontique_011::fontique;

    let bytes = std::fs::read(path)
        .with_context(|| format!("reading font file {path}"))?;
    let blob = fontique::Blob::new(std::sync::Arc::new(bytes));
    let mut collection = slint::fontique_011::shared_collection();
    let fonts = collection.register_fonts(blob, None);
    println!("registered {} font(s) from {path}", fonts.len());
    Ok(fonts.len())
}
