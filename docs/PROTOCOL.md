# Protocol specification

Wire protocol between `simple-graphics-controller` (server) and its clients.
This document is the reference for implementing clients in other languages
(e.g. the C client in kmscube's `drm-lease-client.c`).

- Transport: Unix stream socket, **abstract namespace** address `@sgc`
  (`sun_path[0] = '\0'`, then `"sgc"`; addrlen = `offsetof(sockaddr_un, sun_path) + 1 + 3`).
- Encoding: MessagePack, as produced by `rmp_serde` with `write_named` —
  enums are one-entry maps `{variant: payload}`, payloads are named-field
  maps, and **unit variants are bare strings** (`"Ack"`, `"Fbdev"`). The
  encoding is self-describing; there are no integer variant tags.
- Framing: **length-prefixed**. Every message is prefixed with a 4-byte
  big-endian unsigned length header giving the size of the MessagePack payload
  (max 1 MiB). Readers read 4 bytes, then exactly N bytes. This removes the
  need to guess message boundaries on a stream socket.
- File descriptors: passed out-of-band with `SCM_RIGHTS`. A `Grant` carries
  **exactly one fd** — for its single granted resource. Only `Grant` carries
  fds.

## Message flow

```
client                          server
  |          connect(@sgc)         |
  |------------------------------->|
  |                                |
  |            Advertise           |   {available_resources}
  |<-------------------------------|
  |                                |
  |         Acquire {resource}     |
  |------------------------------->|
  |                                |
  |              Grant             |   {resource}  + fd (SCM_RIGHTS)
  |<-------------------------------|
  |                                |
  |               Ack              |   (client confirms receipt of Grant+fd)
  |------------------------------->|
  |                                |
  |            (or Deny {reason})   |
  |                                |
  |        Release {resource}      |
  |------------------------------->|
  |                                |
  |              Revoke            |   (server-initiated preemption)
  |<-------------------------------|
  |                                |
  |        Release {resource}      |   (the revoke acknowledgment)
  |------------------------------->|
  |                                |
  |              Grant             |   to the next waiter
  |<-------------------------------|
```

On connect the server immediately sends `Advertise` (no hello handshake).

Every request/message names **exactly one resource** — one resource per
`Acquire`, per `Grant`, per `Revoke`, per `Release`. Clients that want several
resources (e.g. a display and a mouse) acquire them one at a time.

`Acquire` is arbitrated by the server's windowing policy (per-resource,
selected by the `SGC_POLICY` server env: `first-owner` | `latest-owner` |
`fair-queue`, default `fair-queue`):

- resource **free** -> `Grant` immediately;
- resource **owned by another client** -> `Deny` (first-owner) or the
  request is **queued** and the owner is preempted (latest-owner /
  fair-queue);
- resource **already owned by the requester** -> `Deny`.

A **queued** `Acquire` gets NO immediate reply — the client simply waits;
the `Grant` arrives when the resource frees. Note that a `Grant` can arrive
with no preceding `Acquire`: it means the client was re-granted after being
preempted. Clients must keep reading after `Release`.

Preemption handoff: the server sends `Revoke {resource}` to the current
owner as an ASK — the owner keeps a valid fd (and for DRM a valid lease)
through the grace window (`REVOKE_TIMEOUT`, 5s) so it can finish its frame.
The owner stops using the resource and replies `Release` — that `Release`
doubles as the revoke acknowledgment, and the server then grants the
resource to the next waiter (the preempted owner is requeued and gets one
more turn). If the owner neither releases nor disconnects within 5 seconds,
the server force-reclaims the resource and grants the next waiter anyway;
for DRM that force is kernel-enforced (the lease is revoked even though the
client keeps its fd open), for fbdev/input it is cooperative.

After sending `Grant`, the server waits up to **5 seconds** for the client to
reply with `Ack` (sent only after the client successfully received the grant
and its fd). If no `Ack` arrives in time, the server logs a warning that the
grant is unconfirmed. The resource stays owned by the client either way;
`Ack` is a delivery signal, not a lease — it never gates the queue.

## Messages

Enums serialize as a one-entry map `{variant: payload}`; struct payloads are
one-entry maps `{field: value}`; unit variants are bare strings.

### Resource

`Resource` is a flat enum with one variant per resource kind. The server
registers one resource per discovered device.

| variant               | fields        | wire (decoded)             |
| --------------------- | ------------- | -------------------------- |
| `Fbdev`               | —             | `"Fbdev"`                  |
| `Drm`                 | `card: u8`    | `{"Drm": {"card": 0}}`     |
| `Input(InputResource)`| —             | `{"Input": {"Mouse": 0}}`  |

`InputResource` is `Mouse(u8) | Keyboard(u8) | Touch(u8)` — the index counts
devices of the same class so the registry can hold several at once. Input
devices come from `/dev/input/event*`, classified by capabilities (touch >
mouse > keyboard).

**Fbdev**: `/dev/fb0`, registered only when the server is built with the
`fbdev` feature (off by default).

