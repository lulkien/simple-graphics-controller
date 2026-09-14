# AGENT.md — simple-graphics-controller

## Purpose
The `@sgc` daemon: resource manager for the board's graphics + input devices (running as root). Owns the board's DRM masters, `/dev/fb0`, and `/dev/input/event*` devices; grants fresh kernel leases (DRM) or dup fds (input/fbdev) to clients over the abstract Unix socket `@sgc`. Implements the policy engine for grant/deny/revoke arbitration across Fbdev, Drm, and Input resources.

## Architecture
- **Single policy engine** — one engine arbitrates everything: Drm, Fbdev, and Input alike; grant, or queue with owner preemption, or deny. No exceptions.
- **Three compile-time backends** — `drm` (default, fresh kernel lease), `input` (default, dup fd), `fbdev` (opt-in, dup fd); a build without a backend never advertises it, and `Acquire` against it is denied "not registered"
- **One task per connection** — client task handles one connection; policy engine sends control messages (Revoke/Grant); client task drives the protocol
- **Ask-first revoke** — the wire `Revoke` is a request; the owner's `Release` is the ack, then it is requeued for one more turn; a silent owner is force-reclaimed after 5s (no requeue); for DRM the kernel `revoke` runs at the handoff, so a revoked client keeps a valid lease through the grace window and can finish its frame
- **Resource registries** — static fds for Fbdev/Input (grants are dups of these); DRM lease factories create fresh lease fds per grant; the server never closes the master fds

## Rust Best Practices (per rust-skills, applied to daemon)
- [`own-borrow-over-clone`] — Grants are dups of the daemon's static fds; the canonical stays with the daemon; `fd()` lends dups (client owns the dup). See `resource_manager::query_resource` and `client_handler::wire`
- [`own-arc-shared`] — `ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>` shared across the server task and client tasks; `Arc` is justified because the registry is genuinely shared across threads (policy engine + multiple client connections)
- [`own-refcell-interior`] — Not used in this crate (mutability handled via DashMap atomically; no single-threaded interior mutability needed); pattern to keep in mind for future refactors
- [`own-cow-conditional`] — Use `Cow<'a, T>` for conditional ownership where appropriate (e.g. error messages that may or may not include context)
- [`err-result-over-panic`] — Return `Result<T, E>` instead of panicking for recoverable errors; server uses `anyhow::Context` for wrap; `server::run` returns `anyhow::Result<()>`
- [`err-from-impl`] — `SgcError` implements `From<ProtocolError>` and `From<std::io::Error>` via `#[from]` to enable `?` operator throughout; clean error propagation
- [`err-question-mark`] — Use `?` operator for clean error propagation; throughout the daemon, `?` is the primary error-propagation mechanism
- [`err-context-chain`] — Add context with `.context()` or `.with_context()` when wrapping errors from I/O or protocol layer; e.g. `client.connect().context("failed to connect")`, `stream.write_all(&frame).map_err(SgcError::Io).context("write frame")`
- [`err-no-unwrap-prod`] — Avoid `unwrap()` in production code; use `?`, `expect()`, or handle errors; `expect()` only for invariants indicating bugs
- [`expect-bugs-only`] — Use `expect()` only for invariants that indicate bugs, not user errors or runtime conditions; e.g. `Box::into_raw(ctx)` should never fail, so `expect` is appropriate there
- [`mem-with-capacity`] — Use `Vec::with_capacity()` when size is known; e.g. `advertised.reserve()`, ` registries.fds.entry()`
- [`perf-iter-over-index`] — Prefer iterators over manual indexing; e.g. `advertised.iter()` over indexing into `advertised`; `registries.fds.keys().collect()`
- [`num-nonzero`] — Use `NonZero*` types to forbid zero and unlock niche optimization; not currently in daemon but principle applies to resource indices / timeout values
- [`api-from-not-into`] — Implement `From<T>`, not `Into<U>` — `SgcError::from` gives you `Into` for free; all fallible conversions use `From`
- [`api-must-use`] — Mark types and functions with `#[must_use]` when ignoring results is likely a bug; e.g. `acquire` return value, `pump` result, `write_frame` result, `query_resource` result
- [`doc-all-public`] — Document all public items with `///` doc comments; all public types, functions, and modules have doc comments
- [`doc-errors-section`] — Include `# Errors` section documenting all error variants; `SgcError` has `# Errors` doc section
- [`doc-panics-section`] — Include `# Panics` section for functions that can panic under documented conditions; e.g. `expect()` documentation
- [`doc-question-mark`] — Use `?` in examples, not `.unwrap()`; examples should demonstrate proper error handling
- [`obs-tracing-over-log`] — Use `tracing` for structured, span-aware diagnostics instead of `println!` or bare `log`; the daemon uses `tracing_subscriber::EnvFilter` + `tracing::fmt()`; structured fields keyed not interpolated into message strings
- [`obs-structured-fields`] — Record structured key-value fields, not values interpolated into the message string; `info!("...: {advertised:?}")` is structured, good
- [`anti-lock-across-await`] — Never hold `Mutex`/`RwLock` across `.await`; the daemon is async (tokio main) but the policy engine and client handler communicate via channels, not locks across await points; the policy engine is single-task (one policy for all resources)
- [`anti-clone-excessive`] — Don't clone when borrowing works; the daemon: `ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>` — the DashMap holds `OwnedFd` which is Clone (dups the fd), but the canonical is never cloned unnecessarily; `fd()` clones the `OwnedFd` intentionally to lend a dup
- [`anti-type-erasure`] — Don't use `Box<dyn Trait>` when `impl Trait` works; the daemon uses concrete types: `PolicyEngine`, `Policy`, `ResourceRegistry`, `ClientId`; `windowing::spawn` creates the engine on one thread
- [`anti-stringly-typed`] — Don't use strings where enums or newtypes would provide type safety; resource kinds use `Resource` enum, not strings; policies use `Policy` enum, not ad-hoc strings

