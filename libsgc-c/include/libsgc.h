/*
 * libsgc.h — C ABI for the simple-graphics-controller client library.
 *
 * One Rust core (libsgc-rs) behind a C ABI; this header is the contract.
 * C++ consumers should use sgc.hpp (RAII wrapper over this header).
 *
 * Ownership rules:
 *   - the client handle is opaque: created by sgc_connect(), consumed by
 *     sgc_release(). One handle = one thread; drive it from a single
 *     thread (the client is single-threaded by design).
 *   - the fd in a GRANTED sgc_event and the fd returned by sgc_fd() are
 *     owned by the caller — close() them.
 *   - the array from sgc_advertised() is malloc'd — free it with
 *     sgc_free().
 *
 * Error convention: fallible functions return 0 on success / -1 (or NULL)
 * on failure and, when `err` is non-NULL, write a NUL-terminated message
 * of up to `err_len` bytes. Pass NULL/0 to ignore error text. No libsgc
 * function ever panics across the boundary.
 */
#ifndef LIBSGC_H
#define LIBSGC_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Resource kinds (sgc_resource.kind). Input classes are distinct kinds so
 * kind+index round-trips the full resource taxonomy. */
#define SGC_RESOURCE_FBDEV    0
#define SGC_RESOURCE_DRM      1
#define SGC_RESOURCE_MOUSE    2
#define SGC_RESOURCE_KEYBOARD 3
#define SGC_RESOURCE_TOUCH    4

/* Event kinds (sgc_event.kind). */
#define SGC_EVENT_REVOKED 0
#define SGC_EVENT_GRANTED 1

typedef struct sgc_client sgc_client; /* opaque */

typedef struct {
    int kind;  /* SGC_RESOURCE_* */
    int index; /* DRM: card index; MOUSE/KEYBOARD/TOUCH: device index;
                  FBDEV: ignored */
} sgc_resource;

typedef struct {
    int kind;         /* SGC_EVENT_* */
    sgc_resource resource;
    int fd;           /* GRANTED only: owned by the caller, close() it;
                         -1 otherwise */
} sgc_event;

/* Connect to the controller's abstract socket @sgc. Returns an opaque
 * handle, or NULL with an error message on failure. */
sgc_client *sgc_connect(char *err, size_t err_len);

/* Copy the controller's advertised resources into a freshly malloc'd array
 * (*out is NULL when there are none). The caller owns *out and must pass
 * it to sgc_free(). Returns 0 on success, -1 on error. */
int sgc_advertised(sgc_client *c, sgc_resource **out, size_t *count);

/* Free a pointer returned by sgc_advertised(). */
void sgc_free(void *p);

/* Request `resource` and BLOCK until the server answers (grant or deny).
 * On grant the client holds the fd internally; borrow it with sgc_fd().
 * Returns 0 on success, -1 on error (denied, not registered, ...). */
int sgc_acquire(sgc_client *c, sgc_resource r, char *err, size_t err_len);

/* Drive the protocol: wait up to timeout_ms for one event and store it in
 * *out. timeout_ms: -1 = block until an event or connection error;
 * 0 = poll once; > 0 = wait that many milliseconds.
 *
 * Returns:
 *   1  an event was stored in *out (GRANTED: close() the fd; REVOKED:
 *      stop drawing and close() any dup you hold)
 *   0  nothing happened (timeout) — call again
 *  -1  connection error — the session is over; every resource you held
 *      was already reported as REVOKED (one event per sgc_pump call)
 */
int sgc_pump(sgc_client *c, int timeout_ms, sgc_event *out,
             char *err, size_t err_len);

/* Borrow `resource`: returns a dup of the held fd, owned by the caller
 * (close() it). Returns -1 with an error message when not held. */
int sgc_fd(sgc_client *c, sgc_resource r, char *err, size_t err_len);

/* Tear down the session and free the handle. NULL is a no-op. The handle
 * must not be used afterwards. */
void sgc_release(sgc_client *c);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LIBSGC_H */
