//! The bridge between `rs-matter`'s cluster hooks and domiform's channels.
//!
//! `rs-matter` calls these hooks on the node thread: reads (`on_off`,
//! `current_level`) when a controller queries an attribute, writes (`set_on_off`,
//! `set_device_level`) when it commands the device. On a *write* we forward the
//! desired state to the engine and nudge the host `Waker` so `poll` drains it into
//! a `RequestedChange`. Engine→node updates go the other way via
//! [`DeviceHooks::apply_engine_state`], which writes the shared cells a subsequent
//! controller read observes.
//!
//! Cluster metadata (`CLUSTER` consts) is copied from `rs-matter`'s own
//! `dimmable_light` example so the On/Off Light + Level Control conformance matches
//! what Apple Home expects.

use core::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use rs_matter::dm::clusters::app::color_control::{ColorControlHooks, RgbGamma, SetDeviceColor};
use rs_matter::dm::clusters::app::level_control::LevelControlHooks;
use rs_matter::dm::clusters::app::on_off::{self, OnOffHooks};
use rs_matter::dm::clusters::decl::bridged_device_basic_information as bridged;
use rs_matter::dm::clusters::decl::color_control::{
    ColorCapabilitiesBitmap, Feature as CcFeature, FULL_CLUSTER as COLOR_CONTROL_FULL_CLUSTER,
};
use rs_matter::dm::clusters::decl::level_control::{
    AttributeId as LcAttr, CommandId as LcCmd, Feature as LcFeature,
    FULL_CLUSTER as LEVEL_CONTROL_FULL_CLUSTER,
};
use rs_matter::dm::clusters::decl::on_off as on_off_cluster;
use rs_matter::dm::{Cluster, Dataver, InvokeContext, ReadContext};
use rs_matter::error::Error as MatterError;
use rs_matter::tlv::{Nullable, TLVBuilderParent, Utf8StrBuilder};
use rs_matter::with;

use crate::color;

use crate::ids::DeviceId;
use crate::model::CapabilityState;
use crate::wake::Waker;

/// Shared light state, read/written by both hooks and the engine-mirror task. The
/// node thread is single-threaded (one `block_on`), so `Cell` suffices; we wrap in
/// `Rc` so both facet handlers share one instance.
struct LightCells {
    on: Cell<bool>,
    /// Matter level 1..=254. Seeded to full brightness rather than `None`: the
    /// LevelControl↔OnOff coupling reads `current_level()` when the light is
    /// switched on and errors (`Error::Failure`) if it is `None`, so a light that
    /// has never had its brightness set must still report a concrete level.
    level: Cell<Option<u8>>,
}

impl Default for LightCells {
    fn default() -> Self {
        Self {
            on: Cell::new(false),
            level: Cell::new(Some(254)),
        }
    }
}

/// Owns the shared cells and the channel back to the engine, and vends per-cluster
/// facet handlers that share them.
pub struct DeviceHooks {
    device: DeviceId,
    label: String,
    cells: Rc<LightCells>,
    to_engine: Sender<(DeviceId, CapabilityState)>,
    waker: Option<Waker>,
    /// Whether the domiform device declares `Color` / `ColorTemperature`; gates
    /// which controller color writes the [`ColorFacet`] forwards to the engine.
    has_color: bool,
    has_color_temp: bool,
}

