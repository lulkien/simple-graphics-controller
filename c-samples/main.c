/*
 * sgc-drm-c — a minimal C sample client for libsgc (libsgc.h).
 *
 * Mirrors what the Rust sgc-drm-client demo does, over the C ABI:
 * connect to @sgc, take the first advertised DRM card, acquire it, and
 * draw a simple animated pattern on the granted lease fd (dumb buffer +
 * SETCRTC, see drmdraw.c — no GBM/EGL needed).
 *
 * The lease is revocable: when another client takes the card the
 * controller asks us to hand it over, and re-grants it when that client
 * leaves (requeue lifecycle). This sample survives the cycle — it stops
 * drawing, waits for the re-grant, and re-modesets on the fresh fd.
 *
 * Build (from the repo root's c-samples/):
 *   meson setup build && meson compile -C build
 *   ./build/sgc-drm-c
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "drmdraw.h"
#include "libsgc.h"

#define FRAME_MS 33
#define APP "sgc-drm-c"

/* Find the first advertised DRM card (index from -D-style selection is
 * left to the caller; samples take the first card, like the Rust demo). */
static int pick_drm(sgc_client *c, sgc_resource *out)
{
	sgc_resource *adv = NULL;
	size_t count = 0;
	size_t i;

	if (sgc_advertised(c, &adv, &count) != 0)
		return -1;
	for (i = 0; i < count; i++) {
		if (adv[i].kind == SGC_RESOURCE_DRM) {
			*out = adv[i];
			sgc_free(adv);
			return 0;
		}
	}
	sgc_free(adv);
	return -1;
}

/* One frame: a hue-cycling gradient + a bouncing orange square. */
static void draw_frame(sgc_screen *s, uint64_t frame)
{
	uint32_t w = s->width, h = s->height;
	uint32_t *buf = s->map;
	uint32_t row_words = s->pitch / 4;
	uint32_t y, x;
	uint8_t t = (uint8_t)(frame >> 1);

	for (y = 0; y < h; y++) {
		for (x = 0; x < w; x++) {
			uint8_t r = (uint8_t)(x * 255 / (w ? w - 1 : 1) + t);
			uint8_t g = (uint8_t)(y * 255 / (h ? h - 1 : 1) + t);
			uint8_t b = (uint8_t)(255 - (x * 255 / (w ? w - 1 : 1)) + t);
			buf[y * row_words + x] =
				((uint32_t)r << 16) | ((uint32_t)g << 8) | b;
		}
	}

	/* Bouncing orange square (triangle wave across the width). */
	{
		uint32_t sq = w / 6;
		if (sq > 96)
			sq = 96;
		uint32_t span = 2 * (w > sq ? w - sq : 1);
		uint32_t p = (uint32_t)((frame * 12) % (span ? span : 1));
		uint32_t x0 = p < w - sq ? p : span - p;
		uint32_t y0 = (h - sq) / 2;
		for (y = y0; y < y0 + sq && y < h; y++)
			for (x = x0; x < x0 + sq && x < w; x++)
				buf[y * row_words + x] = 0x00FFA500; /* orange */
	}
}

/* Pump until the controller re-grants the card to our queued session.
 * Returns a fresh dup of the lease fd (owned by the caller) or -1. */
static int wait_regrant(sgc_client *c, sgc_resource res,
			char *err, size_t err_len)
{
	sgc_event ev;

	for (;;) {
		int r = sgc_pump(c, -1, &ev, err, err_len);
		if (r == 1 && ev.kind == SGC_EVENT_GRANTED) {
			if (ev.fd >= 0)
				close(ev.fd);
			printf(APP ": re-granted; resuming\n");
			return sgc_fd(c, res, err, err_len);
		}
		if (r == 1) /* defensive: another REVOKED, keep waiting */
			continue;
		return -1;
	}
}

int main(void)
{
	char err[128];
	sgc_client *c;
	sgc_resource res;
	int fd;
	uint64_t frame = 0;

	/* Line-buffer stdout even when redirected (log files, nohup). */
	setvbuf(stdout, NULL, _IOLBF, 0);

	c = sgc_connect(err, sizeof(err));
	if (!c) {
		fprintf(stderr, APP ": connect to @sgc: %s\n", err);
		return 1;
	}
	if (pick_drm(c, &res) != 0) {
		fprintf(stderr, APP ": controller does not offer a DRM card\n");
		sgc_release(c);
		return 1;
	}
	printf(APP ": connected; acquiring Drm { card: %d }\n", res.index);

	if (sgc_acquire(c, res, err, sizeof(err)) != 0) {
		fprintf(stderr, APP ": acquire failed: %s\n", err);
		sgc_release(c);
		return 1;
	}
	fd = sgc_fd(c, res, err, sizeof(err));
	if (fd < 0) {
		fprintf(stderr, APP ": borrow lease fd failed: %s\n", err);
		sgc_release(c);
		return 1;
	}
	printf(APP ": drawing (cycling gradient + bouncing orange square)\n");

	for (;;) {
		sgc_screen screen;
		int running = 1;
		int skip_wait = 0;

		if (sgc_screen_open(&screen, fd) != 0) {
			close(fd);
			sgc_release(c);
			return 1;
		}
		printf(APP ": modeset %ux%u (connector %u, crtc %u)\n",
		       screen.width, screen.height, screen.connector_id,
		       screen.crtc_id);

		while (running) {
			sgc_event ev;
			int r;

			draw_frame(&screen, frame++);
			r = sgc_pump(c, 0, &ev, err, sizeof(err));
			if (r == 1 && ev.kind == SGC_EVENT_REVOKED) {
				printf(APP ": revoked; waiting for re-grant\n");
				running = 0;
			} else if (r == 1) {
				/* GRANTED without a revoke seen (revoke +
				 * re-grant landed between frames): stop and
				 * re-dup immediately, do not wait. */
				if (ev.fd >= 0)
					close(ev.fd);
				running = 0;
				skip_wait = 1;
			} else if (r == -1) {
				fprintf(stderr, APP ": connection lost: %s\n", err);
				sgc_screen_close(&screen);
				close(fd);
				sgc_release(c);
				return 1;
			}
			usleep(FRAME_MS * 1000);
		}

		sgc_screen_close(&screen);
		close(fd); /* old lease is dead */
		if (skip_wait)
			fd = sgc_fd(c, res, err, sizeof(err));
		else
			fd = wait_regrant(c, res, err, sizeof(err));
		if (fd < 0) {
			fprintf(stderr, APP ": session over: %s\n", err);
			sgc_release(c);
			return 1;
		}
		printf(APP ": resuming on the re-granted lease\n");
	}
}