## Key Types & Functions
- `Resource` — enum: `Fbdev`, `Drm { card: u8 }`, `Input(InputResource)`; all resource kinds
- `InputResource` — enum: `Mouse(u8)`, `Keyboard(u8)`, `Touch(u8)`; input device indices
- `Policy` — enum: `FairQueue` (default), `LatestOwner`, `FirstOwner`; arbitration policy
- `PolicyEngine` — spawned on one task (`PolicyEngine::spawn(policies)`); arbitrates all resource grants/denies/revokes; one global engine for all resources
- `ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>` — shared across server + client tasks; holds static fds for Fbdev/Input; DRM leases created per grant
- `ResourceRegistries` — `fds: ResourceRegistry` (static fds) + `drm: DrmRegistry` (lease factories); cloned per client connection
- `OpenedResources` — returned by `query_resource()`; registries + advertised order
- `ClientId(u64)` — server-assigned monotonic identity for a connected client; keyed on connection not pid
- `sgns_client_handler::wire` — wire protocol messages: `ControlMessage` (Revoke/Grant), `ClientMessage` (Acquire/Release/Ack)
- `client_handler::control` — per-connection state: `ClientHandler` owns the stream, sessions, and resource borrows
- `windowing::engine` — DRM lease management: `build` connects @sgc + acquires the Drm lease (sgc or die); `revoke=suspend`, `re-grant=rebuild`
- `server::run` — the accept loop: `server::run(engine, registries, advertised).await`

## Policy Engine Policies
| policy | newcomer when resource held | waiters served |
|---|---|---|
| `first-owner` | denied (kiosk/locked-down) | — |
| `latest-owner` | preempts the owner ("bring to front") | newest first |
| `fair-queue` (default) | preempts the owner | oldest first |

## Build & Run
```sh
just                          # release build (drm + input), or:
cargo build --release         # same
just build-gnu-aarch64        # board: gnu dynamic
just build-musl-aarch64       # board: fully static musl
just dist-gnu-aarch64         # + strip into ./dist (build output, gitignored)
just packages                 # all four installable .debs into target/debian/
just ci                       # the gate: fmt + clippy + tests + packages

Run as **root** (it opens `/dev/dri` + `/dev/input`):
RUST_LOG=info ./target/release/simple-graphics-controller

Features:
- default: `drm` + `input` — both built; fbdev opt-in via `--features fbdev`
- `--features fbdev` — enable legacy fbdev path
- `SGC_POLICY=first-owner|latest-owner|fair-queue` — env var overrides default fair-queue
```

