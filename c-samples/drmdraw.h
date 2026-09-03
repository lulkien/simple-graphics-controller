/*
 * drmdraw.h — minimal dumb-buffer modeset glue for the C/C++ sample
 * clients. Structs and ioctl numbers come from the kernel DRM UAPI
 * headers (<drm/drm.h>, <drm/drm_mode.h> — linux-libc-dev); the ioctls
 * are raw ioctl(2) calls, no libdrm/GBM/EGL is linked (mirrors the
 * ioctl pattern of the Rust sgc-drm-client render task).
 *
 * The fd is a DRM lease fd from the controller: the server holds DRM
 * master and grants each client a lease over the card's objects, so the
 * ioctls here work within the lease. The lease can be revoked at any
 * time — on a revoked fd the ioctls fail and are harmless.
 */
#ifndef SGC_DRMDRAW_H
#define SGC_DRMDRAW_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* One live dumb-buffer modeset on a lease fd. */
typedef struct {
	int fd; /* the caller's lease fd (not owned) */
	uint32_t crtc_id;
	uint32_t connector_id;
	uint32_t fb_id;
	uint32_t pitch; /* bytes per row */
	uint32_t width;
	uint32_t height;
	void *map; /* XRGB8888 dumb-buffer mapping */
	size_t map_len;
} sgc_screen;

/* Modeset the first usable output of the card: discovery (resources ->
 * connector -> encoder -> mode), CREATE_DUMB + MAP_DUMB + mmap, ADDFB, then
 * SETCRTC with a black first frame. Returns 0 on success, -1 with the
 * reason on stderr. The fd stays owned by the caller. */
int sgc_screen_open(sgc_screen *s, int fd);

/* RMFB + munmap. Safe on any state, including a revoked lease. The fd
 * stays open. */
void sgc_screen_close(sgc_screen *s);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* SGC_DRMDRAW_H */
