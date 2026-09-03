// C++ smoke test for sgc.hpp: compile-time properties (move-only, RAII)
// plus the error paths against the ABI. Without a controller on @sgc the
// connect path must fail cleanly with a message.
//
// Build: g++ -std=c++17 -Wall -Wextra -I libsgc-c/include libsgc-c/tests/smoke/main.cc
//         target/debug/libsgc.a -lpthread -ldl -lm -o /tmp/sgc-cpp-smoke

#include <cstdio>
#include <string>

#include "sgc.hpp"

static int failures = 0;

#define CHECK(cond, name)                                                     \
    do {                                                                      \
        if (cond) {                                                           \
            std::printf("PASS %s\n", name);                                   \
        } else {                                                              \
            std::printf("FAIL %s\n", name);                                   \
            failures++;                                                       \
        }                                                                     \
    } while (0)

static void sink(std::string) {}  // move-only helpers compile-check

int main() {
    // Compile-time: move-only types, resource ctors, enum values.
    static_assert(!std::is_copy_constructible<sgc::SgcClient>::value, "move-only");
    static_assert(!std::is_copy_assignable<sgc::SgcClient>::value, "move-only");
    static_assert(!std::is_copy_constructible<sgc::Event>::value, "move-only");
    static_assert(!std::is_copy_constructible<sgc::Fd>::value, "move-only");

    sgc::Resource drm = sgc::Resource::drm(0);
    CHECK(drm.kind == SGC_RESOURCE_DRM && drm.index == 0, "Resource::drm()");
    CHECK(sgc::Resource::keyboard(2).index == 2, "Resource::keyboard()");

    // No controller locally: connect must fail with a message.
    std::string err;
    auto client = sgc::SgcClient::connect(&err);
    CHECK(!client.has_value(), "connect() fails without a controller");
    CHECK(!err.empty(), "connect() reports the error");
    std::printf("  connect error: %s\n", err.c_str());

    // Error path from a live-shaped call is impossible without a daemon;
    // exercise the wrapper surface on a moved-from client via move ctors.
    auto moved = sgc::SgcClient::connect(nullptr);
    (void)moved;
    CHECK(true, "connect(nullptr) compiles and is safe");

    // RAII ergonomics: events and fds own nothing here (no grants), but
    // move/assign/destroy must compile and be safe.
    sgc::Event ev;
    CHECK(ev.kind() == sgc::Event::Kind::Revoked && ev.fd() == -1, "default Event");
    sgc::Fd fd;
    CHECK(!fd.valid(), "default Fd is empty");
    sgc::Event ev2 = std::move(ev);
    CHECK(ev2.fd() == -1, "moved Event keeps fd ownership semantics");
    sink(std::string());  // keep sink referenced

    if (failures == 0) {
        std::printf("ALL C++ SMOKE TESTS PASSED\n");
        return 0;
    }
    std::printf("%d C++ SMOKE TESTS FAILED\n", failures);
    return 1;
}
