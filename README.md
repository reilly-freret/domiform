# Domiform

> ⚠️ Status: work in progress; if code is here, it should compile, but may not be
> tested or feature-complete.

Write declarative, text-based configurations for your smart devices and the
relationships between them.

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
- [x] Matter adapter
- [ ] Z-Wave adapter
- [ ] HomeKit adapter
- [ ] Documentation
- [ ] Published binaries
- [x] Dockerfile
