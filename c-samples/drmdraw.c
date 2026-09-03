/*
 * drmdraw.c — dumb-buffer modeset via raw ioctls (see drmdraw.h).
 *
 * ioctl numbers are built like the kernel's _IOC macro:
 * dir(30) | type(8) | nr(0) | size(16), type = 'd'. Structs are the
 * linux/drm_mode.h layouts as used by the Rust render task.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "drmdraw.h"

/* ---- ioctl plumbing -------------------------------------------------- */

static unsigned long drm_ioc(unsigned int dir, unsigned int nr, size_t size)
{
	return ((unsigned long)dir << 30) | ((unsigned long)'d' << 8) |
	       (unsigned long)nr | ((unsigned long)size << 16);
}

/* _IOWR: the kernel writes the struct back. */
static int drm_iowr(int fd, unsigned int nr, void *arg, size_t size)
{
	return ioctl(fd, drm_ioc(3, nr, size), arg);
}

/* _IOW: write-only. */
static int drm_iow(int fd, unsigned int nr, const void *arg, size_t size)
{
	return ioctl(fd, drm_ioc(1, nr, size), arg);
}

#define FAIL(...) do { fprintf(stderr, "drmdraw: " __VA_ARGS__); return -1; } while (0)

/* ---- kernel structs (linux/drm_mode.h, 64-bit layout) ---------------- */

struct mode_card_res {
	uint64_t fb_id_ptr;
	uint64_t crtc_id_ptr;
	uint64_t connector_id_ptr;
	uint64_t encoder_id_ptr;
	uint32_t count_fbs;
	uint32_t count_crtcs;
	uint32_t count_connectors;
	uint32_t count_encoders;
	uint32_t min_width;
	uint32_t max_width;
	uint32_t min_height;
	uint32_t max_height;
};

struct mode_mode_info {
	uint32_t clock;
	uint16_t hdisplay;
	uint16_t hsync_start;
	uint16_t hsync_end;
	uint16_t htotal;
	uint16_t hskew;
	uint16_t vdisplay;
	uint16_t vsync_start;
	uint16_t vsync_end;
	uint16_t vtotal;
	uint16_t vscan;
	uint32_t vrefresh;
	uint32_t flags;
	uint32_t type;
	char name[32];
};

struct mode_get_connector {
	uint64_t encoders_ptr;
	uint64_t modes_ptr;
	uint64_t props_ptr;
	uint64_t prop_values_ptr;
	uint32_t count_modes;
	uint32_t count_props;
	uint32_t count_encoders;
	uint32_t encoder_id;
	uint32_t connector_id;
	uint32_t connector_type;
	uint32_t connector_type_id;
	uint32_t connection;
	uint32_t mm_width;
	uint32_t mm_height;
	uint32_t subpixel;
	uint32_t pad;
};

struct mode_get_encoder {
	uint32_t encoder_id;
	uint32_t encoder_type;
	uint32_t crtc_id;
	uint32_t possible_crtcs;
	uint32_t possible_clones;
};

struct mode_create_dumb {
	uint32_t height;
	uint32_t width;
	uint32_t bpp;
	uint32_t flags;
	uint32_t handle;
	uint32_t pitch;
	uint64_t size;
};

struct mode_map_dumb {
	uint32_t handle;
	uint32_t pad;
	uint64_t offset;
};

struct mode_fb_cmd {
	uint32_t fb_id;
	uint32_t width;
	uint32_t height;
	uint32_t pitch;
	uint32_t bpp;
	uint32_t depth;
	uint32_t handle;
};

struct mode_crtc {
	uint64_t set_connectors_ptr;
	uint32_t count_connectors;
	uint32_t crtc_id;
	uint32_t fb_id;
	uint32_t x;
	uint32_t y;
	uint32_t gamma_size;
	uint32_t mode_valid;
	struct mode_mode_info mode;
};

/* ---- discovery ------------------------------------------------------- */

static int pick_mode(const struct mode_mode_info *modes, uint32_t count,
		     struct mode_mode_info *out)
{
	uint32_t i;

