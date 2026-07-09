//! Multi-device bridge construction: a runtime-sized `DynamicNode` plus a
//! **fixed-depth** handler chain that exposes any number of domiform devices as
//! one Matter bridge — no compile-time device cap.
//!
//! ## Why dispatch shims (not one `.chain()` link per device)
//!
//! `rs-matter`'s handler chain is compile-time-typed: every `.chain()` nests a new
//! `ChainedHandler<M, H, T>` layer into the type. `rs-matter`'s `OnOffHandler` /
//! `LevelControlHandler` each bind to a *single* endpoint, so the obvious approach
//! — one link per (endpoint, cluster) — makes the chain type ~5·N deep. At a few
//! dozen devices that nested type is deep enough that rustc's layout computation
//! for `InteractionModel`'s async body blows up (super-linear memory). Not viable.
//!
//! Instead we keep the chain **depth constant** and move the per-device fan-out to
//! runtime. For each stateful cluster we register **one** handler — a *dispatch
//! shim* — matched to that cluster on *any* endpoint (`EpClMatcher::new(None,
//! cluster_id)`). The shim owns a `Vec` of `rs-matter`'s real per-endpoint handlers
//! (one per device) and, on each read/write/invoke, forwards to the instance whose
//! endpoint matches `ctx.endpt()`. We reuse `rs-matter`'s On/Off and Level Control
//! logic wholesale — the shim only routes. The chain is therefore a fixed ~6 links
//! regardless of device count, and there is no device cap.
//!
//! ## Endpoint layout (Matter bridge)
//!
//! ```text
//!   ep 0            root node
//!   ep 1            aggregator (DEV_TYPE_AGGREGATOR)
//!   ep 2 + i        bridged device i (DEV_TYPE_BRIDGED_NODE + light type)
//! ```

use futures_util::stream::{FuturesUnordered, StreamExt};

use rs_matter::dm::clusters::app::level_control::{self, LevelControlHooks as _};
use rs_matter::dm::clusters::app::on_off::{self, OnOffHooks as _};
use rs_matter::dm::clusters::decl::bridged_device_basic_information as bridged;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::devices::{
    DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_ON_OFF_LIGHT,
};
use rs_matter::dm::networks::SysNetifs;
use rs_matter::dm::{
    endpoints, Async, AsyncHandler, DataModel, Dataver, DynamicNode, Endpoint, EpClMatcher,
    HandlerContext, InvokeContext, InvokeReply, MatchContext, ReadContext, ReadReply, WriteContext,
};
use rs_matter::error::Error as MatterError;
use rs_matter::{clusters, devices};

use super::hooks::{self, BridgedFacet, DeviceHooks, LevelFacet, OnOffFacet};

/// The concrete On/Off handler type for one bridged endpoint.
type OnOff<'a> = on_off::OnOffHandler<'a, OnOffFacet, LevelFacet>;
/// The concrete Level Control handler type for one bridged endpoint.
type Level<'a> = level_control::LevelControlHandler<'a, LevelFacet, OnOffFacet>;

/// Endpoint id of the aggregator (endpoint 1 in a Matter bridge).
pub const AGGREGATOR_EP: u16 = 1;
/// Endpoint id of the first bridged device; device `i` is `FIRST_BRIDGED_EP + i`.
pub const FIRST_BRIDGED_EP: u16 = 2;

/// The endpoint id for bridged device `i` (0-based).
pub fn ep_of(i: usize) -> u16 {
    FIRST_BRIDGED_EP + i as u16
}

/// The maximum number of devices a single `matter_device` adapter can expose.
///
/// The dispatch-shim design has no *compile-time* device cap (the handler chain is
/// fixed-depth regardless of device count). This is a soft sanity limit enforced by
/// the resolver so a runaway config is a clear error rather than an accidental
/// hundred-endpoint node. It also bounds the endpoint-metadata `DynamicNode`
/// capacity ([`NODE_CAPACITY`]).
pub const MAX_MATTER_DEVICES: usize = 64;

/// `DynamicNode` endpoint capacity: root + aggregator + up to [`MAX_MATTER_DEVICES`].
pub const NODE_CAPACITY: usize = MAX_MATTER_DEVICES + 2;

// --- Node metadata ----------------------------------------------------------
// Shared `'static` endpoint templates: every bridged light endpoint of the same
// shape has identical cluster / device-type lists, so we copy a `const` template
// and patch its id. `DeviceType` isn't a nameable public type, so the
// `devices!`/`clusters!` macros build the slices for us.

/// Template for a bridged **dimmable** light endpoint (id patched per device).
const DIMMABLE_TEMPLATE: Endpoint<'static> = Endpoint::new(
    FIRST_BRIDGED_EP,
    devices!(DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_BRIDGED_NODE),
    clusters!(
        desc::DescHandler::CLUSTER,
        groups::GroupsHandler::CLUSTER,
        bridged::FULL_CLUSTER,
        hooks::OnOffFacet::CLUSTER,
        hooks::LevelFacet::CLUSTER
    ),
);