## Packaging & CI
`just ci` is the gate: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, then every package flavor. It is just recipes on purpose (no GitHub Actions workflow), so the same command runs on a workstation and on any runner.

Four `.deb` flavors, all built by `just packages` into `target/debian/`:

| recipe | target | linkage | package name |
|---|---|---|---|
| `package-gnu-x86_64` | x86_64-unknown-linux-gnu | dynamic glibc | `simple-graphics-controller` |
| `package-musl-x86_64` | x86_64-unknown-linux-musl | fully static | `simple-graphics-controller-musl` |
| `package-gnu-aarch64` | aarch64-unknown-linux-gnu | dynamic glibc | `simple-graphics-controller` |
| `package-musl-aarch64` | aarch64-unknown-linux-musl | fully static | `simple-graphics-controller-musl` |

Each installs `/usr/bin/simple-graphics-controller` and `/lib/systemd/system/simple-graphics-controller.service`; `debian/postinst` reloads the unit, enables it at boot and starts it, `debian/prerm` stops and disables it.

Rules that bite:

- `dist/` is gitignored build output; package sources live in `packaging/` (unit, board drop-in) and the install notes in `docs/packaging.md`. A unit kept in `dist/` breaks a fresh clone: cargo-deb needs every asset file to exist.
- cargo-deb names its output `<name>_<version>_<arch>.deb` whatever `--variant` says, and it rewrites `target/debian` when it packages. Two flavors for one architecture therefore need distinct package names; renaming after the fact loses a race with the next flavor's build.
- glibc and musl each declare `Conflicts`/`Replaces` on the other (base metadata and the `musl` variant in `Cargo.toml`). One-sided, the swap works in one direction and fails in the other with a file conflict on `/usr/bin`.
- `debian/postinst` enables unconditionally. A flavor swap runs the removed package's `prerm` first, which disables the unit, so a "keep the previous state" check leaves the replacement disabled.
- Cross targets cannot run `dpkg-shlibdeps`, so the variants state `depends` explicitly: `libc6` for glibc, empty for musl.
- The unit is ordered after `dev-dri-card0.device`. Without it a boot-time start races udev, fails to take DRM master, and burns the restart budget.

Verified on the board (`root@10.21.50.53`) by `dpkg -i` of an arm64 flavor, then `dpkg-query -W`, `systemctl is-enabled/is-active` and `journalctl -u`; installing the second flavor over the first proves the conflict declarations. amd64 installability was checked in a `debian:trixie` container, because this workstation has `/dev/dri` and a local install would take DRM master from the desktop.

## Policy Engine Design (step-by-step)
The policy engine is a single task that arbitrates all resource grants/denies/revokes. It maintains one slot per resource with: owner, waiters queue, revoke timer. When an `Acquire` arrives:
1. If resource free → Grant immediately
2. If resource held + newcomer == owner → Deny (already owns it)
3. If resource held + newcomer != owner → apply policy:
   - `first-owner` → Deny
   - `latest-owner` → Preempt owner (newcomer becomes owner, old owner queued)
   - `fair-queue` → Queue newcomer (oldest waiter gets next Grant)

When a `Release` arrives:
1. Remove the resource from held
2. If waiters exist → dequeue oldest, Grant to them
3. If no waiters → resource becomes free, next Acquire gets Grant immediately

When a `Revoke` arrives:
1. It is a **request**, not an immediate revoke
2. The owner keeps a valid fd (DRM: valid lease) for the grace window (5s)
3. The owner must send `Release` within the grace window (the revoke-ack)
4. After grace timeout, server force-reclaims (DRM: kernel-enforced revoke; fbdev/input: cooperative)
5. On revoke-ack, the owner is requeued at the back of the waiters list