impl DeviceHooks {
    pub fn new(
        device: DeviceId,
        label: String,
        has_color: bool,
        has_color_temp: bool,
        to_engine: Sender<(DeviceId, CapabilityState)>,
        waker: Option<Waker>,
    ) -> Self {
        Self {
            device,
            label,
            cells: Rc::new(LightCells::default()),
            to_engine,
            waker,
            has_color,
            has_color_temp,
        }
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    /// Whether the device declares `Color` (an exposable hue/sat capability).
    pub fn has_color(&self) -> bool {
        self.has_color
    }

    /// Whether the device declares `ColorTemperature`.
    pub fn has_color_temp(&self) -> bool {
        self.has_color_temp
    }

    /// The Bridged Device Basic Information handler for this device's endpoint,
    /// which surfaces the domiform device name as the controller-visible label.
    pub fn bridged_facet(&self, dataver: Dataver) -> BridgedFacet {
        BridgedFacet::new(dataver, self.label.clone())
    }

    /// Reflect an engine-side state change into the shared cells, so a subsequent
    /// controller read returns domiform's current truth.
    pub fn apply_engine_state(&self, state: &CapabilityState) {
        match state {
            CapabilityState::Switch(on) => self.cells.on.set(*on),
            CapabilityState::Brightness(pct) => {
                self.cells
                    .level
                    .set(Some(super::level::pct_to_matter(*pct)));
            }
            // Color / ColorTemperature are *outward-only* today: rs-matter 0.2.0's
            // ColorControl handler owns its attribute state internally and exposes
            // no read-back hook (its `OutOfBandMessage::Update` is a no-op), so an
            // engine-side color change cannot be pushed into the node's attributes.
            // A controller still drives color fine (see `ColorFacet`); it just
            // reflects the last controller-set color, not a southbound one. Revisit
            // when the crate adds an engine→handler color-sync path.
            CapabilityState::Color { .. } | CapabilityState::ColorTemperature(_) => {}
            _ => {}
        }
    }

    pub fn on_off_hooks(&self) -> OnOffFacet {
        OnOffFacet {
            device: self.device,
            cells: self.cells.clone(),
            to_engine: self.to_engine.clone(),
            waker: self.waker.clone(),
        }
    }

    pub fn level_hooks(&self) -> LevelFacet {
        LevelFacet {
            device: self.device,
            cells: self.cells.clone(),
            to_engine: self.to_engine.clone(),
            waker: self.waker.clone(),
        }
    }

    /// ColorControl hooks for this device. `has_color` / `has_color_temp` mirror
    /// the domiform capabilities: a hue/sat write only emits `Color` if the device
    /// declares `Color`, and a mireds write only emits `ColorTemperature` if the
    /// device declares it — so the controller cannot drive a capability the
    /// southbound device does not have.
    pub fn color_hooks(&self, has_color: bool, has_color_temp: bool) -> ColorFacet {
        ColorFacet {
            device: self.device,
            to_engine: self.to_engine.clone(),
            waker: self.waker.clone(),
            has_color,
            has_color_temp,
        }
    }
}

/// Emit a controller-originated desired state to the engine and wake the host.
fn emit(
    to_engine: &Sender<(DeviceId, CapabilityState)>,
    waker: &Option<Waker>,
    device: DeviceId,
    state: CapabilityState,
) {
    let _ = to_engine.send((device, state));
    if let Some(w) = waker {
        w.wake();
    }
}

/// OnOff hooks for one device.
pub struct OnOffFacet {
    device: DeviceId,
    cells: Rc<LightCells>,
    to_engine: Sender<(DeviceId, CapabilityState)>,
    waker: Option<Waker>,
}

impl OnOffHooks for OnOffFacet {
    const CLUSTER: Cluster<'static> = on_off_cluster::FULL_CLUSTER
        .with_revision(6)
        .with_features(on_off_cluster::Feature::LIGHTING.bits())
        .with_attrs(with!(
            required;
            on_off_cluster::AttributeId::OnOff
                | on_off_cluster::AttributeId::GlobalSceneControl
                | on_off_cluster::AttributeId::OnTime
                | on_off_cluster::AttributeId::OffWaitTime
                | on_off_cluster::AttributeId::StartUpOnOff
        ))
        .with_cmds(with!(
            on_off_cluster::CommandId::Off
                | on_off_cluster::CommandId::On
                | on_off_cluster::CommandId::Toggle
                | on_off_cluster::CommandId::OffWithEffect
                | on_off_cluster::CommandId::OnWithRecallGlobalScene
                | on_off_cluster::CommandId::OnWithTimedOff
        ));

    fn on_off(&self) -> bool {
        self.cells.on.get()
    }

    fn set_on_off(&self, on: bool) {
        self.cells.on.set(on);
        emit(
            &self.to_engine,
            &self.waker,
            self.device,
            CapabilityState::Switch(on),
        );
    }

    fn start_up_on_off(&self) -> Nullable<on_off::StartUpOnOffEnum> {
        Nullable::none()
    }

    fn set_start_up_on_off(
        &self,
        _value: Nullable<on_off::StartUpOnOffEnum>,
    ) -> Result<(), MatterError> {
        Ok(())
    }

    async fn handle_off_with_effect(&self, _effect: on_off::EffectVariantEnum) {}
}

/// LevelControl hooks for one device.
pub struct LevelFacet {
    device: DeviceId,
    cells: Rc<LightCells>,
    to_engine: Sender<(DeviceId, CapabilityState)>,
    waker: Option<Waker>,
}

impl LevelControlHooks for LevelFacet {
    const MIN_LEVEL: u8 = 1;
    const MAX_LEVEL: u8 = 254;
    const FASTEST_RATE: u8 = 50;
    const CLUSTER: Cluster<'static> = LEVEL_CONTROL_FULL_CLUSTER
        .with_features(LcFeature::LIGHTING.bits() | LcFeature::ON_OFF.bits())
        .with_attrs(with!(
            required;
            LcAttr::CurrentLevel
                | LcAttr::RemainingTime
                | LcAttr::MinLevel
                | LcAttr::MaxLevel
                | LcAttr::OnOffTransitionTime
                | LcAttr::OnLevel
                | LcAttr::OnTransitionTime
                | LcAttr::OffTransitionTime
                | LcAttr::DefaultMoveRate
                | LcAttr::Options
                | LcAttr::StartUpCurrentLevel
        ))
        .with_cmds(with!(
            LcCmd::MoveToLevel
                | LcCmd::Move
                | LcCmd::Step
                | LcCmd::Stop
                | LcCmd::MoveToLevelWithOnOff
                | LcCmd::MoveWithOnOff
                | LcCmd::StepWithOnOff
                | LcCmd::StopWithOnOff
        ));

