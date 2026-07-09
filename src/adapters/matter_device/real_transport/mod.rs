//! The live `rs-matter`-backed transport: a Matter **bridge** exposing up to
//! [`MAX_MATTER_DEVICES`](bridge::MAX_MATTER_DEVICES) domiform devices as bridged
//! endpoints (see [`bridge`] for the node/handler construction).
//!
//! `rs-matter`'s object graph is stack-allocated and borrow-linked (`&matter`
//! threads through the data model, responder and transport) and runs under a
//! single `block_on`; none of it is `Send`. So — like z2m's network client — a
//! dedicated **background thread owns the whole Matter runtime** and the engine
//! talks to it over channels.
//!
//! ```text
//!   engine (sync)                         matter thread (async, block_on)
//!   ─────────────                         ───────────────────────────────
//!   publish(dev,state) ──[chan]─────────▶ mirror task → hook cells
//!   poll()          ◀──[chan]──────────── controller write → hook → chan
//!                       + Waker::wake()    (set_on_off / set_device_level)
//! ```
//!
//! **Topology:** domiform presents as a **Matter bridge** — root endpoint (0), an
//! Aggregator endpoint (1), then one **bridged** endpoint per exposed device. Each
//! bridged endpoint advertises `DEV_TYPE_BRIDGED_NODE` + its light device type and
//! carries a `BridgedDeviceBasicInformation` cluster whose `node_label` we serve
//! with the device's domiform name — that is what makes it show up as e.g.
//! "living_room_lamp" in Apple Home rather than a generic "Matter Accessory". (The
//! bridge shape is the same whether one or many devices are exposed; this module is
//! currently wired for exactly one bridged endpoint — the N-device generalization
//! uses `DynamicNode<'_, N>` + a generated handler chain, see the design doc.)
//!
//! **Commissioning:** `rs-matter`'s built-in *test* attestation + commissioning
//! data (dev passcode `20202021`, discriminator `3840`) — correct for bring-up /
//! adding to Apple Home during development. The pairing QR/code is printed to the
//! log at startup. Production attestation certs are later work.
//!
//! **Persistence (model B):** a `DirKvBlobStore` at the resolved per-adapter state
//! directory, so a commissioned controller survives a restart.
//!
//! The structure follows `rs-matter`'s own `dimmable_light` / `onoff_light`
//! examples (v0.2.0) closely; the only domiform-specific parts are the hook impls
//! (`hooks.rs`) that bridge to the engine channels.

use core::pin::pin;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};

use embassy_futures::select::{select, select4};

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::error::{Error as MatterError, ErrorCode};
use rs_matter::im::{EthInteractionModelState, InteractionModel};
use rs_matter::pairing::qr::QrTextType;
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::utils::select::Coalesce;
use rs_matter::{Matter, MATTER_PORT};

use crate::ids::DeviceId;
use crate::model::{CapabilityKind, CapabilityState};
use crate::wake::Waker;

use super::{ExposedDevice, MatterTransport};

mod bridge;
mod hooks;
mod mdns;
use hooks::DeviceHooks;

pub use bridge::MAX_MATTER_DEVICES;

/// Percentage (0..=100) ↔ Matter level (1..=254), shared with `hooks`.
pub(crate) mod level {
    /// Brightness percent → Matter level. 0% → the minimum level 1 (Matter treats
    /// 0 as invalid for On/Off Lighting); 100% → 254.
    pub fn pct_to_matter(pct: u8) -> u8 {
        let pct = pct.min(100) as u16;
        (1 + (pct * 253 + 50) / 100) as u8
    }
    /// Matter level → brightness percent (inverse of `pct_to_matter`).
    pub fn matter_to_pct(level: u8) -> u8 {
        let level = level.clamp(1, 254) as u16;
        (((level - 1) * 100 + 126) / 253) as u8
    }
}

type Msg = (DeviceId, CapabilityState);

/// Engine-side handle: sends state to mirror, receives controller writes.
struct ChannelTransport {
    to_node: Sender<Msg>,
    from_node: Receiver<Msg>,
}

impl MatterTransport for ChannelTransport {
    fn publish(&mut self, device: DeviceId, state: &CapabilityState) {
        let _ = self.to_node.send((device, state.clone()));
    }
    fn poll(&mut self) -> Vec<Msg> {
        self.from_node.try_iter().collect()
    }
}

/// A transport that does nothing — used when no device is exposed.
struct NoopTransport;
impl MatterTransport for NoopTransport {
    fn publish(&mut self, _d: DeviceId, _s: &CapabilityState) {}
    fn poll(&mut self) -> Vec<Msg> {
        Vec::new()
    }
}

/// Spawn the Matter node thread and return a channel transport wired to it. An
/// empty exposed set yields a no-op transport (nothing to announce).
pub fn connect(
    exposed: &[ExposedDevice],
    state_dir: &Path,
    interface: Option<String>,
    waker: Option<Waker>,
) -> Box<dyn MatterTransport> {
    if exposed.is_empty() {
        return Box::new(NoopTransport);
    }
    let devices = exposed.to_vec();

    let (to_node, node_rx) = channel::<Msg>();
    let (node_tx, from_node) = channel::<Msg>();
    let state_dir = state_dir.to_path_buf();

    std::thread::Builder::new()
        .name("matter-device".into())
        .spawn(move || {
            if let Err(e) = run_node(&devices, &state_dir, interface, node_rx, node_tx, waker) {
                log::error!("[matter_device] node stopped: {e:?}");
            }
        })
        .expect("spawn matter-device thread");

    Box::new(ChannelTransport { to_node, from_node })
}

