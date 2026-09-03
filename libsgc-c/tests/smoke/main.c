/*
 * C smoke test for libsgc.h: exercises every symbol and the error
 * channel. Run without a controller on @sgc, so the connect path must
 * fail cleanly with a message; every other call is NULL/robustness
 * checking against the ABI contract.
 *
 * Build:
 *   gcc -Wall -Wextra -I libsgc-c/include libsgc-c/tests/smoke/main.c \
 *       target/debug/libsgc.a -lpthread -ldl -lm -o /tmp/sgc-c-smoke
 * (link flags after the .a; add -lgcc_s if your toolchain needs it)
 */
#include <stdio.h>
#include <string.h>

#include "libsgc.h"

static int failures = 0;

#define CHECK(cond, name)                                                     \
    do {                                                                      \
        if (cond) {                                                           \
            printf("PASS %s\n", name);                                        \
        } else {                                                              \
            printf("FAIL %s\n", name);                                        \
            failures++;                                                       \
        }                                                                     \
    } while (0)

int main(void) {
    /* No controller on @sgc locally: connect must fail with a message. */
    char err[128];
    sgc_client *c = sgc_connect(err, sizeof(err));
    CHECK(c == NULL, "sgc_connect fails without a controller");
    CHECK(strlen(err) > 0, "sgc_connect writes an error message");
    printf("  connect error: %s\n", err);

    /* NULL-handle robustness across the whole surface. */
    sgc_resource *advertised = (sgc_resource *)0x1; /* must be overwritten */
    size_t count = 999;
    CHECK(sgc_advertised(NULL, &advertised, &count) == -1, "sgc_advertised(NULL) errors");
    CHECK(advertised == (sgc_resource *)0x1 && count == 999,
          "sgc_advertised(NULL) leaves outputs untouched");

    sgc_event ev;
    memset(&ev, 0, sizeof(ev));
    CHECK(sgc_pump(NULL, 0, &ev, err, sizeof(err)) == -1, "sgc_pump(NULL) errors");
    CHECK(strlen(err) > 0, "sgc_pump(NULL) writes an error message");

    sgc_resource drm = {SGC_RESOURCE_DRM, 0};
    CHECK(sgc_acquire(NULL, drm, err, sizeof(err)) == -1, "sgc_acquire(NULL) errors");
    CHECK(sgc_fd(NULL, drm, err, sizeof(err)) == -1, "sgc_fd(NULL) errors");

    CHECK(sgc_pump(NULL, 0, NULL, err, sizeof(err)) == -1, "sgc_pump NULL event errors");
    sgc_release(NULL); /* must be a no-op */
    CHECK(1, "sgc_release(NULL) is a no-op");

    sgc_free(NULL); /* malloc/free pairing: free(NULL) is fine */
    CHECK(1, "sgc_free(NULL) is a no-op");

    /* Constant sanity (ABI stability). */
    CHECK(SGC_RESOURCE_FBDEV == 0 && SGC_RESOURCE_DRM == 1 && SGC_RESOURCE_MOUSE == 2 &&
              SGC_RESOURCE_KEYBOARD == 3 && SGC_RESOURCE_TOUCH == 4,
          "resource kind constants");
    CHECK(SGC_EVENT_REVOKED == 0 && SGC_EVENT_GRANTED == 1, "event kind constants");

    if (failures == 0) {
        printf("ALL C SMOKE TESTS PASSED\n");
        return 0;
    }
    printf("%d C SMOKE TESTS FAILED\n", failures);
    return 1;
}