    fn set_device_level(&self, level: u8) -> Result<Option<u8>, ()> {
        self.cells.level.set(Some(level));
        emit(
            &self.to_engine,
            &self.waker,
            self.device,
            CapabilityState::Brightness(super::level::matter_to_pct(level)),
        );
        Ok(Some(level))
    }

    fn current_level(&self) -> Option<u8> {
        self.cells.level.get()
    }

    fn set_current_level(&self, level: Option<u8>) {
        self.cells.level.set(level);
    }

    fn start_up_current_level(&self) -> Result<Option<u8>, MatterError> {
        Ok(None)
    }

    fn set_start_up_current_level(&self, _value: Option<u8>) -> Result<(), MatterError> {
        Ok(())
    }
}

/// ColorControl hooks for one device.
///
/// Unlike OnOff/Level, rs-matter's ColorControl handler owns all attribute state
/// internally and only calls *out* to the device via [`set_device_color`]. So this
/// facet holds no shared cells: it forwards a controller-set color to the engine
/// (mapping hue/sat → `Color`, mireds → `ColorTemperature`) and nothing more. The
/// engine→node read-back is unsupported by the crate (see `apply_engine_state`).
///
/// [`set_device_color`]: ColorControlHooks::set_device_color
pub struct ColorFacet {
    device: DeviceId,
    to_engine: Sender<(DeviceId, CapabilityState)>,
    waker: Option<Waker>,
    /// Whether the domiform device declares `Color` (gates hue/sat emission).
    has_color: bool,
    /// Whether the domiform device declares `ColorTemperature` (gates mireds).
    has_color_temp: bool,
}

impl ColorControlHooks for ColorFacet {
    // Advertise both hue/saturation and color-temperature. A single facet type
    // must carry one cluster shape, so we always enable both features and let
    // per-device capability flags (`has_color` / `has_color_temp`) decide which
    // controller writes actually reach the engine. Revision 7 is required by the
    // handler's own `validate()`.
    const CLUSTER: Cluster<'static> = COLOR_CONTROL_FULL_CLUSTER
        .with_revision(7)
        .with_features(CcFeature::HUE_AND_SATURATION.bits() | CcFeature::COLOR_TEMPERATURE.bits())
        .with_attrs(with!(
            required;
            // Mandatory attributes (independent of features).
            rs_matter::dm::clusters::decl::color_control::AttributeId::ColorMode
                | rs_matter::dm::clusters::decl::color_control::AttributeId::Options
                | rs_matter::dm::clusters::decl::color_control::AttributeId::NumberOfPrimaries
                | rs_matter::dm::clusters::decl::color_control::AttributeId::EnhancedColorMode
                | rs_matter::dm::clusters::decl::color_control::AttributeId::ColorCapabilities
                | rs_matter::dm::clusters::decl::color_control::AttributeId::RemainingTime
                // HUE_AND_SATURATION feature.
                | rs_matter::dm::clusters::decl::color_control::AttributeId::CurrentHue
                | rs_matter::dm::clusters::decl::color_control::AttributeId::CurrentSaturation
                // COLOR_TEMPERATURE feature.
                | rs_matter::dm::clusters::decl::color_control::AttributeId::ColorTemperatureMireds
                | rs_matter::dm::clusters::decl::color_control::AttributeId::ColorTempPhysicalMinMireds
                | rs_matter::dm::clusters::decl::color_control::AttributeId::ColorTempPhysicalMaxMireds
        ))
        .with_cmds(with!(
            // StopMoveStep is mandatory when any move feature is enabled.
            rs_matter::dm::clusters::decl::color_control::CommandId::StopMoveStep
                // HUE_AND_SATURATION commands.
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveToHue
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveHue
                | rs_matter::dm::clusters::decl::color_control::CommandId::StepHue
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveToSaturation
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveSaturation
                | rs_matter::dm::clusters::decl::color_control::CommandId::StepSaturation
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveToHueAndSaturation
                // COLOR_TEMPERATURE commands.
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveToColorTemperature
                | rs_matter::dm::clusters::decl::color_control::CommandId::MoveColorTemperature
                | rs_matter::dm::clusters::decl::color_control::CommandId::StepColorTemperature
        ));

