# Protocol specification

Wire protocol between `simple-graphics-controller` (server) and its clients.
This document is the reference for implementing clients in other languages
(e.g. the planned C library for LVGL integration).

- Transport: Unix stream socket, **abstract namespace** address `@sgc`
  (`sun_path[0] = '\0'`, then `"sgc"`; addrlen = `offsetof(sockaddr_un, sun_path) + 1 + 3`).
- Encoding: MessagePack, as produced by `rmp_serde` with `write_named` —
  i.e. enums are string-tagged maps, fields are named maps, resources are
  plain strings. Self-describing; no integer variant tags.
- Framing: **length-prefixed**. Every message is prefixed with a 4-byte
  big-endian unsigned length header giving the size of the MessagePack payload
  (max 1 MiB). Readers read 4 bytes, then exactly N bytes. This removes the
  need to guess message boundaries on a stream socket. (Before framing, each
  message was a bare MessagePack value.)
- File descriptors: passed out-of-band with `SCM_RIGHTS` (one `int` per
  granted resource, same order as the `resources` field in `Grant`).
  Only `Grant` carries fds.

## Message flow

```
client                          server
  |          connect(@sgc)         |
  |------------------------------->|
  |                                |
  |            Advertise           |   {available_resources}
  |<-------------------------------|
  |                                |
  |      Acquire {resources}       |
  |------------------------------->|
  |                                |
  |              Grant             |   {resources}  + fds (SCM_RIGHTS)
  |<-------------------------------|
  |                                |
  |               Ack              |   (client confirms receipt of Grant+fds)
  |------------------------------->|
  |                                |
  |            (or Deny {reason})   |
  |                                |
  |       Release {resources}      |
  |------------------------------->|
  |                                |
  |             Revoke             |   (not currently sent by the server)
  |<-------------------------------|
```

On connect the server immediately sends `Advertise` (no hello handshake).
`Acquire` is **atomic**: if any requested resource is not free — owned by
another client, or already owned by the requesting client — the server denies
the *entire* request with `Deny {reason}` and grants nothing.

After sending `Grant`, the server waits up to **5 seconds** for the client to
reply with `Ack` (sent only after the client successfully received the grant
and its fds). If no `Ack` arrives in time — or the client sends something else
first — the server logs a warning that the grant is unconfirmed. The resource
stays owned by the client either way; `Ack` is a delivery signal, not a lease.

## Messages

Enums serialize as a one-entry map `{variant: payload}`; struct payloads are
one-entry maps `{field: value}`.

### Resource

| value   | wire                                |
| ------- | ----------------------------------- |
| Fbdev   | `a5 46 62 64 65 76`  ("Fbdev")      |

### ClientRequest (client → server)

| variant  | fields                       | example wire (hex) |
| -------- | ---------------------------- | ------------------ |
| Acquire  | `resources: [Resource]`      | `81 a7 41 63 71 75 69 72 65 81 a9 72 65 73 6f 75 72 63 65 73 91 a5 46 62 64 65 76` |
| Release  | `resources: [Resource]`      | `81 a7 52 65 6c 65 61 73 65 81 a9 72 65 73 6f 75 72 63 65 73 91 a5 46 62 64 65 76` |
| Ack      | —                            | `a3 41 63 6b`      |

`Acquire {resources: [Fbdev]}` decoded: `{"Acquire": {"resources": ["Fbdev"]}}`.

### ServerMessage (server → client)

| variant    | fields                        | fds |
| ---------- | ----------------------------- | --- |
| Advertise  | `available_resources: [Resource]` | no |
| Grant      | `resources: [Resource]`       | yes, one per resource, same order |
| Deny       | `reason: String`              | no |
| Revoke     | `resources: [Resource]`       | no |

Example `Grant {resources: [Fbdev]}`:
`81 a5 47 72 61 6e 74 81 a9 72 65 73 6f 75 72 63 65 73 91 a5 46 62 64 65 76`
decoded: `{"Grant": {"resources": ["Fbdev"]}}`.

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
- **FD passing**: use `recvmsg()` for **all** reads — fds are attached to the
  first bytes of a Grant frame and a plain `read()` would silently discard
  them. `CMSG_SPACE(sizeof(int))` is enough for one fd, scale for multiple.
  Only `Grant` messages carry fds; a `Grant` with N resources carries exactly
  N fds, `fd[i]` belongs to `resources[i]`. Ownership transfers to the client —
  close the fds when done or on `Release`.
- **Strings**: "Fbdev", variant names, field names are fixed for now, but parse
  them as arbitrary-length MessagePack strings (fixstr/str8/str16/str32) to stay
  compatible. Same for arrays/maps (fix/16/32 forms).
- **Verification tool**: `cargo run -p simple-graphics-protocol --example
  wire_dump` prints the exact bytes for every message.
