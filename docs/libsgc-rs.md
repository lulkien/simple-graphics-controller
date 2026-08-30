# libsgc-rs — client library design

Rust client library for `simple-graphics-controller` (`@sgc`). Apps link the
crate and drive a `SgcClient`; all wire work (framing, SCM_RIGHTS fd passing,
`Ack`, the `Revoke` handshake, re-grant) happens on the library's own reader
thread. The app calls blocking `acquire`/`release` and drains `SgcEvent`s for
ownership changes it did not ask for.

## Usage model (the app's view)

```
app start -> SgcClient::connect()            spawns the lib's reader thread
             fd = client.acquire(Fbdev)?     BLOCKS until the server answers
             Ok(fd)       -> render thread draws with fd
             Err(Denied)  -> not allowed; retry later or give up
             ... render ...
             client.release(Fbdev)            voluntary hand-back (returns
                                              once the Release is written)
preempted:  event: Revoked { resource }      drop fd, stop rendering
                                             (lib already sent the revoke-ack
                                              Release — resource disowned)
regranted:  event: Granted { resource, fd }  render again with the new fd —
                                             no acquire needed
```

- `acquire()`/`release()` BLOCK the calling thread until their outcome is
  known: when `acquire()` returns you KNOW whether you may use the resource
  (`Ok(fd)` = yes, `Err(Denied)` = no). All wire I/O still happens on the
  library's own reader thread; the calling thread waits on a reply channel.
- The event receiver (`mpsc::Receiver<SgcEvent>` from `connect`) is the
  channel between the lib thread and the app's main/render threads: it
  carries the re-granted fd (`OwnedFd` is `Send`).
- Re-grant after revoke is an EVENT (`Granted { fd }`), not an `acquire()`
  result: the server handed the resource back on its own. `acquire()` is
  only for requesting (startup / after a voluntary `release()`).
- An app that must not block its main thread calls `acquire()` from a
  worker thread (or accepts the block — there is nothing to render before
  the fd).

### Failure semantics (what the app must handle)

- `Revoked` is NON-fatal: drop the fd, stop rendering — everything else in
  the app keeps running (the library already sent the revoke-ack `Release`;
  the session stays connected). A later `Granted { fd }` resumes rendering.
- Session over (server died / connection dropped) IS fatal: the reader
  thread exits, so the event receiver's senders drop — the app's `recv()`
  returns `Err(Disconnected)` — and any blocked `acquire()` returns
  `Err(HandlePoisoned)`. The app drops its fds and exits (or reconnects).
  This is the ONLY fatal condition; there is no other signal.

## Status

| phase | objective | state |
| ----- | --------- | ----- |
| 1 | scaffold crate, Rust-idiomatic API shape | committed (`9604925`) |
| 2 | connect + Advertise + reader thread + RAII Drop (shutdown(2) + join) | **implemented, uncommitted — awaiting review gate** |
| 3 | `acquire` (blocking request/reply) + auto-`Ack` | pending |
| 4 | `release` + held-resource tracking | pending |
| 5 | `Revoke` event + auto revoke-ack `Release` + re-grant (`Granted { fd }` event) | pending |
| 6 | test matrix + docs | pending |
| 7 | demo client refactor (`sgc-fbdev-client` onto the lib) | pending |

Workflow: one phase per commit, `cargo check`/`cargo test -p libsgc-rs` green
per commit, diff shown to the user before committing.

## Threading model

```
app/main thread                            reader thread ("sgc-reader", blocking)
acquire():                                  loop:
  lock request mutex                          blocking recvmsg one frame
  write Acquire (write mutex, one frame)        Grant  -> resolve pending /
  block on reply channel                                 fire Granted{fd},
  -> Ok(fd) | Err(Denied)                              auto-Ack
release():                                    Deny   -> fail pending acquire
  check held, write Release                   Revoke -> auto-Release (revoke-ack),
  -> returns once written                               fire Revoked
event loop:                                  EOF    -> fail pending, mark dead,
  drain Receiver<SgcEvent>                            exit
Drop: shutdown(2) -> reader EOF -> join
```

- Inbound is the reader thread's whole job. It owns the socket for READING
  (the stream moved into `reader_main`).
- Outbound is written through a shared `Mutex<UnixStream>` (a `try_clone` of
  the same socket): the app writes `Acquire`/`Release`, the reader writes
  `Ack` and the revoke-ack `Release`. The mutex is held ONLY for the duration
  of one write — never while waiting on a reply (or the reader's revoke-ack
  write would deadlock behind a blocked `acquire`).
- Teardown: last `SgcClient` clone's `Drop` calls `shutdown(2)` on a socket
  clone -> the reader's blocking read returns EOF -> thread exits -> join.
  No poll, no pipe, no command channel.
- Blocking is per-call on the CALLING thread; the library's own threads
  never block on the app.

## Shared state (`ClientInner`)