    // Full color capabilities matching the enabled features.
    const COLOR_CAPABILITIES: ColorCapabilitiesBitmap = ColorCapabilitiesBitmap::from_bits_truncate(
        ColorCapabilitiesBitmap::HUE_SATURATION.bits()
            | ColorCapabilitiesBitmap::COLOR_TEMPERATURE.bits(),
    );

    // Physical color-temperature bounds, in mireds. Mirrors domiform's tunable-
    // white range (`color::COLD_KELVIN`..`WARM_KELVIN`): 6500 K ≈ 153 mireds (min),
    // 2700 K ≈ 370 mireds (max). Must satisfy MIN < MAX.
    const COLOR_TEMP_PHYSICAL_MIN_MIREDS: u16 = (1_000_000 / color::COLD_KELVIN) as u16;
    const COLOR_TEMP_PHYSICAL_MAX_MIREDS: u16 = (1_000_000 / color::WARM_KELVIN) as u16;

    fn set_device_color(&self, target: SetDeviceColor) -> Result<(), ()> {
        match target {
            SetDeviceColor::ColorTemperature { mireds } => {
                if self.has_color_temp {
                    emit(
                        &self.to_engine,
                        &self.waker,
                        self.device,
                        CapabilityState::ColorTemperature(mireds),
                    );
                }
            }
            // Hue/sat and xy both describe a chromaticity; map either to sRGB and
            // emit as `Color`. Brightness is a separate capability, so we take the
            // sRGB primaries at full value (rs-matter's `to_rgb` assumes full).
            other => {
                if self.has_color {
                    let (r, g, b) = other.to_rgb(RgbGamma::SRgb);
                    emit(
                        &self.to_engine,
                        &self.waker,
                        self.device,
                        CapabilityState::Color { r, g, b },
                    );
                }
            }
        }
        Ok(())
    }
}

/// The Bridged Device Basic Information cluster for one bridged endpoint.
///
/// We serve the device's domiform name via this cluster's naming attributes
/// (`NodeLabel`, `ProductName`) and `VendorName`, plus the mandatory `Reachable`
/// and `UniqueID`. Which attribute a controller *displays* is up to the
/// controller: the Matter spec designates `NodeLabel` as the user-facing name,
/// but the major ecosystems (Apple Home, Google, Alexa) are inconsistent about
/// bridged devices — see the naming caveat in the module docs. We serve all of
/// them so a well-behaved controller shows the right name; a controller that
/// ignores them (as current Apple Home appears to for bridged endpoints) falls
/// back to its own device-type default and the user renames manually. This is a
/// controller limitation, not something we can fix from the accessory side.
pub struct BridgedFacet {
    dataver: Dataver,
    /// The device's domiform name, surfaced as the naming attributes.
    label: String,
    /// A stable, per-device unique id (also surfaced to the controller).
    unique_id: String,
}

impl BridgedFacet {
    pub fn new(dataver: Dataver, label: String) -> Self {
        // A stable unique id derived from the device name; controllers use this
        // to keep an accessory's identity across restarts / re-pairs.
        let unique_id = format!("domiform-{label}");
        Self {
            dataver,
            label,
            unique_id,
        }
    }
}

impl bridged::ClusterHandler for BridgedFacet {
    // Advertise the naming attributes (`NodeLabel`, `ProductName`, `VendorName`)
    // in addition to the mandatory ones. See the `BridgedFacet` docs and the
    // module-level naming caveat for why we serve all of them and why the
    // displayed name is still ultimately the controller's call.
    const CLUSTER: Cluster<'static> = bridged::FULL_CLUSTER
        .with_features(0)
        .with_attrs(with!(
            required;
            bridged::AttributeId::ProductName
                | bridged::AttributeId::VendorName
                | bridged::AttributeId::NodeLabel
                | bridged::AttributeId::UniqueID
        ))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn reachable(&self, _ctx: impl ReadContext) -> Result<bool, MatterError> {
        // domiform is the source of truth and always "present"; a device that
        // goes offline downstream is a separate concern (not yet modelled).
        Ok(true)
    }

    fn product_name<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, MatterError> {
        out.set(&self.label)
    }

    fn vendor_name<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, MatterError> {
        out.set("domiform")
    }

    /// The spec-designated user-facing name; kept in sync with `product_name`.
    fn node_label<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, MatterError> {
        out.set(&self.label)
    }

    fn unique_id<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        out: Utf8StrBuilder<P>,
    ) -> Result<P, MatterError> {
        out.set(&self.unique_id)
    }

    fn handle_keep_active(
        &self,
        _ctx: impl InvokeContext,
        _req: bridged::KeepActiveRequest<'_>,
    ) -> Result<(), MatterError> {
        // Only meaningful for ICD (sleepy) bridged devices; domiform's are always on.
        Ok(())
    }
}
