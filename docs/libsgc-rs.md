# libsgc-rs — client library design

Rust client library for `simple-graphics-controller` (`@sgc`). Apps link the
crate and drive a `SgcClient`; the client is a PLAIN SYNCHRONOUS wrapper
over the stream — one struct, one field, driven by ONE app thread. No
reader thread, no mutexes, no internal channels, no Drop impl.

This design is validated end-to-end in `sgc-fbdev-client` and
`sgc-fbdev-client-2` (identical architecture, different scenes): connect,
acquire, full preemption cycle (A holds → B preempts → A revoked with auto
revoke-ack → B granted → server requeues A → A re-granted → draws again)
on host + aarch64 board with a real `/dev/fb0`. libsgc-rs is the
extraction of that validated client code.

## The one decision: no reader thread

The threaded design (background "sgc-reader" thread + shared-state struct +
event receiver) was rejected as over-complicated. The final shape:

    pub struct SgcClient { stream: UnixStream }

Why this is sound: the app thread is ALWAYS on the socket when it matters —
blocked in `acquire()` it is not the owner yet (the server only revokes
owners, so no Revoke can target a queued acquirer), and blocked in
`start_event_loop()` it is always listening, so the server's 5s
revoke/ack deadlines are met. An app that must do other work while holding
the resource runs this client on its own thread — exactly like the demo's
main thread does next to the render thread.

Contract (stated once): while holding the resource, the client's thread
must stay in the event loop.

## Usage model (the app's view)

```
app start -> SgcClient::connect()                 connect + read Advertise
             fd = client.acquire(Fbdev)?          BLOCKS until the server
                                                  answers (queued requests
                                                  block here too)
             Ok(fd)       -> spawn render task, send it the fd
             Err(Denied)  -> not allowed; give up (or retry later)
             client.start_event_loop(&render_tx)  BLOCKS until the
                                                  connection ends; drives
                                                  the render task
revoked:    wire Revoke   -> send RenderCmd::Stop, reply Release
                             (the revoke-ack — resource disowned)
regranted:  wire Grant+fd -> send Ack, send RenderCmd::Draw(fd) — no
                             acquire needed
disconnect: EOF/error     -> send Stop, return
```

- `acquire()` BLOCKS the calling thread until the outcome is known:
  `Ok(fd)` = you may use the resource, `Err(Denied)` = you may not.
  Queued requests are indistinguishable from immediate grants — the
  blocked caller cannot tell "queued then granted" from "granted
  immediately", which is the point.
- Re-grant after revoke is a WIRE `Grant` handled inside the event loop
  (mapped to `RenderCmd::Draw(fd)`), not an `acquire()` result: the
  server handed the resource back on its own. `acquire()` is only for
  requesting (startup).
- There is NO event enum: the client maps wire messages straight to the
  app's command type (`RenderCmd`). Demo-internal coupling is fine; the
  lib extraction decides later whether to generalize.

## API (final shape)

    pub struct SgcClient { stream: UnixStream }   // single field, NOT Clone, &mut self

    impl SgcClient {
        pub fn connect() -> Result<(Self, Vec<Resource>), SgcError>;
            // connect + read Advertise; returns the advertised list so the
            // app can fail fast ("server does not offer Fbdev") instead of
            // eating a Deny round trip.
        pub fn acquire(&mut self, Resource) -> Result<OwnedFd, SgcError>;
            // write Acquire -> read one frame -> Ack -> return fd.
            // Deny { reason } -> Err(Denied).
        pub fn start_event_loop(&mut self, render_tx: &mpsc::Sender<RenderCmd>);
            // BLOCKING loop on the app thread until the connection ends:
            //   Revoke {resources} -> send RenderCmd::Stop to render,
            //                        reply Release (the revoke-ack; the
            //                        server waits <=5s for it)
            //   Grant + fd         -> send Ack (5s server timer), send
            //                        RenderCmd::Draw(fd) to render
            //   EOF/error          -> send Stop, return ("connection lost")
    }

`comm.rs` is folded into `client.rs` (`read_framed` is a private fn).
`RenderCmd` lives in the app's render module:

    pub enum RenderCmd {
        Draw(OwnedFd),   // a grant (initial or re-grant): draw with this fd
        Stop,            // revoked: stop rendering and drop the granted fd
    }

## App structure (the reference shape, from the demos)

- main thread: connect → check advertised resources → acquire (blocking)
  → spawn render task → send the granted fd as the first `Draw` → run
  `client.start_event_loop(&render_tx)` → send `Stop`, drop the sender,
  join the render task.
