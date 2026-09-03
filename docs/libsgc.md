# libsgc client library — one core, three faces

The client library for the simple-graphics-controller daemon (`@sgc`),
delivered to three audiences from ONE Rust core:

- **Rust** — the native crate (`libsgc-rs`): `SgcClient` + `SgcEvent`.
- **C** — a C ABI (`include/libsgc.h`) over the same core, from the
  `libsgc-c` shim crate.
- **C++** — a thin RAII header-only wrapper (`include/sgc.hpp`) over the
  C ABI (no second implementation, no protocol code in C++).

## Why one core

The protocol is the expensive part to get right: msgpack framing, SCM_RIGHTS
fd passing, the Ack timer, the ask-first Revoke handshake, and the
revoke/requeue/regrant lifecycle. It exists once, in Rust, and is
board-verified (DRM and Fbdev cycles on the dev board, and kmscube -L as a
real C consumer). Every other language gets a *view* of that core, never a
second copy that can drift. C and C++ consume the C ABI; Rust consumes the
crate. The wire format is fixed in [PROTOCOL.md](PROTOCOL.md) and pinned by
the `wire_dump` golden bytes.

## Architecture

```
        +----------------------------------------------+
        |                 libsgc core (Rust)            |
        |  SgcClient: pump-based, no background threads |
        |  - connect/acquire  (blocking request/answer) |
        |  - pump(): one frame -> Option<SgcEvent>      |
        |  - revoke/regrant lifecycle, Ack, fd lending  |
        +----------------------------------------------+
                    |                        |
         native use |                C ABI (libsgc-c)
                    v                        v
        Rust apps (crate)      +----------------------------+
                               |  include/libsgc.h          |
                               |  opaque sgc_client* handle |
                               |  sgc_* functions           |
                               +----------------------------+
                                             |
                          +------------------+------------------+
                          |                                     |
                          v                                     v
                    C apps (kmscube-style)      C++ RAII wrapper (sgc.hpp,
                                               header-only, move-only)
```

The core owns all protocol state. Handles are opaque; fds cross the ABI as
plain `int` with documented ownership (the grant fd is owned by the caller
until released; `sgc_fd` returns a dup the caller closes).

## The pump core (the Rust-native shape)

`SgcClient` drives the socket with an explicit **pump**, so every language
face is a thin adapter over the same state machine:

```rust
pub struct SgcClient { /* stream, held canonicals, pending acquire */ }

impl SgcClient {
    pub fn connect() -> Result<(Self, Vec<Resource>), SgcError>;
    pub fn acquire(&mut self, resource: Resource) -> Result<(), SgcError>;
    pub fn fd(&self, resource: &Resource) -> Result<OwnedFd, SgcError>;
    pub fn held(&self) -> Vec<Resource>;

    /// Drive the protocol: read one frame and return the resulting event.
    /// Blocks up to `timeout` (None = until a frame or error arrives);
    /// Ok(None) = nothing happened (caller re-pumps).
    pub fn pump(&mut self, timeout: Option<Duration>)
        -> Result<Option<SgcEvent>, SgcError>;
}

pub enum SgcEvent {
    Revoked { resource: Resource },              // stop drawing, drop the dup
    Granted { resource: Resource, fd: OwnedFd }, // fresh dup, draw
}
```

Why a pump and not only callbacks:

- C cannot express Rust closures over a borrowed `&mut SgcClient`. A pump is
  a plain function call: `sgc_pump(client, timeout_ms)` — the caller loops,
  exactly like `poll()`.
- Re-entrancy is structurally impossible: `acquire()` is only called between
  pumps, and `pump()` never invokes user code, so a callback cannot
  accidentally re-enter the client (the C footgun a callback API would
  export).
