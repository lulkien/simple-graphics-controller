# simple-graphics-controller

Graphics resource controller for Linux. A daemon owns graphics resources and
hands them out to clients over an abstract Unix socket (`@sgc`).

All documentation lives in [docs/](docs/):

- [README](docs/README.md) — project overview, build, usage
- [PROTOCOL](docs/PROTOCOL.md) — wire protocol specification
- [policy-engine](docs/policy-engine.md) — windowing policy engine:
  preemption, waiter queues, policies
- [resource-manager](docs/resource-manager.md) — backend features
  (fbdev/drm/input), registries, DRM lease state machine