- render task (separate thread): receives `RenderCmd::Draw(OwnedFd)` /
  `Stop` through an mpsc channel; owns the granted fd (drops it on
  `Stop`); redraws its cached scene on every `Draw`.
- The demos render until killed: no `--hold` flag, no graceful release —
  the server frees the resource on our disconnect when the process dies.

## Render task / renderer constraints (linfb)

- linfb's `Compositor` is `!Send` (holds `Box<dyn Shape>`), so the
  Renderer CANNOT be built in main and moved into the thread — create it
  INSIDE the render task from the first granted fd. (Compositor is an
  owned struct — `compositor(&self, bg) -> Compositor`, no lifetime — so
  it stores fine next to the Framebuffer; it just can't cross threads.)
- linfb's `Framebuffer::open_with_fd` TAKES OWNERSHIP of the fd
  (`File::from_raw_fd`, closes on drop) — hand it a `dup`
  (`OwnedFd::try_clone`), keep the granted OwnedFd as single owner.
  OwnedFd's IO-safety check aborts on double close; the old
  `unsafe from_raw_fd` demo silently double-closed.
- Create the Renderer ONCE per app run and KEEP it across revokes: the
  fbdev mmap stays valid after the granted fd closes (mmap holds the file
  description) and the device is the same, so a revoked-then-regranted
  cycle just redraws. `Stop` drops the granted OwnedFd only; the renderer
  is dropped when the task ends (channel closed = main gone).
- Opening (ioctls + mmap) and building the scene (font loading) are the
  expensive parts — that is why they happen exactly once. Redrawing the
  same static compositor is idempotent (it covers the whole screen with
  its background), so a redraw is just draw + flush.

## Wire mechanics (unchanged, apply to any client)

- Every read is recvmsg (`sendfd::RecvWithFd`) — fds attach to the first
  bytes of a Grant frame; a plain read silently discards them. Header read
  included.
- Convert SCM_RIGHTS fds with `unsafe { OwnedFd::from_raw_fd(fd) }`
  (sound: ownership transfers; there is NO safe `TryFrom<RawFd>` for
  OwnedFd).
- Ack after every Grant (the server arms a 5s ack timer on every grant —
  the initial grant AND the unsolicited regrant). Release on Revoke is
  the revoke-ack (server force-reclaims after 5s and does NOT requeue).
- Regrant arrives unsolicited (no preceding Acquire) — that is the normal
  preemption handoff; the client just Acks and hands the fd on.

## Error mapping

`SgcError` (thiserror, 5 variants — no anyhow in the public API):

| site | variant |
| ---- | ------- |
| connect: no server / refused | `ConnectFailed(io)` |
| connect: bad frame or non-`Advertise` first message | `Protocol` / `UnexpectedMessage` |
| acquire: server deny | `Denied { reason }` |
| acquire: grant without exactly 1 fd | `Io(InvalidData)` |
| acquire/event loop: wire write/read failure | `Io` |
| any: frame decode | `Protocol` |

EOF inside the event loop is normal teardown ("connection lost"), not an
error the app must handle.

## Status

The design is FINAL and board-verified (see the intro). What remains is
the mechanical extraction:

| step | objective | state |
| ---- | --------- | ----- |
| 1 | validate the design inline in `sgc-fbdev-client` | done — committed (`ac9c601`); verified on the aarch64 board |
| 2 | sibling demo `sgc-fbdev-client-2` (same client, different scene) | done — committed (`ac9c601`) |
| 3 | extract `client.rs` + `error.rs` into `libsgc-rs`, replacing the phase-2 threaded scaffold | pending |
| 4 | rework `libsgc-rs` crate API surface to the final shape above | pending |
| 5 | fake-server tests (readiness handshake via `sync_channel(0)`; failure path via a nonexistent abstract name) | pending |
| 6 | real-daemon matrix on a test socket (needs `SGC_SOCKET` + `SGC_FBDEV_PATH` knobs on the server) | pending |
| 7 | demo refactor: `sgc-fbdev-client`/`-2` link libsgc-rs instead of their inline `client.rs` | pending |

Workflow: one step per commit, `cargo check`/`cargo test -p libsgc-rs`
green per commit, diff shown to the user before committing.

## Explicit non-goals (v1)

- Multi-resource `acquire` (server denies it anyway; revisit with the
  server).
- Acquire timeout (a future `acquire_timeout` is a one-liner — the reply
  is a direct read; do not build it in now).
- Non-blocking `acquire` (an app that must not block its main thread runs
  the client on a worker thread; the client itself never blocks anything
  else).
- Reconnect logic (session dies loudly; the app reconnects).
- The C client library (LVGL) — a later crate; the wire spec it needs is
  already in `docs/PROTOCOL.md`.
