# Domiform

> ⚠️ Status: work in progress; if code is here, it should compile, but may not be
> tested or feature-complete.

Write declarative, text-based configurations for your smart devices and the
relationships between them.

### Configuration vs. runtime state

Domiform's config is **fully reproducible from the YAML**: the file you author is
the single source of truth, and domiform never writes back to it. A few features
additionally need **runtime state** — data that is created at runtime and *cannot*
exist in a hand-authored file. The clearest example is the `matter_device`
adapter: when a controller (Apple Home, Google Home, Alexa) commissions domiform's
Matter node, it mints an operational certificate and keys that live only after
pairing. That is runtime data, analogous to a database's data directory — not
configuration.

Such state lives under `system.runtime_storage_path` (default: the config file's
own directory), so it is stable regardless of where domiform is launched from.
Persisting it is what lets a paired controller survive a domiform restart; delete
the state and the controller must re-pair. The reproducibility guarantee covers
the configuration — not commissioned identities, which no declarative file can
capture.

## Docker

Domiform ships a small (~17 MB) static Alpine image. You can either pull a
prebuilt image or build your own from the `Dockerfile` in this repo.

The binary reads `./config.yaml` by default, so mount your config into the
container's working directory (or pass `-c` to point elsewhere):

```sh
docker run --rm -v "$PWD/config.yaml:/config.yaml" ghcr.io/OWNER/domiform
```

### Build it yourself

For your own machine's architecture, a plain build is all you need:

```sh
docker build -t domiform .
```

To build for a specific architecture — e.g. an `arm64` Raspberry Pi from an
`amd64` laptop — pass `--platform`. Docker emulates the target during the build
via QEMU (`binfmt`), so no cross-toolchain setup is required:

```sh
docker build --platform linux/arm64 -t domiform .
```

### Multi-arch images (maintainers)

A single tag can serve every architecture; Docker picks the right variant per
host automatically. This uses `buildx` and builds one image per platform, so it
must be pushed to a registry:

```sh
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t ghcr.io/OWNER/domiform:latest \
  --push .
```

(Foreign-architecture builds run under emulation and are correspondingly slower.
On the first run, `docker run --privileged --rm tonistiigi/binfmt --install all`
registers the emulators if they aren't already present.)

## TODO

- [x] Zigbee2mqtt adapter
- [x] Matter adapter (southbound: control Matter devices via `python-matter-server`)
- [x] Z-Wave adapter
- [x] Northbound Matter bridge (`matter_device`): expose domiform devices as a
      native Matter node so Apple Home / Google Home / Alexa can control them.
      Switch + Brightness are live; Occupancy / Battery / Color are not yet
      projected (admitting them without cluster handlers would advertise the
      wrong device type).
- [ ] Documentation
- [ ] Published binaries
- [x] Dockerfile