	for (i = 0; i < count; i++) {
		if (modes[i].type & 0x80) { /* DRM_MODE_TYPE_PREFERRED */
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
		       struct mode_mode_info *mode)
{
	struct mode_card_res res;
	struct mode_get_encoder enc;
	uint32_t *crtc_ids = NULL;
	uint32_t *connector_ids = NULL;
	uint32_t cand_connector = 0, cand_encoder = 0;
	int have_candidate = 0, cand_connected = 0;
	struct mode_mode_info cand_mode;
	uint32_t i;
	int rc = -1;

	memset(&res, 0, sizeof(res));
	if (drm_iowr(fd, 0xA0, &res, sizeof(res)) < 0) /* GETRESOURCES */
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
	if (drm_iowr(fd, 0xA0, &res, sizeof(res)) < 0)
		FAIL("GETRESOURCES (fill): %s\n", strerror(errno));

	for (i = 0; i < res.count_connectors; i++) {
		struct mode_get_connector conn;
		struct mode_mode_info *modes = NULL;
		uint32_t *encoders = NULL;

		memset(&conn, 0, sizeof(conn));
		conn.connector_id = connector_ids[i];
		if (drm_iowr(fd, 0xA7, &conn, sizeof(conn)) < 0) /* GETCONNECTOR */
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
		if (drm_iowr(fd, 0xA7, &conn, sizeof(conn)) < 0) {
			free(modes);
			free(encoders);
			continue;
		}

		if (pick_mode(modes, conn.count_modes, &cand_mode) == 0 &&
		    (!have_candidate || conn.connection == 1)) {
			cand_connector = connector_ids[i];
			cand_encoder = conn.encoder_id;
			cand_connected = conn.connection == 1;
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
	if (drm_iowr(fd, 0xA6, &enc, sizeof(enc)) < 0) { /* GETENCODER */
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

static int create_framebuffer(int fd, const struct mode_mode_info *mode,
			      uint32_t *fb_id, void **map, size_t *map_len,
			      uint32_t *pitch)
{
	struct mode_create_dumb dumb;
	struct mode_map_dumb m;
	struct mode_fb_cmd fb;
	void *ptr;

	memset(&dumb, 0, sizeof(dumb));
	dumb.width = mode->hdisplay;
	dumb.height = mode->vdisplay;
	dumb.bpp = 32;
	if (drm_iowr(fd, 0xB2, &dumb, sizeof(dumb)) < 0) /* CREATE_DUMB */
		FAIL("CREATE_DUMB: %s\n", strerror(errno));

	memset(&m, 0, sizeof(m));
	m.handle = dumb.handle;
	if (drm_iowr(fd, 0xB3, &m, sizeof(m)) < 0) /* MAP_DUMB */
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
	if (drm_iowr(fd, 0xAE, &fb, sizeof(fb)) < 0) { /* ADDFB */
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
		    uint32_t connector_id, const struct mode_mode_info *mode)
{
	struct mode_crtc crtc;
	uint64_t connector_ptr;

	memset(&crtc, 0, sizeof(crtc));
	crtc.crtc_id = crtc_id;
	crtc.fb_id = fb_id;
	crtc.count_connectors = 1;
	crtc.mode_valid = 1;
	crtc.mode = *mode;
	connector_ptr = (uint64_t)(uintptr_t)&connector_id;
	crtc.set_connectors_ptr = connector_ptr;
	if (drm_iowr(fd, 0xA2, &crtc, sizeof(crtc)) < 0) /* SETCRTC */
		FAIL("SETCRTC: %s\n", strerror(errno));
	return 0;
}

/* ---- public API ------------------------------------------------------ */

int sgc_screen_open(sgc_screen *s, int fd)
{
	struct mode_mode_info mode;
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
	struct mode_fb_cmd fb;

	if (!s->map)
		return;

	/* Remove the fb first (kernel stops scanning out of it). On a
	 * revoked lease the ioctl fails — harmless. */
	memset(&fb, 0, sizeof(fb));
	fb.fb_id = s->fb_id;
	drm_iow(s->fd, 0xAF, &fb, sizeof(fb)); /* RMFB */
	munmap(s->map, s->map_len);
	s->map = NULL;
}
