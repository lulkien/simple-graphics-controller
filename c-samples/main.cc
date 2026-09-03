// sgc-drm-cpp — a minimal C++ sample client for libsgc (sgc.hpp).
//
// The C++ counterpart of sgc-drm-c: same lifecycle over the RAII wrapper —
// connect, acquire the first advertised DRM card, draw an animated pattern
// on the lease fd, survive revoke/re-grant cycles. The pattern differs
// (checkerboard + moving cyan stripe) so the two samples are
// distinguishable on the display.
//
// Build (from the repo root's c-samples/):
//   meson setup build && meson compile -C build
//   ./build/sgc-drm-cpp
#include <unistd.h>

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <optional>

#include "drmdraw.h"
#include "sgc.hpp"

namespace {

constexpr int kFrameMs = 33;
constexpr const char *kApp = "sgc-drm-cpp";

// One frame: a checkerboard whose colors phase over time + a cyan stripe
// sweeping down.
void drawFrame(sgc_screen *s, uint64_t frame) {
  const uint32_t w = s->width, h = s->height;
  auto *buf = static_cast<uint32_t *>(s->map);
  const uint32_t rowWords = s->pitch / 4;
  const uint8_t t = static_cast<uint8_t>(frame >> 1);
  const uint32_t cell = 40;

  for (uint32_t y = 0; y < h; y++) {
    for (uint32_t x = 0; x < w; x++) {
      // Checkerboard: color shifts with time (phase per cell).
      const bool odd = ((x / cell) + (y / cell)) & 1;
      const uint8_t phase = static_cast<uint8_t>((x / cell + y / cell) * 32 + t * 3);
      uint8_t r, g, b;
      if (odd) {
        r = phase;
        g = static_cast<uint8_t>(255 - phase);
        b = static_cast<uint8_t>(phase >> 1);
      } else {
        r = static_cast<uint8_t>(255 - phase);
        g = static_cast<uint8_t>(phase >> 1);
        b = phase;
      }
      buf[y * rowWords + x] =
          (static_cast<uint32_t>(r) << 16) | (static_cast<uint32_t>(g) << 8) | b;
    }
  }

  // Cyan stripe sweeping top to bottom (triangle wave).
  const uint32_t band = h / 10;
  const uint32_t span = 2 * (h > band ? h - band : 1);
  const uint32_t p = static_cast<uint32_t>((frame * 10) % (span ? span : 1));
  const uint32_t y0 = p < h - band ? p : span - p;
  for (uint32_t y = y0; y < y0 + band && y < h; y++)
    for (uint32_t x = 0; x < w; x++) buf[y * rowWords + x] = 0x0000FFFF; /* cyan */
}

}  // namespace

int main() {
  std::string err;

  // Line-buffer stdout even when redirected (log files, nohup).
  std::setvbuf(stdout, nullptr, _IOLBF, 0);

  auto client = sgc::SgcClient::connect(&err);
  if (!client) {
    std::fprintf(stderr, "%s: connect to @sgc: %s\n", kApp, err.c_str());
    return 1;
  }

  // First advertised DRM card.
  sgc::Resource res;
  bool found = false;
  for (const auto &a : client->advertised()) {
    if (a.kind == SGC_RESOURCE_DRM) {
      res = a;
      found = true;
      break;
    }
  }
  if (!found) {
    std::fprintf(stderr, "%s: controller does not offer a DRM card\n", kApp);
    return 1;
  }
  std::printf("%s: connected; acquiring Drm { card: %d }\n", kApp, res.index);

  auto denied = client->acquire(res);
  if (denied) {
    std::fprintf(stderr, "%s: acquire failed: %s\n", kApp, denied->c_str());
    return 1;
  }

  sgc::Fd lease = client->fd(res);  // dup of the held lease fd (RAII)
  if (!lease.valid()) {
    std::fprintf(stderr, "%s: borrow lease fd failed: %s\n", kApp,
                 client->last_error().c_str());
    return 1;
  }
  std::printf("%s: drawing (phase-shifting checkerboard + cyan stripe)\n", kApp);

  uint64_t frame = 0;
  for (;;) {
    sgc_screen screen;
    if (sgc_screen_open(&screen, lease.get()) != 0)
      return 1;
    std::printf("%s: modeset %ux%u (connector %u, crtc %u)\n", kApp,
                screen.width, screen.height, screen.connector_id,
                screen.crtc_id);

    bool skipWait = false;
    while (true) {
      drawFrame(&screen, frame++);

      sgc::Event ev;
      const auto r = client->pump(0, ev);
      if (r == sgc::SgcClient::Pump::Event && ev.kind() == sgc::Event::Kind::Revoked) {
        std::printf("%s: revoked; waiting for re-grant\n", kApp);
        break;
      }
      if (r == sgc::SgcClient::Pump::Event && ev.granted()) {
        // Revoke + re-grant landed between frames: re-dup immediately.
        // ev closes its own fd on destruction.
        skipWait = true;
        break;
      }
      if (r == sgc::SgcClient::Pump::Error) {
        std::fprintf(stderr, "%s: connection lost: %s\n", kApp,
                     client->last_error().c_str());
        sgc_screen_close(&screen);
        return 1;
      }
      usleep(kFrameMs * 1000);
    }

    sgc_screen_close(&screen);
    lease = sgc::Fd();  // old lease fd is dead; drop it
    if (!skipWait) {
      // Block pumping until the controller re-grants to our queued session.
      // The granted event closes its own fd on destruction; the canonical
      // lives in the client, and client->fd() re-dups it below.
      sgc::Event ev;
      while (true) {
        const auto r = client->pump(-1, ev);
        if (r == sgc::SgcClient::Pump::Error) {
          std::fprintf(stderr, "%s: session over: %s\n", kApp,
                       client->last_error().c_str());
          return 1;
        }
        if (r == sgc::SgcClient::Pump::Event && ev.granted())
          break;
      }
    }
    lease = client->fd(res);  // fresh dup of the re-granted canonical
    if (!lease.valid()) {
      std::fprintf(stderr, "%s: session over: %s\n", kApp,
                   client->last_error().c_str());
      return 1;
    }
    std::printf("%s: re-granted; resuming\n", kApp);
  }
}
