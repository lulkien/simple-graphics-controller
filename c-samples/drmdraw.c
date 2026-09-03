/*
 * drmdraw.c — dumb-buffer modeset via raw ioctls (see drmdraw.h).
 *
 * Structs and ioctl numbers come from the kernel DRM UAPI headers
 * (<drm/drm.h> + <drm/drm_mode.h>, shipped by linux-libc-dev) — nothing
 * is hand-copied. Only ioctl(2) is used: no libdrm functions are linked,
 * no GBM/EGL.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

#include "drmdraw.h"

#define FAIL(...) do { fprintf(stderr, "drmdraw: " __VA_ARGS__); return -1; } while (0)

/* ---- discovery ------------------------------------------------------- */

static int pick_mode(const struct drm_mode_modeinfo *modes, uint32_t count,
		     struct drm_mode_modeinfo *out)
{
	uint32_t i;

	for (i = 0; i < count; i++) {
		if (modes[i].type & DRM_MODE_TYPE_PREFERRED) {
			*out = modes[i];
			return 0;
		}
	}
	if (count == 0)
		return -1;
	*out = modes[0];
	return 0;
}

/* Find a usable output: a CONNECTED connector with modes is preferred
 * (first found wins); otherwise the first connector with modes. The crtc
 * comes from the connector's current encoder, else the card's first. */
static int find_output(int fd, uint32_t *crtc_id, uint32_t *connector_id,
		       struct drm_mode_modeinfo *mode)
{
	struct drm_mode_card_res res;
	struct drm_mode_get_encoder enc;
	uint32_t *crtc_ids = NULL;
	uint32_t *connector_ids = NULL;
	uint32_t cand_connector = 0, cand_encoder = 0;
	int have_candidate = 0, cand_connected = 0;
	struct drm_mode_modeinfo cand_mode;
	uint32_t i;
	int rc = -1;

	memset(&res, 0, sizeof(res));
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0)
		FAIL("GETRESOURCES: %s\n", strerror(errno));
	if (res.count_crtcs == 0 || res.count_connectors == 0)
		FAIL("card has no CRTCs or connectors\n");

	crtc_ids = calloc(res.count_crtcs, sizeof(uint32_t));
	connector_ids = calloc(res.count_connectors, sizeof(uint32_t));
	if (!crtc_ids || !connector_ids)
		FAIL("out of memory\n");

	/* Only ask for crtcs/connectors: zero the other counts so the
	 * kernel skips the (null) fb/encoder pointers. */
	res.count_fbs = 0;
	res.count_encoders = 0;
	res.crtc_id_ptr = (uint64_t)(uintptr_t)crtc_ids;
	res.connector_id_ptr = (uint64_t)(uintptr_t)connector_ids;
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0)
		FAIL("GETRESOURCES (fill): %s\n", strerror(errno));

	for (i = 0; i < res.count_connectors; i++) {
		struct drm_mode_get_connector conn;
		struct drm_mode_modeinfo *modes = NULL;
		uint32_t *encoders = NULL;

		memset(&conn, 0, sizeof(conn));
		conn.connector_id = connector_ids[i];
		if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) < 0)
			continue;
		if (conn.count_modes == 0)
			continue; /* e.g. writeback — no display modes */

		modes = calloc(conn.count_modes, sizeof(*modes));
		encoders = calloc(conn.count_encoders, sizeof(uint32_t));
		if (!modes || !encoders)
			goto out_free;
		conn.count_props = 0; /* skip property writes (null ptrs) */
		conn.modes_ptr = (uint64_t)(uintptr_t)modes;
		conn.encoders_ptr = (uint64_t)(uintptr_t)encoders;
		if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) < 0) {
			free(modes);
			free(encoders);
			continue;
		}

		if (pick_mode(modes, conn.count_modes, &cand_mode) == 0 &&
		    (!have_candidate || conn.connection == 1 /* DRM_MODE_CONNECTED */)) {
			cand_connector = connector_ids[i];
			cand_encoder = conn.encoder_id;
			cand_connected = conn.connection == 1 /* DRM_MODE_CONNECTED */;
			have_candidate = 1;
		}
		free(modes);
		free(encoders);
		if (cand_connected)
			break; /* connected preferred: first one wins */
	}

	if (!have_candidate) {
		fprintf(stderr, "drmdraw: no connector with modes\n");
		goto out_free;
	}

	/* Encoder's current crtc, else the card's first crtc. */
	memset(&enc, 0, sizeof(enc));
	enc.encoder_id = cand_encoder;
	if (ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &enc) < 0) {
		fprintf(stderr, "drmdraw: GETENCODER: %s\n", strerror(errno));
		goto out_free;
	}

	*crtc_id = enc.crtc_id != 0 ? enc.crtc_id : crtc_ids[0];
	*connector_id = cand_connector;
	memcpy(mode, &cand_mode, sizeof(*mode));
	rc = 0;