- Loop-shaped consumers (LVGL, game loops, kmscube's render loop) drive
  `pump()` from their own main loop with a small timeout — no thread
  ownership imposed on the app.
- The library performs the protocol work (Ack after Grant, Release as the
  revoke-ack) inside `pump()` before returning the event, so every face gets
  correct wire behavior for free. Release-then-revoke-ack arrives before the
  event, well inside the server's grace period.

Disconnect semantics: when the connection dies, `pump` first returns
`SgcEvent::Revoked` for every still-held resource (one per call, in no
particular order), then the fatal error. No state leaks across the ABI
boundary.

The callback convenience (`start_event_loop`) is a thin wrapper that calls
`pump(None)` in a loop and dispatches `SgcEvent` to the app's `FnMut`.

## The C ABI

`libsgc-c` is a shim crate: `#[repr(C)]` types + `#[unsafe(no_mangle)]`
`extern "C"` functions over the core, `catch_unwind` at every entry point.
The library name is `sgc`, so consumers link `libsgc.a` / `libsgc.so` —
built with the rest of the workspace (`just build` for the host,
`just build-gnu-aarch64` for the board).

Resource kinds are flat ints — the input classes are distinct kinds so
kind+index is a total round-trip encoding of the 3-level Rust enum:

| kind | index                       |
|------|-----------------------------|
| 0  FBDEV   | ignored               |
| 1  DRM     | card index            |
| 2  MOUSE   | device index          |
| 3  KEYBOARD| device index          |
| 4  TOUCH   | device index          |

```c
typedef struct sgc_client sgc_client;          /* opaque */

typedef struct {
    int kind;   /* SGC_RESOURCE_FBDEV | DRM | MOUSE | KEYBOARD | TOUCH */
    int index;  /* card / device index */
} sgc_resource;

typedef struct {
    int kind;         /* SGC_EVENT_REVOKED | SGC_EVENT_GRANTED */
    sgc_resource resource;
    int fd;           /* GRANTED only: owned by the caller, close() it */
} sgc_event;

sgc_client *sgc_connect(char *err, size_t err_len);          /* NULL on error */
int  sgc_advertised(sgc_client *c, sgc_resource **out, size_t *count); /* malloc'd, sgc_free() it */
void sgc_free(void *p);
int  sgc_acquire(sgc_client *c, sgc_resource r, char *err, size_t err_len);
int  sgc_pump(sgc_client *c, int timeout_ms, sgc_event *out, char *err, size_t err_len);
int  sgc_fd(sgc_client *c, sgc_resource r, char *err, size_t err_len); /* dup, caller closes */
void sgc_release(sgc_client *c);                            /* drop + close, NULL ok */
```

Rules that keep the ABI honest:

- Opaque handle, no struct layout exposed.
- All fallible functions that take an `err` buffer return `0` on success /
  `-1` (or `NULL`) with a message in it (the C analog of `SgcError`); no
  panics cross the boundary — `catch_unwind` at every entry point.
  `sgc_advertised` is the exception: it signals failure with `-1` only.
- The grant fd and `sgc_fd()` dups transfer ownership to the caller; the
  header says so next to every fd-returning signature.
- `sgc_pump` returns `1` = event stored in `*out`, `0` = nothing happened,
  `-1` = connection error; `timeout_ms` is `-1` block forever / `0` poll
  once / `>0` milliseconds — the C loop-shape matches `poll(2)`.
- The advertised list crosses the ABI as a malloc'd array the caller frees
  with `sgc_free` (plain malloc/free pairing).
- Enums are plain `int` constants, not C enums, so ABI size is stable.

The header is handwritten (the surface is ~6 functions + 2 structs);
cbindgen earns its keep only if the surface grows. The host build emits
`libsgc.a` + `libsgc.so`; the aarch64 build is static-only (`crt-static`
drops the cdylib) — board consumers link `libsgc.a`.

## The C++ face

A header-only `sgc.hpp` over the C ABI (same include dir), giving RAII
without protocol logic:

```cpp
class SgcClient {           // move-only; dtor calls sgc_release
public:
    static std::optional<SgcClient> connect(std::string *err = nullptr);
    std::vector<Resource> advertised() const;
    std::optional<std::string> acquire(const Resource &r);
    Fd fd(const Resource &r);                       // owned dup
    Pump pump(int timeout_ms, Event &out);          // Event owns its fd
    void run(const std::function<void(Event)> &on_event);
    const std::string &last_error() const;
private:
    sgc_client *c_;
};
```

No second state machine: `pump`/`run` are loops over `sgc_pump`, exactly
like the Rust `start_event_loop`. `Event` is move-only and closes its
`GRANTED` fd on destruction; `Fd` wraps `sgc_fd` results. C++ gets
ergonomics; C gets the plain ABI; Rust gets the native crate — all over the
same tested core.

## Test strategy

- Core unit tests (Rust, `libsgc-rs`): an interactive fake controller drives
  connect/acquire/pump — grant, deny, revoke-ack (Release observed),
  unsolicited regrant (Ack observed), disconnect draining one `Revoked` per
  held resource, bounded and zero-timeout pumps.
- Shim unit tests (`libsgc-c`): kind round-trips over every variant and u8
  edge, invalid encodings rejected, error-buffer truncation.
- C and C++ smoke tests (`libsgc-c/tests/smoke`): compiled against the
  staticlib with -Wall -Wextra; exercise every symbol and the error channel
  (NULL handles, output pointers untouched on error, connect failure carries
  the real Rust error text).
- Board tests: sgc-drm-client (Rust) and kmscube -L (C, over libsgc) run
  the full grant / ask-first revoke / requeue / re-grant / resume cycle
  against the real daemon — kmscube survives preemption and rebuilds its
  display stack on the re-granted lease fd.

## Source layout

```
libsgc-rs/            native crate (the core lives here)
  src/client.rs       SgcClient, pump(), SgcEvent
libsgc-c/             C ABI shim: #[unsafe(no_mangle)] extern "C" (lib name sgc)
  include/libsgc.h    C contract (handwritten)
  include/sgc.hpp     C++ RAII wrapper (header-only, over the C ABI)
  tests/smoke/        main.c + main.cc compile/link gates
```