| field | type | written by | purpose |
| ----- | ---- | ---------- | ------- |
| `shutdown` | `UnixStream` (clone) | — | `Drop` unblocks the reader via shutdown(2) |
| `join` | `Mutex<Option<JoinHandle<()>>>` | `Drop` | last clone joins the reader |
| `write` | `Mutex<UnixStream>` (clone) | app + reader | all outbound frames (one write per lock) |
| `request` | `Mutex<()>` | app | serializes `acquire()`: ONE outstanding acquire |
| `pending` | `Mutex<Option<oneshot::Sender<Result<OwnedFd, SgcError>>>>` | app + reader | the blocked `acquire()`'s reply channel |
| `held` | `Mutex<HashSet<Resource>>` | app + reader | what this session owns (gates `release`, revoke-ack) |
| `dead` | `AtomicBool` | reader | set on reader exit; `acquire`/`release` fail fast with `HandlePoisoned` |
| `events` | `mpsc::Sender<SgcEvent>` | reader | lives in `reader_main` (already wired) |

`acquire()` holds `request` across the WHOLE call (lock -> write -> reply
wait): the reply channel is a single slot, so only one acquire may be in
flight. The `write` mutex is only taken for each individual frame.

## Reader dispatch

`reader_main` loop, per received message:

| server message | action |
| -------------- | ------ |
| `Grant { resources }` + fds | validate `fds.len() == resources.len()` (violation = fatal: log, mark dead, exit). Convert the fd to `OwnedFd` (safe `OwnedFd::try_from` — never the demo client's `unsafe from_raw_fd`). If `pending` is set: resolve it with `Ok(fd)` — the blocked `acquire()` returns, NO event. Else the grant is the unsolicited re-grant after revoke: fire `SgcEvent::Granted { resource, fd }`. Then `held.insert` and send `Ack` (library auto-acks — the app never sees the wire). |
| `Deny { reason }` | if `pending` set -> resolve with `Err(Denied { reason })`; else log warn (unexpected) |
| `Revoke { resources }` | per resource: if in `held` -> remove, send `Release` (the revoke-ack; server is waiting inside `REVOKE_TIMEOUT`), then fire `SgcEvent::Revoked`. If NOT held -> log warn, no `Release` (nothing to release; a stray Release just makes server-side noise) |
| `Advertise { .. }` | duplicate (server sends only on connect) -> log warn, ignore |
| other / unparseable | log error, ignore (forward compat) |

Protocol violations (fd-count mismatch, grant for a resource nobody asked
for) are fatal: log error, mark dead, exit the reader. Staying connected to
a corrupting server is worse than a loud failure.

Reader exit path: fail `pending` if set (drop the sender — the blocked
`acquire()`'s `recv()` errors and maps to `HandlePoisoned`), mark `dead`,
drop the `events` sender (the app's `recv()` returns `Err(Disconnected)` =
"session over"), exit.

## API semantics

```
pub fn acquire(&self, resource: Resource) -> Result<OwnedFd, SgcError>   // blocks
pub fn release(&self, resource: Resource) -> Result<(), SgcError>
```

### acquire()

BLOCKING request/reply. The ONE request call: app startup (or again after a
voluntary `release()`). A re-grant after revoke does NOT go through
`acquire()` — the server hands the resource back unsolicited and the fresh
fd arrives inside `SgcEvent::Granted`.

1. Lock `request` (serializes concurrent callers; second thread waits).
2. Create a `oneshot`, stash the sender in `pending`, write
   `Acquire { resources: [resource] }` (write mutex, one write), then block
   on the reply:
   - `Grant` -> `Ok(OwnedFd)`
   - `Deny` -> `Err(Denied { reason })` (e.g. first-owner policy, or
     "already owned by this client")
   - reader died -> `Err(HandlePoisoned)`
   - queued: the server sends NO reply; the `Grant` arrives later (via the
     engine's control channel) and resolves the pending acquire exactly like
     an immediate grant. The blocked caller cannot tell "queued then
     granted" from "granted immediately" — which is the point.
3. Blocks indefinitely (v1). A future `acquire_timeout` is trivial: the
   reply is a channel, so `recv_timeout` is a one-liner. Do not build the
   timeout in now.

Deliberately NOT pre-checking `held` (the server's "already owned" Deny is
the single source of truth — no local state drift).

### release()

Voluntary hand-back. Check `held`: not held -> `Err(ResourceNotHeld { resource })`
(also covers "revoke already disowned it for me" — the reader removed it
from `held` when it sent the revoke-ack, so a double `Release` never hits
the wire). Otherwise write `Release { resources: [resource] }`, remove from
`held`, return `Ok(())`. The server sends no reply, so "blocking" means
"returns once the frame is written".

`release()` is for VOLUNTARY hand-back. On `Revoke` the library already sent
the `Release` on the app's behalf; the app's job is only to drop the
`OwnedFd` and stop drawing.

### Events

- `Granted { resource, fd }` — the server re-granted the resource after
  revoking us (we were requeued). The fresh fd is IN the event: start
  drawing with it immediately — no `acquire` call. Only fires when no
  `acquire` is in flight: a grant that answers a pending `acquire` is
  returned directly by that call. The protocol `Ack` was already sent.
  `SgcEvent` is `Debug`-only (it carries `OwnedFd`, which is
  `!Clone`/`!PartialEq`).
- `Revoked { resource }` — drop the fd, stop drawing. The revoke-ack
  `Release` was already sent; the resource is disowned.

### Drop

As in phase 2. Last clone: `shutdown(2)`, join. The reader's exit path fails
`pending` and marks `dead`; nothing else to do. Panic in the reader is
tolerated (join result ignored; a blocked `acquire` still wakes via the
dropped channel).

## Error mapping

| site | variant |
| ---- | ------- |
| connect: no server / refused | `ConnectFailed(io)` |
| connect: bad frame or non-`Advertise` first message | `Protocol` / `UnexpectedMessage` |
| acquire: server deny | `Denied { reason }` |
| acquire: reader died while waiting | `HandlePoisoned` |
| acquire/release after reader death | `HandlePoisoned` |
| acquire/release: wire write failure | `Io` |
| release: not held | `ResourceNotHeld { resource }` |
| any: frame decode | `Protocol` |

No `anyhow` in the public API (thiserror only, already the case).

## Phase plan

### Phase 3 — acquire (blocking request/reply) + auto-Ack

- `ClientInner`: add `write`, `request`, `pending`, `held`, `dead`.
- `reader_main`: dispatch `Grant` (resolve pending with the fd, auto-Ack; no
  pending -> warn + close the fds so nothing leaks) and `Deny` (fail
  pending); mark `dead` on exit.
- `SgcClient::acquire` implemented per above.
- Tests (fake-server pattern, readiness handshake via `sync_channel(0)`):
  - acquire granted immediately -> `Ok(fd)`, server observed the auto-`Ack`
  - acquire denied -> `Err(Denied { reason })`
  - server dies while acquire waits -> `Err(HandlePoisoned)`
  - two threads racing `acquire` -> serialized (request mutex), both
    eventually served
  - unsolicited `Grant` with no pending -> fds closed, no leak, session
    stays alive

### Phase 4 — release + held tracking

- `held` maintained by `Grant` (reader) and `release()` (app).
- `SgcClient::release` implemented per above.
- Tests:
  - release ok, server observed `Release`
  - double release -> `ResourceNotHeld`
  - release of never-acquired resource -> `ResourceNotHeld`

### Phase 5 — revoke + re-grant

- Reader: `Revoke` -> auto-`Release` + `Revoked` event; unsolicited `Grant`
  (no pending) -> `Granted { resource, fd }` event.
- Tests (fake server, full wire fidelity incl. SCM_RIGHTS via `sendfd`):
  - revoke while holding -> `Revoked` event fired, server observed the
    auto-`Release`
  - revoke for a resource not held -> no `Release` on the wire, warn only
  - re-grant with no pending acquire -> `Granted { fd }` fires, fd usable
    immediately, and the fake server saw NO second `Acquire` frame
  - full preemption cycle: acquire -> `Ok(fd)` -> revoke (`Revoked`, drop
    fd) -> re-grant (`Granted { fd }`, draw again) — the app made exactly
    one `acquire` call, ever

### Phase 6 — test matrix + docs

- Run the full matrix against the REAL daemon, spawned as a subprocess on a
  test abstract socket. Requires two small daemon testability knobs (server
  side, one commit):
  - `SGC_SOCKET` env: abstract socket name (default `sgc`) — currently
    hardcoded in `main.rs:71`
  - `SGC_FBDEV_PATH` env: device path (default `/dev/fb0`) — so a host
    without fbdev can register `/dev/null` (matches the controller's
    integration-test trick)
- Matrix: immediate grant; deny (first-owner); queued grant; preemption
  handoff (revoke -> auto-release -> regrant); force-reclaim (server
  revokes, lib silent -> server reclaims, lib survives); server death
  mid-session (`HandlePoisoned` + event channel disconnect); drop mid-acquire.
- Docs: this file gains a usage section (short example: connect, acquire,
  draw, drain events); `docs/README.md` workspace table gains `libsgc-rs`;
  root `README.md` links stay minimal.

### Phase 7 — demo client refactor

- `sgc-fbdev-client` drops raw protocol handling AND tokio: use
  `SgcClient::connect` + event receiver + `acquire`/`release`. `linfb`
  drawing stays.
- Main loop: `fd = acquire()` once -> draw -> drain events: `Granted { fd }`
  draws with the new fd, `Revoked` stops rendering; `--hold` keeps looping,
  ctrl-c releases and exits.
- Removes `tokio`, `sendfd`, `rmp-serde` (transitively) from the demo
  client's dependency tree.

## Explicit non-goals (v1)

- Multi-resource `acquire` (server denies it anyway; revisit with the
  server).
- Acquire timeout (future `acquire_timeout`, one-liner on the reply
  channel).
- Non-blocking `acquire` (an app that must not block its main thread calls
  `acquire()` from a worker thread; the library itself never blocks its
  reader thread).
- Reconnect logic (session dies loudly; the app reconnects).
- The C client library (LVGL) — a later crate; the wire spec it needs is
  already in `docs/PROTOCOL.md`.
