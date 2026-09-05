# simple-graphics-controller

Graphics resource controller for Linux. A daemon owns graphics resources and
hands them out to clients over an abstract Unix socket (`@sgc`). This repo
ships only the daemon; the wire contract, client libraries and demos live in
sibling repos (see [docs/README.md](docs/README.md) — "Ecosystem").

Design docs in [docs/](docs/):

- [README](docs/README.md) — what it does, backends, build/run, ecosystem
- [policy-engine](docs/policy-engine.md) — windowing policy: preemption,
  waiter queues, policies
- [resource-manager](docs/resource-manager.md) — backends (fbdev/drm/input),
  registries, DRM lease state machine
