//! Interned identifiers.
//!
//! The compiler resolves every string reference (`"kitchen_light"`) into one of
//! these. The runtime never sees a name. These are arena-style indices — the
//! idiomatic Rust answer to "an object graph with direct references" without
//! fighting the borrow checker over `Rc<RefCell<_>>` cycles.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuleId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SceneId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScheduleId(pub u32);

/// A stateless device event declared in config (a button press, knob turn, scene
/// button). Each device's `events:` map interns to one of these; the adapter
/// resolves a raw protocol string to it, and triggers match on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActionId(pub u32);

/// Index into the engine's adapter table. Adapter 0 is always the scheduler.
pub type AdapterIdx = usize;
