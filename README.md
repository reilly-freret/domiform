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

### Known limitations of the Matter bridge

Two rough edges are worth knowing before you rely on `matter_device`. Neither is a
bug we can fix from domiform's side:

- **Device names.** domiform serves each bridged device's real name to the
  controller (via the Bridged Device Basic Information cluster), but **Apple Home
  ignores it for bridged Matter devices** and shows its own device-type defaults
  ("Light", "Light 2", …), with the bridge itself listed as "Matter Accessory".
  This is a well-known Apple-side limitation shared by every Matter bridge (Home
  Assistant's included) — rename the accessories once in the Home app and the names
  stick. Google Home and Alexa are better-behaved.
- **Color is controller-authoritative.** A color / color-temperature change made
  *inside* domiform (a rule, or another adapter) is **not** reflected back to a
  controller reading the Matter node — the underlying `rs-matter` (0.2) color
  cluster exposes no way to push an externally-set color into its attributes. Color
  set *from* a controller works fine; only the reverse (domiform → controller color
  sync) is missing. On/off and brightness sync both directions normally. This lifts
  when `rs-matter` adds an engine→handler color write-back.

### Virtual devices (`type: virtual`)

Some devices have no protocol that reports their state — a "dumb" air conditioner
driven only by an IR remote is the canonical case. The `virtual` adapter lets you
declare a **domiform-owned stateful device**: it holds state that lives only in
domiform, echoing each commanded value back as truth. Its *behavior* lives in rules,
not the adapter.

This is what turns a stateless appliance into a real, tappable tile in Apple Home:
declare a virtual `switch`, expose it via `matter_device`, and write a rule that
translates its changes into IR. A tap in the Home app (or a wall button toggling the
same virtual switch) flips the state and fires the IR — see
[`examples/virtual_ac.yaml`](examples/virtual_ac.yaml).

Because write-only IR can't report the appliance's real state, a virtual switch is
domiform's *belief*, not ground truth: using the OEM remote can drift the tile out of
sync. That's inherent to IR, not a domiform bug. Where the appliance has discrete
on/off IR codes (not just a toggle), a rule per direction keeps them aligned.

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
      Switch, Brightness, Color and ColorTemperature are live (a color device
      appears as an extended color light); Occupancy / Battery are not yet
      projected (admitting them without their sensor/power clusters would
      advertise the wrong device type).
- [ ] Documentation
- [ ] Published binaries
- [x] Dockerfile
