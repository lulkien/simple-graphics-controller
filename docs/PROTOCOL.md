# Protocol specification

Wire protocol between `simple-graphics-controller` (server) and its clients.
This document is the reference for implementing clients in other languages
(e.g. the planned C library for LVGL integration).

- Transport: Unix stream socket, **abstract namespace** address `@sgc`
  (`sun_path[0] = '\0'`, then `"sgc"`; addrlen = `offsetof(sockaddr_un, sun_path) + 1 + 3`).
- Encoding: MessagePack, as produced by `rmp_serde` with `write_named` —
  i.e. enums are string-tagged maps, fields are named maps, resources are
  nested string-tagged maps. Self-describing; no integer variant tags.
- Framing: **length-prefixed**. Every message is prefixed with a 4-byte
  big-endian unsigned length header giving the size of the MessagePack payload
  (max 1 MiB). Readers read 4 bytes, then exactly N bytes. This removes the
  need to guess message boundaries on a stream socket. (Before framing, each
  message was a bare MessagePack value.)
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
owner; the owner stops using the resource and replies `Release` — that
`Release` doubles as the revoke acknowledgment, and the server then grants
the resource to the next waiter (the preempted owner is requeued and gets
one more turn). If the owner neither releases nor disconnects within
**5 seconds**, the server force-reclaims the resource and grants the next
waiter anyway.

After sending `Grant`, the server waits up to **5 seconds** for the client to
reply with `Ack` (sent only after the client successfully received the grant
and its fd). If no `Ack` arrives in time, the server logs a warning that the
grant is unconfirmed. The resource stays owned by the client either way;
`Ack` is a delivery signal, not a lease — it never gates the queue.

## Messages

Enums serialize as a one-entry map `{variant: payload}`; struct payloads are
one-entry maps `{field: value}`.

### Resource

Two-level enum: a resource is either a **display** or an **input** device.
The server registers one resource per discovered device.

| kind    | value                | wire (decoded)                  |
| ------- | -------------------- | ------------------------------- |
| Display | `Fbdev`              | `{"Display": "Fbdev"}`          |
| Display | `Drm { card: u8 }`   | `{"Display": {"Drm": 0}}`       |
| Input   | `Mouse(u8)`          | `{"Input": {"Mouse": 0}}`       |
| Input   | `Keyboard(u8)`       | `{"Input": {"Keyboard": 0}}`    |
| Input   | `Touch(u8)`          | `{"Input": {"Touch": 0}}`       |

The index counts devices of the same class (`Mouse(0)`, `Mouse(1)`, ...) so
the registry can hold several at once. Input devices come from
`/dev/input/event*`, classified by capabilities (touch > mouse > keyboard).

**DRM**: the server registers every card that can present a display — has
display connectors (writeback connectors are capture-only and don't count) —
one `Drm { card }` per physical `/dev/dri/cardN`. Render nodes
(`renderDNN`) are never registered. `Advertise` lists the cards in priority
order: a card with a connected connector first, then the lowest index — so
the first `Drm` entry is the best display card. A client that wants "a
screen" takes the first `Drm` in the list; one that wants a specific card
names it.

`{"Display": "Fbdev"}` wire hex:
`81 a7 44 69 73 70 6c 61 79 a5 46 62 64 65 76`.

### ClientRequest (client → server)

| variant  | fields                 | example wire (hex) |
| -------- | ---------------------- | ------------------ |
| Acquire  | `resource: Resource`   | `81 a7 41 63 71 75 69 72 65 81 a8 72 65 73 6f 75 72 63 65 81 a7 44 69 73 70 6c 61 79 a5 46 62 64 65 76` |
| Release  | `resource: Resource`   | `81 a7 52 65 6c 65 61 73 65 81 a8 72 65 73 6f 75 72 63 65 81 a7 44 69 73 70 6c 61 79 a5 46 62 64 65 76` |
| Ack      | —                      | `a3 41 63 6b`      |

`Acquire {resource: Display(Fbdev)}` decoded:
`{"Acquire": {"resource": {"Display": "Fbdev"}}}`.

### ServerMessage (server → client)

| variant    | fields                          | fds |
| ---------- | ------------------------------- | --- |
| Advertise  | `available_resources: [Resource]` | no |
| Grant      | `resource: Resource`            | yes, exactly one |
| Deny       | `reason: String`                | no |
| Revoke     | `resource: Resource`            | no |

Example `Grant {resource: Display(Fbdev)}`:
`81 a5 47 72 61 6e 74 81 a8 72 65 73 6f 75 72 63 65 81 a7 44 69 73 70 6c 61 79 a5 46 62 64 65 76`
decoded: `{"Grant": {"resource": {"Display": "Fbdev"}}}`.

Example `Deny {reason: "owned"}`:
`81 a4 44 65 6e 79 81 a6 72 65 61 73 6f 6e a5 6f 77 6e 65 64`.

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
- **Strings**: "Display", "Input", "Fbdev", variant names, field names are
  fixed for now, but parse them as arbitrary-length MessagePack strings
  (fixstr/str8/str16/str32) to stay compatible. Same for arrays/maps
  (fix/16/32 forms).
- **Verification tool**: `cargo run -p simple-graphics-protocol --example
  wire_dump` prints the exact bytes for every message.
