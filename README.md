# Domiform

> ⚠️ Status: work in progress; if code is here, it should compile but may not be
> tested or feature-complete.
---
> 🤖 I used LLMs extensively in the development of this program. See [this explanation](#llm-disclaimer)
> for more, but if you're averse to slop, be aware that it abounds. This document is entirely hand-written.
---

Write static, declarative, text-based configurations for your smart devices and the
relationships between them.

## What is it?

Domiform is a single binary that builds and runs a graph that represents your
smart home. If you have a lightbulb that runs on the [zigbee](https://en.wikipedia.org/wiki/Zigbee)
protocol and a mains-powered scene controller that runs on [z-wave](https://en.wikipedia.org/wiki/Z-Wave),
you might write this config file:

```yaml
# ./config.yaml
adapters:
  z2m:
    type: zigbee2mqtt
    url: mqtt://localhost:1883
    base_topic: zigbee2mqtt
  zwave:
    type: zwavejs
    url: ws://localhost:3002
devices:
  my_lamp:
    adapter: z2m
    address: 0x12345678
    capabilities: [switch, brightness, color]
  my_scene_controller:
    adapter: zwave
    address: "2"
    events:
      scene_1_pressed: 1:KeyPressed
rules:
  toggle_my_lamp:
    when: my_scene_controller.scene_1_pressed
    then: 
      - toggle: my_lamp
```

Run `domiform -c config.yaml`, press the button on your scene controller, and watch
as your lamp turns on of off. That's it!

Domiform aims to be as simple as possible -- both to use and to develop. It should appeal to
users who want extremely predictable and reproducible behavior instead of (or in addition to!)
runtime-configured orchestrators.

## What isn't it?

Domiform is not a drop-in replacement for any of the (excellent) smart-home orchestration tools out there.
It will never:

- require you to interact with a GUI
- store or modify its core configuration via opaque registries
- depend on proprietary hardware or software

## Dependencies

`domiform`, the binary, has no prerequisites; after building it from source or grabbing a version from
the release page, you can just run it. However, most users will need "sidecar" programs to serve
as the transport layer for devices and commands (as Domiform does not implement these protocols
itself).

For example, a zigbee lightbulb cannot communicate directly with Domiform; instead, there exists
a [zigbee2mqtt adapter](./src/adapters/zigbee2mqtt.rs) whose inclusion in your `config.yaml` file
tells Domiform how to find a sidecar process for [zigbee2mqtt](https://github.com/Koenkk/zigbee2mqtt)
running on your network. Basically, if you want to make a device available to Domiform, you'll need
an adapter for its network/framework.

## Concepts

### Devices

### Capabilities

### Events

### Rules

### Adapters

### Other

scenes, rooms, etc?

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

## LLM Disclaimer

I don't know Rust. Or, at least, when I had the idea for Domiform, I didn't know Rust
*nearly* well enough to implement it. However, Rust was the right language choice. Its
semantics, idioms, and philosophies align with what I envisioned for Domiform:

- extremely strong typing
- compilation as a feature
- runtime simplicity (single-binary)
- extensive dev tooling

Rather than compromise by (a) choosing a different language, or (b) waiting until
my Rust experience was up to the task, I relied on LLM tools to fast-track
implementation.

I don't feel good about it. I *like* learning. I *like* the sense of accomplishment
I feel after building something (even software) with my own hands. This is my first
attempt at an open-source project; I regret taking the easy way out and sanitizing
the development of its humanity.

Going forward, my intent is to phase out LLM-generated code, because:

- slop is hard to maintain
- if *I'm* not writing the code, then this isn't *my* project
- fuck tech CEOs
- I'm better at Rust than I was when I started

At time of writing, I plan to phase it out by (a) not committing any new code that
was generated by LLM tools, and (b) re-writing existing code by hand when I have
the opportunity to do so.

You may not care that I used gen-AI, and that's fine. You may care so much that you
decide to not use the program, and that's fine, too. If you contribute, please
understand that I won't accept LLM-authored changes (see the [contribution guide](./CONTRIBUTING.md)).