out_free:
	free(crtc_ids);
	free(connector_ids);
	return rc;
}

/* ---- framebuffer ----------------------------------------------------- */

static int create_framebuffer(int fd, const struct drm_mode_modeinfo *mode,
			      uint32_t *fb_id, void **map, size_t *map_len,
			      uint32_t *pitch)
{
	struct drm_mode_create_dumb dumb;
	struct drm_mode_map_dumb m;
	struct drm_mode_fb_cmd fb;
	void *ptr;

	memset(&dumb, 0, sizeof(dumb));
	dumb.width = mode->hdisplay;
	dumb.height = mode->vdisplay;
	dumb.bpp = 32;
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) < 0)
		FAIL("CREATE_DUMB: %s\n", strerror(errno));

	memset(&m, 0, sizeof(m));
	m.handle = dumb.handle;
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &m) < 0)
		FAIL("MAP_DUMB: %s\n", strerror(errno));

	ptr = mmap(NULL, dumb.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
		   m.offset);
	if (ptr == MAP_FAILED)
		FAIL("mmap: %s\n", strerror(errno));

	memset(&fb, 0, sizeof(fb));
	fb.width = dumb.width;
	fb.height = dumb.height;
	fb.pitch = dumb.pitch;
	fb.bpp = 32;
	fb.depth = 24; /* XRGB8888: depth 24, bpp 32 */
	fb.handle = dumb.handle;
	if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) < 0) { /* ADDFB */
		munmap(ptr, dumb.size);
		FAIL("ADDFB: %s\n", strerror(errno));
	}

	*fb_id = fb.fb_id;
	*map = ptr;
	*map_len = dumb.size;
	*pitch = dumb.pitch;
	return 0;
}

static int set_crtc(int fd, uint32_t crtc_id, uint32_t fb_id,
		    uint32_t connector_id, const struct drm_mode_modeinfo *mode)
{
	struct drm_mode_crtc crtc;
	uint64_t connector_ptr;

	memset(&crtc, 0, sizeof(crtc));
	crtc.crtc_id = crtc_id;
	crtc.fb_id = fb_id;
	crtc.count_connectors = 1;
	crtc.mode_valid = 1;
	crtc.mode = *mode;
	connector_ptr = (uint64_t)(uintptr_t)&connector_id;
	crtc.set_connectors_ptr = connector_ptr;
	if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &crtc) < 0)
		FAIL("SETCRTC: %s\n", strerror(errno));
	return 0;
}

/* ---- public API ------------------------------------------------------ */

int sgc_screen_open(sgc_screen *s, int fd)
{
	struct drm_mode_modeinfo mode;
	int ret;

	memset(s, 0, sizeof(*s));
	s->fd = fd;

	if (find_output(fd, &s->crtc_id, &s->connector_id, &mode) < 0)
		return -1;
	if (create_framebuffer(fd, &mode, &s->fb_id, &s->map, &s->map_len,
			       &s->pitch) < 0)
		return -1;
	s->width = mode.hdisplay;
	s->height = mode.vdisplay;

	/* Black first frame so the screen does not flash garbage. */
	memset(s->map, 0, s->map_len);
	ret = set_crtc(fd, s->crtc_id, s->fb_id, s->connector_id, &mode);
	if (ret < 0) {
		sgc_screen_close(s);
		return -1;
	}
	return 0;
}

void sgc_screen_close(sgc_screen *s)
{
	struct drm_mode_fb_cmd fb;

	if (!s->map)
		return;

	/* Remove the fb first (kernel stops scanning out of it). On a
	 * revoked lease the ioctl fails — harmless. */
	memset(&fb, 0, sizeof(fb));
	fb.fb_id = s->fb_id;
	ioctl(s->fd, DRM_IOCTL_MODE_RMFB, &fb);
	munmap(s->map, s->map_len);
	s->map = NULL;
}
