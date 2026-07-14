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
you might write this [config file](./schema/domiform.schema.json):

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
  turn_my_lamp_on_or_off:
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
- store or modify its core configuration in runtime registries
- depend on proprietary hardware or software

Additionally, Domiform itself is **not** a bridge or gateway between your server and end-devices.
See [dependencies](#dependencies) and [adapters](#adapters).

## Dependencies

`domiform`, the binary, has no prerequisites; after building it from source or grabbing a version from
the release page, you can just run it. However, most users will need "sidecar" programs to serve
as the transport layer between their server and devices (as Domiform does not implement these protocols
itself).

For example, a zigbee lightbulb cannot communicate directly with Domiform. Instead, there exists
a [zigbee2mqtt adapter](./src/adapters/zigbee2mqtt.rs) whose inclusion in your `config.yaml` file
tells Domiform how to find a sidecar process for [zigbee2mqtt](https://github.com/Koenkk/zigbee2mqtt)
running on your network. Basically, if you want to make a device available to Domiform, you'll need
an adapter to facilitate communication with the device's actual manager (which is usually a separate
program).

## Usage

Compile and run the engine for a configuration:

```zsh
freret@mac-studio:~ $ ./domiform -c path/to/config.yaml
compiled 15 device(s), 2 scene(s), 20 rule(s)
running 
```

Check validity of configuration and exit:

```zsh
freret@mac-studio:~ $ ./domiform --check # config defaults to ./config.yaml
ok: config.yaml is valid (15 device(s), 2 scene(s), 20 rule(s))
```

Compile and run in verbose mode:

```zsh
freret@mac-studio:~ $ ./domiform -c examples/simple.yaml -v
compiled 2 device(s), 0 scene(s), 1 rule(s)
[   0.000] [v] event  @0  StateReported { device: DeviceId(2), state: TimeOfDay(922) }
[   0.000] [v]   state  clock := TimeOfDay(922)
[   0.000] [v] event  @0  StateReported { device: DeviceId(2), state: SunUp(true) }
[   0.000] [v]   state  clock := SunUp(true)
running (verbose)
```

## Concepts

### Devices

The IoT entities that you want to control and/or observe. Lightbulbs, power meters, motion detectors,
momentary switches, contact sensors -- all devices. They can be registered in your config file
under the `devices` block. A device must declare an [adapter](#adapters) and should declare at least
one [capability](#capabilities) or [event](#events).

### Capabilities

The actions that a device can accept and the states that it can report. For example, if a device
declares the `switch` capability, Domiform expects it to be able to accept the
[Commands](./src/model.rs#L234) `SetSwitch` and `ToggleSwitch`; Domiform also expects the device
to report a [CapabilityState](./src/model.rs#L95) variant `Switch(bool)`.

### Events

The messages that a device can emit. Some devices don't make sense to describe only in terms
[capabilities](#capabilities). A battery-powered scene controller button, for instance, may *look*
like a switch, but (a) it can't respond to messages from a controller in any meaningful way, and (b)
it has no runtime "state" that we'd want to observe.

For devices like these, you'll add an `events` block in your device's config to map a named
[Event](./src/model.rs#L188) to a message or slug emitted by your device. You can then configure
[rules](#rules) that use that device's `event` as a trigger (defined by the `when`) rule block.

### Rules

Definitions of what should happen when something else happens. Rules are like the "edges" of the
graph that represents your smart home; they connect the devices, which are "nodes".

Rules must have a `when` clause and a list of at least one `then` clause. They also take an optional
`if` clause that will prevent the actions under `then` from firing if it evaluates to `false`. See
the [examples](./examples/) directory for a variety of rule expressions.

### Adapters

Modules that facilitate communication between Domiform and end-device manager programs. An important
part of Domiform's model is that the core runtime engine (which is responsible for evaluating
[rules](#rules)) is protocol-agnostic. The engine knows nothing about the implementation differences
between, e.g., zigbee and z-wave.

An adapter's main job is to translate:

- protocol-specific messages into [Events](./src/model.rs#L188) (inbound)
- [Commands](./src/model.rs#L234) into protocol-specific messages (outbound)

However, adapters are *not* required to emit/accept messages for a protocol; in fact, the
[Clock](./src/adapters/clock.rs), [Scheduler](./src/adapters/scheduler.rs), and
[Virtual](./src/adapters/virtual_device.rs) adapters don't have any network capabilities at all.
They exist so that the rules engine can be entirely deterministic and replayable by keeping
side effects in isolated modules.

I'm adding adapters as I encounter new protocols IRL, and you're welcome to do the same! In general,
adapter code is a lot easier than engine code to write and review because it's confined to its
own module.

## Development

See the [contribution guide](./CONTRIBUTING.md) for information on modifying Domiform's source.

[Mise](https://mise.jdx.dev/) is recommended (but not required) for local development.

Use the standard `cargo` commands to build, test, lint, etc. the program:

```zsh
freret@mac-studio:~ $ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.11s
```

```zsh
freret@mac-studio:~ $ cargo test -q
running 20 tests
....................
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
running 4 tests
....
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
# ...
```

As the project evolves, I plan to lean on `mise` for scripting anything that could be in a
Makefile:

```zsh
freret@mac-studio:~ $ mise run check
[check] $ cargo fmt
[check] $ cargo clippy
    Checking domiform v0.0.0 (/Users/reillyfreret/Documents/domiform)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.73s
[check] $ cargo check
    Checking domiform v0.0.0 (/Users/reillyfreret/Documents/domiform)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.41s
[check] $ cargo run -- --check examples/*.yaml
# ...
```

```zsh
freret@mac-studio:~ $ mise run build-docker
[assert-git-clean] $ git diff-index --quiet --cached HEAD -- || (echo "staged-but-uncommitted changes" && exit 1)
[assert-git-clean] $ git diff-files --quiet || (echo "unstaged changes" && exit 1)
# ...
```

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
