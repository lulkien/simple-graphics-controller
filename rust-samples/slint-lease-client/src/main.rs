// Demo: run a Slint UI on a DRM lease granted by the simple-graphics-controller
// daemon (@sgc), via the linuxsgc backend (branch sgc-lease-1.17 fork work).
//
// The backend owns the whole @sgc session: BackendBuilder::default().build()
// connects to the daemon and acquires the card lease, and the backend's event
// loop pumps the session — a revoke suspends rendering until the lease is
// re-granted (display stack rebuilt), no app involvement. This app never sees
// SgcClient; it only builds the linuxsgc backend and runs.
//
// Renderer chosen by cargo feature: default = software (musl static builds),
// `--features femtovg` = OpenGL over gbm/EGL (gnu dynamic builds). The GL
// renderer cannot be rebuilt in-process (its EGL/GL context dies with the lease
// fd), so the femtovg flavor exits with an error when preempted — documented
// limitation of the backend.

use std::time::Duration;

use anyhow::Context;

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
            text: "the linuxsgc backend owns the @sgc session, not this app";
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
    // sgc or die: this connects to @sgc and acquires the card lease. Without a
    // running daemon the backend build fails and the app exits here.
    let backend = i_slint_backend_linuxsgc::BackendBuilder::default()
        .build()
        .context("linuxsgc backend init: is the @sgc daemon running?")?;
    slint::platform::set_platform(Box::new(backend)).context("set_platform")?;

    // Register fonts WITHOUT fontconfig: a fully static musl binary cannot
    // dlopen the board's glibc libfontconfig, so the fontique system source is
    // empty. Loading the font file into the shared collection gives the text
    // pipeline its fonts directly.
    register_font("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
        .context("registering DejaVuSans.ttf")?;

    let ui = MainWindow::new().context("creating the UI window")?;

    // Bounce the square. Runs even while a revoke suspends rendering — the
    // backend just skips frames until the lease is re-granted, then continues.
    let ui_weak = ui.as_weak();
    let mut phase: f32 = 0.0;
    let animation = slint::Timer::default();
    animation.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            phase += 0.07;
            let x = 1720.0 * (0.5 + 0.5 * phase.sin());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_anim_x(x);
            }
        },
    );

    ui.run().context("event loop failed")?;
    Ok(())
}

/// Load a font file into the process-global fontique collection used by the
/// text pipeline. Returns the number of fonts registered.
fn register_font(path: &str) -> anyhow::Result<usize> {
    use slint::fontique_010::fontique;

    let bytes = std::fs::read(path).with_context(|| format!("reading font file {path}"))?;
    let blob = fontique::Blob::new(std::sync::Arc::new(bytes));
    let mut collection = slint::fontique_010::shared_collection();
    let fonts = collection.register_fonts(blob, None);
    println!("registered {} font(s) from {path}", fonts.len());
    Ok(fonts.len())
}