**DRM**: every `/dev/dri/cardN` that can present a display — has display
connectors (writeback connectors are capture-only and don't count) — becomes
one `{"Drm": {"card": N}}`, registered only when built with the `drm`
feature (default). Render nodes (`renderDNN`) are never registered.
`Advertise` lists the cards in priority order: a card with a connected
connector first, then the lowest index — so the first `Drm` entry is the best
display card. A client that wants "a screen" takes the first `Drm` in the
list; one that wants a specific card names it. The granted fd is a DRM
lease, never the master fd.

Resource wire encodings (raw payload bytes):

| resource                      | wire hex                                      |
| ----------------------------- | --------------------------------------------- |
| `Fbdev`                       | `a5 46 62 64 65 76`                           |
| `Drm { card: 0 }`             | `81 a3 44 72 6d 81 a4 63 61 72 64 00`         |
| `Input(Mouse(1))`             | `81 a5 49 6e 70 75 74 81 a5 4d 6f 75 73 65 01` |

### ClientRequest (client → server)

| variant  | fields                 | wire (decoded) |
| -------- | ---------------------- | -------------- |
| Acquire  | `resource: Resource`   | `{"Acquire": {"resource": ...}}` |
| Release  | `resource: Resource`   | `{"Release": {"resource": ...}}` |
| Ack      | —                      | `"Ack"`        |

Raw payload bytes:

| request                     | wire hex                                      |
| --------------------------- | --------------------------------------------- |
| `Acquire {resource: Fbdev}` | `81 a7 41 63 71 75 69 72 65 81 a8 72 65 73 6f 75 72 63 65 a5 46 62 64 65 76` |
| `Acquire {resource: Drm{0}}`| `81 a7 41 63 71 75 69 72 65 81 a8 72 65 73 6f 75 72 63 65 81 a3 44 72 6d 81 a4 63 61 72 64 00` |
| `Release {resource: Drm{0}}`| `81 a7 52 65 6c 65 61 73 65 81 a8 72 65 73 6f 75 72 63 65 81 a3 44 72 6d 81 a4 63 61 72 64 00` |
| `Ack`                       | `a3 41 63 6b`              |

`Acquire {resource: Drm{0}}` decoded: `{"Acquire": {"resource": {"Drm": {"card": 0}}}}`.

### ServerMessage (server → client)

| variant    | fields                          | fds |
| ---------- | ------------------------------- | --- |
| Advertise  | `available_resources: [Resource]` | no |
| Grant      | `resource: Resource`            | yes, exactly one |
| Deny       | `reason: String`                | no |
| Revoke     | `resource: Resource`            | no |

Raw payload bytes:

| message                             | wire hex |
| ----------------------------------- | -------- |
| `Advertise {available_resources: [Drm{0}, Fbdev, Input(Mouse(0))]}` | `81 a9 41 64 76 65 72 74 69 73 65 81 b3 61 76 61 69 6c 61 62 6c 65 5f 72 65 73 6f 75 72 63 65 73 93 81 a3 44 72 6d 81 a4 63 61 72 64 00 a5 46 62 64 65 76 81 a5 49 6e 70 75 74 81 a5 4d 6f 75 73 65 00` |
| `Grant {resource: Drm{0}}`          | `81 a5 47 72 61 6e 74 81 a8 72 65 73 6f 75 72 63 65 81 a3 44 72 6d 81 a4 63 61 72 64 00` |
| `Revoke {resource: Drm{0}}`         | `81 a6 52 65 76 6f 6b 65 81 a8 72 65 73 6f 75 72 63 65 81 a3 44 72 6d 81 a4 63 61 72 64 00` |
| `Deny {reason: "owned"}`            | `81 a4 44 65 6e 79 81 a6 72 65 61 73 6f 6e a5 6f 77 6e 65 64` |

`Grant {resource: Drm{0}}` decoded: `{"Grant": {"resource": {"Drm": {"card": 0}}}}`.

## Notes for a C implementation

- **Socket**: `socket(AF_UNIX, SOCK_STREAM, 0)`; set `sun_family`, `sun_path[0]=0`,
  `memcpy(sun_path+1, "sgc", 3)`; `connect()` with the full addrlen. Abstract
  sockets need no filesystem path and vanish when the server dies.
- **Read loop**: read 4 bytes (big-endian u32 length), validate it is within
  the 1 MiB limit, then read exactly that many payload bytes. Do not assume a
  single `read()` returns a whole message — loop until the exact byte count is
  reached.
- **FD passing**: use `recvmsg()` for **all** reads — the fd is attached to
  the first bytes of a Grant frame and a plain `read()` would silently
  discard it. `CMSG_SPACE(sizeof(int))` is enough. Only `Grant` messages
  carry an fd, and it is exactly one, for the granted resource. Ownership
  transfers to the client — close the fd when done or on `Release`.
- **Strings**: variant names ("Advertise", "Acquire", "Grant", "Deny",
  "Revoke", "Release", "Ack"), field names ("available_resources",
  "resource", "reason", "card"), resource kinds ("Fbdev", "Drm", "Input",
  "Mouse", "Keyboard", "Touch") are fixed for now, but parse them as
  arbitrary-length MessagePack strings (fixstr/str8/str16/str32) to stay
  compatible. Same for arrays/maps (fix/16/32 forms). Card indexes and the
  input class indexes are small non-negative integers (positive fixint, or
  uint8/16 for large values).
- **Unit vs map variants**: a resource that carries no data is a bare
  string (`"Fbdev"`); one with data is `{"Kind": {"field": value}}` (`{"Drm":
  {"card": 0}}`, `{"Input": {"Mouse": 0}}`). Clients that only want a Drm
  card must skip entries of other shapes when scanning the advertise list.
- **Revoke while drawing**: `Revoke` can arrive at any time, not just as a
  reply. A client that renders in a loop must watch the control socket
  concurrently (e.g. a reader thread) and answer `Release` within the grace
  window; otherwise the server force-reclaims after 5s and the client's next
  modeset ioctl fails on the invalidated fd.
- **Verification tool**: `cargo run -p simple-graphics-protocol --example
  wire_dump` prints the exact bytes for every message.