/// Template for a bridged **on/off** light endpoint (no Level Control).
const ONOFF_TEMPLATE: Endpoint<'static> = Endpoint::new(
    FIRST_BRIDGED_EP,
    devices!(DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE),
    clusters!(
        desc::DescHandler::CLUSTER,
        groups::GroupsHandler::CLUSTER,
        bridged::FULL_CLUSTER,
        hooks::OnOffFacet::CLUSTER
    ),
);

/// Build the runtime-sized bridge node metadata for the exposed devices (one
/// `dimmable` flag each). Endpoints: root (0), aggregator (1), then one bridged
/// endpoint per device (2..2+n).
pub fn build_node(dimmable: &[bool]) -> DynamicNode<'static, NODE_CAPACITY> {
    // Root endpoint (id 0). Built as a `const` so the `root_endpoint!` macro's
    // borrowed cluster/device-type arrays get `'static` promotion.
    const ROOT: Endpoint<'static> = rs_matter::root_endpoint!(eth);
    let aggregator = Endpoint::new(
        AGGREGATOR_EP,
        devices!(DEV_TYPE_AGGREGATOR),
        clusters!(desc::DescHandler::CLUSTER),
    );

    let mut node = DynamicNode::new();
    let _ = node.add(ROOT);
    let _ = node.add(aggregator);
    for (i, &dim) in dimmable.iter().enumerate() {
        let mut ep = if dim {
            DIMMABLE_TEMPLATE
        } else {
            ONOFF_TEMPLATE
        };
        ep.id = ep_of(i);
        let _ = node.add(ep);
    }
    node
}

/// A [`rs_matter::dm::Metadata`] adapter over a [`DynamicNode`] (which only offers
/// a `Node<'_>` view via `.node()`).
pub struct NodeMeta<'a>(pub &'a DynamicNode<'static, NODE_CAPACITY>);

impl rs_matter::dm::Metadata for NodeMeta<'_> {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&rs_matter::dm::Node<'_>) -> R,
    {
        f(&self.0.node())
    }
}

// --- Per-device handler storage + dispatch shims ----------------------------

/// The real per-endpoint `rs-matter` handlers, one entry per exposed device (index
/// `i` ⇒ endpoint `ep_of(i)`). Built once, then borrowed by the dispatch shims and
/// by the On/Off↔Level coupling — so it must not move after `couple`.
pub struct Devices<'a> {
    pub on_off: Vec<OnOff<'a>>,
    pub level: Vec<Level<'a>>,
    pub bridged: Vec<BridgedFacet>,
}

impl<'a> Devices<'a> {
    /// Build one On/Off + Level + Bridged handler per device from its hooks.
    pub fn new(hooks: &'a [DeviceHooks], rand: &mut (impl rand::RngCore + Copy)) -> Self {
        let on_off = hooks
            .iter()
            .enumerate()
            .map(|(i, h)| {
                on_off::OnOffHandler::new(Dataver::new_rand(rand), ep_of(i), h.on_off_hooks())
            })
            .collect();
        let level = hooks
            .iter()
            .enumerate()
            .map(|(i, h)| {
                level_control::LevelControlHandler::new(
                    Dataver::new_rand(rand),
                    ep_of(i),
                    h.level_hooks(),
                    level_control::AttributeDefaults::default(),
                )
            })
            .collect();
        let bridged = hooks
            .iter()
            .map(|h| h.bridged_facet(Dataver::new_rand(rand)))
            .collect();
        Self {
            on_off,
            level,
            bridged,
        }
    }

    /// Couple each device's On/Off and Level Control handlers (mutual generic
    /// resolution + the spec's On/Off↔Level coupling). Must run after `new`; the
    /// `Vec`s must not move afterward (the coupling stores sibling references).
    pub fn couple(&'a self) {
        for (o, l) in self.on_off.iter().zip(&self.level) {
            o.init(Some(l));
            l.init(Some(o));
        }
    }

    fn len(&self) -> usize {
        self.on_off.len()
    }
}

/// Find the device index whose bridged endpoint matches `ctx`'s endpoint, if any.
fn index_of(ctx: &impl MatchContext, n: usize) -> Option<usize> {
    let ep = ctx.endpt()?;
    let i = ep.checked_sub(FIRST_BRIDGED_EP)? as usize;
    (i < n).then_some(i)
}

