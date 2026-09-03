// sgc.hpp — header-only C++ RAII wrapper over the libsgc C ABI.
//
// No protocol logic here: this is a thin, type-safe view of libsgc.h.
// Fds are owned: Event closes its GRANTED fd, Fd closes on destruction,
// SgcClient releases the C handle. SgcClient and Event are move-only.
//
// Error style: methods that can fail return std::nullopt on success and an
// error message on failure (or take an out-param / expose last_error()).
#pragma once

#include <unistd.h>

#include <cstddef>
#include <functional>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "libsgc.h"

namespace sgc {

struct Resource {
    int kind = SGC_RESOURCE_FBDEV;
    int index = 0;

    static Resource fbdev() { return {SGC_RESOURCE_FBDEV, 0}; }
    static Resource drm(int card) { return {SGC_RESOURCE_DRM, card}; }
    static Resource mouse(int device) { return {SGC_RESOURCE_MOUSE, device}; }
    static Resource keyboard(int device) { return {SGC_RESOURCE_KEYBOARD, device}; }
    static Resource touch(int device) { return {SGC_RESOURCE_TOUCH, device}; }

    sgc_resource c() const { return {kind, index}; }
};

// Owned fd (from SgcClient::fd). Closes on destruction; move-only.
class Fd {
public:
    Fd() = default;
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    Fd(Fd&& o) noexcept : fd_(o.fd_) { o.fd_ = -1; }
    Fd& operator=(Fd&& o) noexcept {
        if (this != &o) {
            reset(o.fd_);
            o.fd_ = -1;
        }
        return *this;
    }
    ~Fd() { reset(-1); }
    bool valid() const { return fd_ >= 0; }
    int get() const { return fd_; }

private:
    explicit Fd(int fd) : fd_(fd) {}
    void reset(int steal) {
        if (fd_ >= 0) ::close(fd_);
        fd_ = steal;
    }
    int fd_ = -1;
    friend class SgcClient;
};

// One event from SgcClient::pump. A GRANTED event owns its fd: it is
// closed when the event is destroyed or overwritten.
class Event {
public:
    enum class Kind : int { Revoked = SGC_EVENT_REVOKED, Granted = SGC_EVENT_GRANTED };

    Event() = default;
    Event(const Event&) = delete;
    Event& operator=(const Event&) = delete;
    Event(Event&& o) noexcept : kind_(o.kind_), resource_(o.resource_), fd_(o.fd_) {
        o.fd_ = -1;
    }
    Event& operator=(Event&& o) noexcept {
        if (this != &o) {
            kind_ = o.kind_;
            resource_ = o.resource_;
            close_fd();
            fd_ = o.fd_;
            o.fd_ = -1;
        }
        return *this;
    }
    ~Event() { close_fd(); }

    Kind kind() const { return kind_; }
    bool granted() const { return kind_ == Kind::Granted; }
    const Resource& resource() const { return resource_; }
    int fd() const { return fd_; }  // GRANTED only; owned by this event

private:
    static Event from_c(sgc_event c) {
        Event e;
        e.kind_ = static_cast<Kind>(c.kind);
        e.resource_ = {c.resource.kind, c.resource.index};
        e.fd_ = c.fd;
        return e;
    }
    void close_fd() {
        if (fd_ >= 0) ::close(fd_);
        fd_ = -1;
    }
    Kind kind_ = Kind::Revoked;
    Resource resource_;
    int fd_ = -1;
    friend class SgcClient;
};

// RAII session over the sgc_client handle. Move-only; one thread per
// client (the client is single-threaded by design).
class SgcClient {
public:
    enum class Pump { Event = 1, Timeout = 0, Error = -1 };

    SgcClient(const SgcClient&) = delete;
    SgcClient& operator=(const SgcClient&) = delete;
    SgcClient(SgcClient&& o) noexcept : c_(o.c_) { o.c_ = nullptr; }
    SgcClient& operator=(SgcClient&& o) noexcept {
        if (this != &o) {
            if (c_) sgc_release(c_);
            c_ = o.c_;
            o.c_ = nullptr;
        }
        return *this;
    }
    ~SgcClient() {
        if (c_) sgc_release(c_);
    }

    // Connect to the controller's abstract socket @sgc. Returns a client,
    // or std::nullopt with the reason in *err (if given).
    static std::optional<SgcClient> connect(std::string* err = nullptr) {
        char buf[256];
        sgc_client* c = sgc_connect(buf, sizeof(buf));
        if (!c) {
            if (err) *err = buf;
            return std::nullopt;
        }
        return SgcClient(c);
    }

    std::vector<Resource> advertised() const {
        std::vector<Resource> out;
        sgc_resource* raw = nullptr;
        size_t n = 0;
        if (c_ && sgc_advertised(c_, &raw, &n) == 0) {
            out.reserve(n);
            for (size_t i = 0; i < n; ++i) out.push_back({raw[i].kind, raw[i].index});
        }
        if (raw) sgc_free(raw);
        return out;
    }

    // Request `resource`, blocking until the server answers.
    // std::nullopt = granted; otherwise an error message.
    std::optional<std::string> acquire(const Resource& r) {
        char buf[256];
        if (sgc_acquire(c_, r.c(), buf, sizeof(buf)) == 0) return std::nullopt;
        return std::string(buf);
    }

    // Borrow `resource`: a dup of the held fd, owned by the returned Fd.
    Fd fd(const Resource& r) {
        char buf[256];
        int raw = sgc_fd(c_, r.c(), buf, sizeof(buf));
        if (raw < 0) {
            last_err_ = buf;
            return Fd();
        }
        return Fd(raw);
    }

    // One protocol step: wait up to timeout_ms (-1 = forever, 0 = once)
    // for an event. On Pump::Event, `ev` takes ownership of the fd (if
    // granted). Pump::Error ends the session — resources were already
    // reported as revoked, one event per call.
    Pump pump(int timeout_ms, Event& ev) {
        char buf[256];
        sgc_event c_ev{};
        int r = sgc_pump(c_, timeout_ms, &c_ev, buf, sizeof(buf));
        if (r == 1) {
            ev = Event::from_c(c_ev);
        } else if (r == -1) {
            last_err_ = buf;
        }
        return static_cast<Pump>(r);
    }

    // Pump until the connection ends, dispatching each event by value.
    void run(const std::function<void(Event)>& on_event) {
        Event ev;
        while (pump(-1, ev) == Pump::Event) on_event(std::move(ev));
    }

    const std::string& last_error() const { return last_err_; }

private:
    explicit SgcClient(sgc_client* c) : c_(c) {}
    sgc_client* c_ = nullptr;
    std::string last_err_;
};

static_assert(std::is_move_constructible<SgcClient>::value,
              "SgcClient must be movable");
static_assert(!std::is_copy_constructible<SgcClient>::value,
              "SgcClient must be move-only");
static_assert(!std::is_copy_constructible<Event>::value,
              "Event must be move-only");

}  // namespace sgc