/// Run the Matter bridge node until a fatal error. All on the node thread. The
/// node exposes every device in `devices` as a bridged endpoint (see
/// [`bridge`]); the engine talks to it over the two channels.
fn run_node(
    devices: &[ExposedDevice],
    state_dir: &Path,
    interface: Option<String>,
    inbound_from_engine: Receiver<Msg>,
    outbound_to_engine: Sender<Msg>,
    waker: Option<Waker>,
) -> Result<(), MatterError> {
    // Per-device dimmability drives each bridged endpoint's shape (On/Off vs
    // dimmable light). The order here fixes the endpoint-id assignment (device `i`
    // → endpoint `2 + i`).
    let dimmable: Vec<bool> = devices
        .iter()
        .map(|d| d.capabilities.contains(&CapabilityKind::Brightness))
        .collect();

    // Root node details: start from the test config (keeping the test VID/PID so
    // the test attestation still validates during commissioning) and brand the
    // *bridge* itself. Per-device names come from each bridged endpoint's
    // `BridgedDeviceBasicInformation.node_label` (see `hooks::BridgedFacet`), not
    // from here — the root `BasicInformation` names the hub, not the accessory.
    let dev_details = rs_matter::dm::clusters::basic_info::BasicInfoConfig {
        product_name: "domiform bridge",
        device_name: "domiform bridge",
        vendor_name: "domiform",
        ..TEST_DEV_DET
    };
    let matter = Matter::new(&dev_details, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);

    // Persistence (model B): the fabric store as a directory of key-files. Use the
    // resolved per-adapter state path as that directory (created on first store).
    let store = DirKvBlobStore::new(state_dir.to_path_buf());
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let kv = matter.kv(store);
    futures_lite::future::block_on(matter.load_persist(&kv))?;

    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);
    let rand = crypto.rand()?;

    // One `DeviceHooks` per exposed device (index `i` ⇒ bridged endpoint `2 + i`),
    // each sharing the outbound channel + waker.
    let hooks: Vec<DeviceHooks> = devices
        .iter()
        .map(|d| {
            DeviceHooks::new(
                d.id,
                d.label.clone(),
                outbound_to_engine.clone(),
                waker.clone(),
            )
        })
        .collect();

    // The real per-endpoint handlers (one On/Off + Level + Bridged per device).
    // Built first, then coupled by reference — so `devices_h` must not move after.
    let devices_h = bridge::Devices::new(&hooks, &mut { rand });
    devices_h.couple();

    // Runtime-sized node metadata: root + aggregator + one bridged endpoint per
    // exposed device.
    let node = bridge::build_node(&dimmable);

    let dm = bridge::build_data_model(&node, &devices_h, rand);
    let im = InteractionModel::new(&matter, &crypto, &buffers, dm, &kv, &state);

    let responder = DefaultResponder::new(&im);
    let mut respond = pin!(responder.run::<4, 4>());
    let mut im_job = pin!(im.run());

    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)
        .map_err(|_| ErrorCode::StdIoError)?;
    let mut mdns = pin!(mdns::run_mdns(&matter, &crypto, interface.as_deref()));
    let mut transport = pin!(matter.run(&crypto, &socket, &socket, &socket));

    if !matter.is_commissioned() {
        // Print the pairing QR/code and open commissioning so the device can be
        // added to Apple Home / Google / Alexa.
        matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;
    }

    // Drain engine→node state updates into the matching device's hooks so
    // controller reads see truth.
    let mut mirror = pin!(mirror_engine_state(&inbound_from_engine, &hooks));

    // Compose all tasks. `Coalesce` (from rs-matter) flattens a `selectN` of
    // same-`Result` futures into one; nest so the mirror pump joins the set.
    let all = select4(
        &mut transport,
        &mut mdns,
        select(&mut respond, &mut im_job).coalesce(),
        &mut mirror,
    )
    .coalesce();
    futures_lite::future::block_on(all)
}

/// Continuously apply engine state updates to the right device's hooks by device
/// id (cooperative poll of the sync channel; never returns, so it doesn't complete
/// the select). Inert padding slots are skipped implicitly — their sentinel id
/// never matches an engine device.
async fn mirror_engine_state(rx: &Receiver<Msg>, hooks: &[DeviceHooks]) -> Result<(), MatterError> {
    loop {
        while let Ok((device, state)) = rx.try_recv() {
            if let Some(h) = hooks.iter().find(|h| h.device_id() == device) {
                h.apply_engine_state(&state);
            }
        }
        // A driver-free periodic yield via async-io's own timer (avoids pulling in
        // an embassy-time driver, which isn't wired in this link configuration).
        async_io::Timer::after(std::time::Duration::from_millis(50)).await;
    }
}