/// One dispatch shim per stateful cluster: matched to that cluster on *any* bridged
/// endpoint, it routes each operation to the device instance selected by
/// `ctx.endpt()` and drives every instance's background `run()` loop concurrently.
///
/// `$name` is the shim type, `$field` the `Devices` field it dispatches over, and
/// `$adapt(inner)` wraps one real handler into an `AsyncHandler` (so we can reuse
/// `rs-matter`'s per-endpoint logic verbatim).
macro_rules! dispatch_shim {
    ($name:ident, $field:ident, $adapt:expr) => {
        /// Endpoint-dispatching shim (see [`dispatch_shim!`]). Borrows the shared
        /// [`Devices`] and picks the per-endpoint handler by `ctx.endpt()`.
        pub struct $name<'a>(&'a Devices<'a>);

        impl<'a> AsyncHandler for $name<'a> {
            async fn read(
                &self,
                ctx: impl ReadContext,
                reply: impl ReadReply,
            ) -> Result<(), MatterError> {
                match index_of(&ctx, self.0.len()) {
                    Some(i) => $adapt(&self.0.$field[i]).read(ctx, reply).await,
                    None => Err(rs_matter::error::ErrorCode::EndpointNotFound.into()),
                }
            }

            async fn write(&self, ctx: impl WriteContext) -> Result<(), MatterError> {
                match index_of(&ctx, self.0.len()) {
                    Some(i) => $adapt(&self.0.$field[i]).write(ctx).await,
                    None => Err(rs_matter::error::ErrorCode::EndpointNotFound.into()),
                }
            }

            async fn invoke(
                &self,
                ctx: impl InvokeContext,
                reply: impl InvokeReply,
            ) -> Result<(), MatterError> {
                match index_of(&ctx, self.0.len()) {
                    Some(i) => $adapt(&self.0.$field[i]).invoke(ctx, reply).await,
                    None => Err(rs_matter::error::ErrorCode::CommandNotFound.into()),
                }
            }

            fn bump_dataver(&self, ctx: impl MatchContext) {
                if let Some(i) = index_of(&ctx, self.0.len()) {
                    $adapt(&self.0.$field[i]).bump_dataver(ctx);
                }
            }

            async fn run(&self, ctx: impl HandlerContext) -> Result<(), MatterError> {
                // Drive every device's background loop concurrently. Each future
                // owns its adaptor (an `async move` block moves the thin
                // `&handler` wrapper inside, so nothing borrows a temporary that
                // outlives the future). The inner `run()`s never complete (they
                // loop forever); if one errors we surface it. An empty set never
                // resolves (there is nothing to do).
                let ctx = &ctx;
                let mut jobs: FuturesUnordered<_> = (0..self.0.len())
                    .map(|i| {
                        let inner = $adapt(&self.0.$field[i]);
                        async move { inner.run(ctx).await }
                    })
                    .collect();
                if jobs.is_empty() {
                    core::future::pending::<()>().await;
                    return Ok(());
                }
                while let Some(r) = jobs.next().await {
                    r?;
                }
                Ok(())
            }
        }
    };
}

dispatch_shim!(OnOffDispatch, on_off, on_off::HandlerAsyncAdaptor);
dispatch_shim!(LevelDispatch, level, level_control::HandlerAsyncAdaptor);
dispatch_shim!(BridgedDispatch, bridged, |h| Async(
    bridged::HandlerAdaptor(h)
));

/// Build the composed data model for the whole bridge: node metadata + a
/// **fixed-depth** handler chain (system handlers, the aggregator descriptor, a
/// shared bridged-endpoint descriptor + groups handler matched on any endpoint,
/// then the three per-cluster dispatch shims). Depth is constant regardless of the
/// number of exposed devices.
pub fn build_data_model<'a>(
    node: &'a DynamicNode<'static, NODE_CAPACITY>,
    devices: &'a Devices<'a>,
    mut rand: impl rand::RngCore + Copy,
) -> impl DataModel + 'a {
    let handler = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(rand)
        // Aggregator (ep 1) descriptor — enumerates the bridged endpoints as Parts.
        .chain(
            EpClMatcher::new(Some(AGGREGATOR_EP), Some(desc::DescHandler::CLUSTER.id)),
            Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut rand)).adapt()),
        )
        // Descriptor + Groups are stateless, so one instance each serves every
        // bridged endpoint (matched on any endpoint for that cluster).
        .chain(
            EpClMatcher::new(None, Some(desc::DescHandler::CLUSTER.id)),
            Async(desc::DescHandler::new(Dataver::new_rand(&mut rand)).adapt()),
        )
        .chain(
            EpClMatcher::new(None, Some(groups::GroupsHandler::CLUSTER.id)),
            Async(groups::GroupsHandler::new(Dataver::new_rand(&mut rand)).adapt()),
        )
        // The three stateful clusters, each dispatched per endpoint by its shim.
        .chain(
            EpClMatcher::new(None, Some(bridged::FULL_CLUSTER.id)),
            BridgedDispatch(devices),
        )
        .chain(
            EpClMatcher::new(None, Some(hooks::OnOffFacet::CLUSTER.id)),
            OnOffDispatch(devices),
        )
        .chain(
            EpClMatcher::new(None, Some(hooks::LevelFacet::CLUSTER.id)),
            LevelDispatch(devices),
        );

    (NodeMeta(node), handler)
}