## Common Pitfalls to Avoid
- ❌ Do NOT call `.unwrap()` in production paths — use `?`, `expect()` only for invariants indicating bugs
- ❌ Do NOT ignore `SGC_POLICY` env var — default is `fair-queue`; changing it requires recompile and affects all resources
- ❌ Do NOT build without at least one backend — the daemon will advertise nothing and all Acquires are denied
- ❌ Do NOT forget to run as **root** — it opens `/dev/dri` + `/dev/input`; running as non-root causes I/O errors on startup
- ❌ Do NOT hold locks across await points — the daemon is async (tokio) but the policy engine is single-task; client handlers use channels, not locks
- ❌ Do NOT advertise a resource without opening it — each backend module (`drm`, `input`, `fbdev`) only registers its resource when its feature is enabled
- ❌ Do NOT forget the 5s revoke grace window — the owner must Acknowledge within 5s or the server force-reclaims; for DRM the kernel enforces this at the ioctl level
- ❌ Do NOT mix `Arc` and `Rc` carelessly — `ResourceRegistry = Arc<DashMap<...>>` is genuinely shared across threads (server task + multiple client tasks); if a type is only ever accessed from one thread, prefer `Rc` over `Arc` to avoid atomic refcount overhead
- ❌ Do NOT forget to close dup fds — when a borrower is done with a lent fd, it must drop it; the canonical stays with the client, but the dup is the borrower's responsibility
- ❌ Do NOT use `mem::zeroed()` or `mem::uninitialized()` for types with validity invariants — use `MaybeUninit` instead; the daemon uses `OwnedFd` which has validity invariants (fd >= 0)
- ❌ Do NOT forget the `SgcError` variants — all fallible functions return `Result` with `SgcError`; possible variants: `ConnectFailed`, `Denied { reason }`, `NotHeld { resource }`, `NotAvailable { resource }`, `Protocol(ProtocolError)`, `UnexpectedMessage(ServerMessage)`, `Io(io::Error)`

## Windowing Backend Design
- **DRM backend** (`--features drm`): creates fresh kernel lease fds per grant; the master fd never leaves the daemon; granted fd is a lease (holder can modeset on objects but never becomes DRM master); the server can revoke it at any time; kernel `revoke` runs at the handoff, so a revoked client keeps a valid lease through the grace window and can finish its frame
- **Fbdev backend** (`--features fbdev`): uses `/dev/fb0`; grants are dups of the daemon's fd; legacy path; no kernel lease involved
- **Input backend** (`--features input`): uses `/dev/input/event*`; grants are dups of the daemon's fd; devices are enumerated at startup AND reconciled while the daemon runs (`resource_manager::hotplug`, 2 s): a device that appears is adopted and offered to the engine, one that goes away is withdrawn (holder revoked), and a node that udev re-creates for the same device is re-opened for future grants without disturbing its holder

## Windowing Engine Lifecycle
1. **Build** — `PolicyEngine::spawn(policies)` starts one background task; no clients connected yet
2. **Connect** — client connects to `@sgc`; daemon reads `Advertise` on connect; sends available resources
3. **Acquire** — client requests a resource; policy engine arbitrates; on grant, daemon sends `Grant + fd`; client stores canonical fd in `held`
4. **Pump** — client pumps for events; returns `SgcEvent::Granted` (fresh dup lent to app) or `SgcEvent::Revoked` (drop fd, stop drawing)
5. **Release** — client releases a resource; policy engine dequeues next waiter or marks resource free
6. **Revoke** — client (or daemon) initiates revoke; owner has 5s grace window to send `Release` (revoke-ack); after timeout, force-reclaim
7. **Disconnect** — stream dies; surface one `Revoked` per still-held resource, then surface the fatal error

## Integration Test Conventions
- Tests in `#[cfg(test)] mod integration_tests { }` in `main.rs`
- Test the full wire protocol e2e over a real abstract socket
- Use `fake_server` / `FakeController` from `libsgc-rs` for bidirectional protocol tests
- Tests cover: connect, acquire-denied, acquire-grant, pump flow, revoke/re-grant cycle, policy arbitration
- Tests should compile and run without external dependencies (fake in-process server via Unix socket)
- Use `sendfd::SendWithFd`/`RecvWithFd` for SCM_RIGHTS fd passing in tests
- Test all three policies: `first-owner`, `latest-owner`, `fair-queue`