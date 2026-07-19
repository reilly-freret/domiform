//! Read-only GUI: a lightweight HTTP server that renders the compiled config as a
//! device/rule graph (devices are nodes, rules are edges).
//!
//! This is a *host* concern, like the real-time pump loop in `main` — it lives in
//! the binary, not the library. The engine stays single-threaded and untouched;
//! the server runs on its own `std::thread` and serves an owned [`GraphSnapshot`]
//! built from the [`CompiledConfig`] at startup.
//!
//! The page is fully self-contained and offline — no CDN; the one dependency
//! (dagre, a mature layered-graph layout engine) is vendored in `assets/`,
//! embedded in the binary, and served locally at `/dagre.js`. The graph is
//! embedded as JSON; the client splits it into rule-connected clusters, lays each
//! out with dagre, packs the clusters to the viewport's aspect ratio, and parks
//! rule-less devices in an "unconnected" tray. Interaction is aimed at the
//! questions a config author actually asks: rule-count badges (which devices do
//! many rules touch?), the tray (which devices does no rule reference?), and
//! click-to-trace downstream reachability (what happens when this fires?).
//! See [`SCRIPT`].
//!
//! Topology is static after boot today, so a one-shot snapshot suffices. The
//! server holds it behind an `Arc`; a future "reload config" feature would swap
//! that `Arc` to refresh the graph without a restart.

use std::sync::Arc;
use std::thread;

use domiform::{
    ActionId, CapabilityKind, CmpOp, Command, CompiledConfig, Condition, CrossDir, DeviceId, Millis,
    Trigger,
};
use serde::Serialize;

/// A node in the rendered graph: a device (or the synthetic clock).
#[derive(Serialize)]
pub struct Node {
    /// The `DeviceId`'s underlying index — stable within one snapshot.
    pub id: u32,
    pub label: String,
}

/// An edge in the rendered graph: one rule, from its trigger's source device to a
/// device its commands act on. Labeled with the rule's friendly name.
#[derive(Serialize)]
pub struct Edge {
    pub from: u32,
    pub to: u32,
    pub label: String,
    /// A human-readable, multi-line description of what the rule does — the
    /// command this edge represents, plus the rule's trigger, condition, and
    /// `for` duration. Shown on hover. Built with the compiler's friendly names.
    pub detail: String,
}

/// An owned, engine-independent view of the config graph. Carries friendly names
/// for every node and edge (see the module note on where names come from), so the
/// server never has to reach back into the compiler or the live engine.
#[derive(Serialize)]
pub struct GraphSnapshot {
    pub system_name: Option<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl GraphSnapshot {
    /// Build the graph from a compiled config. Nodes are the configured devices
    /// (plus the synthetic clock, if any rule hangs off it); edges are rules, from
    /// the trigger's source device to each device the rule's commands target.
    ///
    /// Friendly names come straight from the compiler: `DeviceDef.name` and
    /// `Rule.name`. The one id without a config name is the synthetic clock
    /// (`cfg.clock_device()`), which we name `"clock"` — mirroring how `main`
    /// names it for the stderr observer.
    pub fn from_config(cfg: &CompiledConfig) -> GraphSnapshot {
        let clock = cfg.clock_device();

        let mut nodes: Vec<Node> = cfg
            .devices
            .iter()
            .map(|d| Node {
                id: d.id.0,
                label: d.name.clone(),
            })
            .collect();

        let mut edges = Vec::new();
        let mut clock_used = false;
        for rule in &cfg.rules {
            // Source: the device whose event fires the rule. Time/timer triggers
            // (and a catch-all command-failure trigger) have no device, so they
            // originate at the clock node.
            let source = trigger_device(&rule.trigger).unwrap_or(clock);
            for cmd in &rule.commands {
                // Scene/timer commands target no device — skip them; they don't
                // form a device→device edge.
                if let Some(target) = cmd.target_device() {
                    if source == clock {
                        clock_used = true;
                    }
                    edges.push(Edge {
                        from: source.0,
                        to: target.0,
                        label: rule.name.clone(),
                        detail: edge_detail(cfg, rule, cmd),
                    });
                }
            }
        }

        // Only surface the synthetic clock as a node when something actually
        // originates from it — an unconnected "clock" box would just be noise.
        if clock_used {
            nodes.push(Node {
                id: clock.0,
                label: "clock".to_string(),
            });
        }

        GraphSnapshot {
            system_name: cfg.system.name.clone(),
            nodes,
            edges,
        }
    }
}

/// A multi-line hover description for one edge: the specific command it
/// represents, then the rule's trigger, condition (if any), and `for` duration.
fn edge_detail(cfg: &CompiledConfig, rule: &domiform::Rule, cmd: &Command) -> String {
    let mut detail = describe_command(cfg, cmd);
    detail.push_str(&format!("\ntrigger: {}", describe_trigger(cfg, &rule.trigger)));
    if let Some(cond) = describe_condition(cfg, &rule.condition) {
        detail.push_str(&format!("\nif: {cond}"));
    }
    if let Some(ms) = rule.for_duration {
        detail.push_str(&format!("\nfor: {ms}ms"));
    }
    detail
}

/// A device's friendly name (the synthetic clock reads as `clock`).
fn dev_name(cfg: &CompiledConfig, id: DeviceId) -> String {
    if id == cfg.clock_device() {
        "clock".to_string()
    } else {
        cfg.device(id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("device#{}", id.0))
    }
}

/// The local event name a device declared for an `ActionId`, e.g. `bottom_right_single`.
fn action_name(cfg: &CompiledConfig, device: DeviceId, action: ActionId) -> String {
    cfg.device(device)
        .and_then(|d| d.events.iter().find(|e| e.id == action))
        .map(|e| e.name.clone())
        .unwrap_or_else(|| format!("action#{}", action.0))
}

fn cap_name(kind: CapabilityKind) -> String {
    // Debug is the readable PascalCase name (Switch, Occupancy, ColorTemperature).
    format!("{kind:?}")
}

fn cmp_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "<",
        CmpOp::Le => "≤",
        CmpOp::Eq => "=",
        CmpOp::Ne => "≠",
        CmpOp::Ge => "≥",
        CmpOp::Gt => ">",
    }
}

fn describe_trigger(cfg: &CompiledConfig, t: &Trigger) -> String {
    match t {
        Trigger::Action { device, action } => {
            format!("on '{}' from {}", action_name(cfg, *device, *action), dev_name(cfg, *device))
        }
        Trigger::Changed { device, kind, to } => {
            format!("when {} {} → {}", dev_name(cfg, *device), cap_name(*kind), to)
        }
        Trigger::Crosses { device, kind, bound, dir } => {
            let arrow = match dir {
                CrossDir::Above => "rises to",
                CrossDir::Below => "falls to",
            };
            format!("when {} {} {} {}", dev_name(cfg, *device), cap_name(*kind), arrow, bound)
        }
        Trigger::Reports { device, kind } => {
            format!("on every {} {} report", dev_name(cfg, *device), cap_name(*kind))
        }
        Trigger::Timer { .. } => "when a timer elapses".to_string(),
        Trigger::Time { schedule } => {
            let name = cfg
                .schedules
                .iter()
                .find(|s| s.id == *schedule)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| format!("schedule#{}", schedule.0));
            format!("at schedule '{name}'")
        }
        Trigger::CommandFailed { device } => match device {
            Some(d) => format!("when a command to {} fails", dev_name(cfg, *d)),
            None => "when any command fails".to_string(),
        },
    }
}

/// A readable condition, or `None` for the always-true condition (nothing to show).
fn describe_condition(cfg: &CompiledConfig, c: &Condition) -> Option<String> {
    match c {
        Condition::Always => None,
        Condition::Not(inner) => Some(format!(
            "not ({})",
            describe_condition(cfg, inner).unwrap_or_else(|| "always".to_string())
        )),
        Condition::And(parts) => join_conditions(cfg, parts, " and "),
        Condition::Or(parts) => join_conditions(cfg, parts, " or "),
        Condition::BoolEquals { device, kind, value } => {
            Some(format!("{} {} is {}", dev_name(cfg, *device), cap_name(*kind), value))
        }
        Condition::Compare { device, kind, op, value } => Some(format!(
            "{} {} {} {}",
            dev_name(cfg, *device),
            cap_name(*kind),
            cmp_symbol(*op),
            value
        )),
        Condition::ColorEquals { device, r, g, b } => {
            Some(format!("{} color is #{r:02X}{g:02X}{b:02X}", dev_name(cfg, *device)))
        }
    }
}

fn join_conditions(cfg: &CompiledConfig, parts: &[Condition], sep: &str) -> Option<String> {
    let rendered: Vec<String> = parts.iter().filter_map(|c| describe_condition(cfg, c)).collect();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join(sep))
    }
}

fn describe_command(cfg: &CompiledConfig, cmd: &Command) -> String {
    let over = |t: &Option<Millis>| t.map(|m| format!(" over {m}ms")).unwrap_or_default();
    match cmd {
        Command::SetSwitch { device, on } => {
            format!("set {} {}", dev_name(cfg, *device), if *on { "on" } else { "off" })
        }
        Command::ToggleSwitch { device } => format!("toggle {}", dev_name(cfg, *device)),
        Command::SetBrightness { device, value, transition } => {
            format!("set {} brightness to {}%{}", dev_name(cfg, *device), value, over(transition))
        }
        Command::DecreaseBrightness { device, value } => {
            format!("decrease {} brightness by {}", dev_name(cfg, *device), value)
        }
        Command::IncreaseBrightness { device, value } => {
            format!("increase {} brightness by {}", dev_name(cfg, *device), value)
        }
        Command::SetColor { device, r, g, b, transition } => format!(
            "set {} color to #{r:02X}{g:02X}{b:02X}{}",
            dev_name(cfg, *device),
            over(transition)
        ),
        Command::SetColorTemperature { device, mireds, transition } => {
            format!("set {} color temp to {} mired{}", dev_name(cfg, *device), mireds, over(transition))
        }
        Command::SendIrCode { device, code } => {
            format!("send IR code '{}' to {}", code, dev_name(cfg, *device))
        }
        Command::ActivateScene { .. } => "activate a scene".to_string(),
        Command::ScheduleTimer { .. } => "schedule a timer".to_string(),
        Command::CancelTimer { .. } => "cancel a timer".to_string(),
    }
}

/// The device whose event fires this trigger, if any. Unlike `Trigger::watched`
/// (which is scoped to the edge triggers that support a `for` qualifier), this
/// covers every trigger that names a device. Time/timer triggers, and a
/// device-agnostic command-failure trigger, return `None` → the clock node.
fn trigger_device(t: &Trigger) -> Option<DeviceId> {
    match t {
        Trigger::Action { device, .. }
        | Trigger::Changed { device, .. }
        | Trigger::Crosses { device, .. }
        | Trigger::Reports { device, .. } => Some(*device),
        Trigger::CommandFailed { device } => *device,
        Trigger::Timer { .. } | Trigger::Time { .. } => None,
    }
}

/// Connectivity stats for the header line: the number of rule-connected clusters
/// (connected components with at least one edge) and the number of devices no
/// rule references at all. These directly answer "how much of the home is one
/// connected system vs. isolated smart rooms?" and "which devices are inert?".
fn cluster_stats(g: &GraphSnapshot) -> (usize, usize) {
    use std::collections::{HashMap, HashSet};
    fn find(parent: &mut HashMap<u32, u32>, mut x: u32) -> u32 {
        while parent[&x] != x {
            let grandparent = parent[&parent[&x]];
            parent.insert(x, grandparent);
            x = grandparent;
        }
        x
    }
    let mut parent: HashMap<u32, u32> = g.nodes.iter().map(|n| (n.id, n.id)).collect();
    let mut touched: HashSet<u32> = HashSet::new();
    for e in &g.edges {
        if parent.contains_key(&e.from) && parent.contains_key(&e.to) {
            touched.insert(e.from);
            touched.insert(e.to);
            let (a, b) = (find(&mut parent, e.from), find(&mut parent, e.to));
            if a != b {
                parent.insert(a, b);
            }
        }
    }
    let roots: HashSet<u32> = touched.iter().map(|&id| find(&mut parent, id)).collect();
    let orphans = g.nodes.iter().filter(|n| !touched.contains(&n.id)).count();
    (roots.len(), orphans)
}

/// Escape the few characters that matter for text interpolated into HTML markup
/// (the page title). SVG label text is set via the DOM (`textContent`) client-side,
/// so it needs no escaping here.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the full self-contained HTML page: the graph as embedded JSON plus the
/// inline force-directed renderer. Built by concatenation (not `format!`) so the
/// script's `{}`/braces need no escaping.
fn render_page(g: &GraphSnapshot) -> String {
    let title = g.system_name.as_deref().unwrap_or("domiform");

    let mut html = String::new();
    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str("<title>");
    html.push_str(&html_escape(title));
    html.push_str(" — domiform</title>\n<style>");
    html.push_str(STYLE);
    html.push_str("</style>\n</head>\n<body>\n");

    // Header with the site name and a live count + a one-line interaction hint.
    html.push_str("<header><h1>");
    html.push_str(&html_escape(title));
    html.push_str("</h1><p>");
    let (clusters, orphans) = cluster_stats(g);
    html.push_str(&format!(
        "{} device(s) · {} rule edge(s) · {} cluster(s) · {} unconnected",
        g.nodes.len(),
        g.edges.len(),
        clusters,
        orphans
    ));
    html.push_str(" — click a device to trace what it triggers · hover an edge for its rule &amp; parameters · scroll to zoom · drag to pan · double-click to fit");
    html.push_str("</p></header>\n");

    if g.nodes.is_empty() {
        html.push_str("<main><p class=\"empty\">No devices configured.</p></main>\n");
        html.push_str("</body>\n</html>\n");
        return html;
    }

    html.push_str("<svg id=\"graph\"></svg>\n");

    // dagre (layered layout) is served by this same binary at /dagre.js — offline,
    // no CDN. It must load before the renderer below runs.
    html.push_str("<script src=\"/dagre.js\"></script>\n");

    // Embed the graph as inert JSON (parsed by the script), not executable JS.
    // Escaping `<` as < inside the JSON prevents a `</script>` breakout if a
    // device/rule name ever contained one; < decodes back to `<` on parse.
    let json = serde_json::to_string(g).unwrap_or_else(|_| "{}".to_string());
    html.push_str("<script id=\"graph-data\" type=\"application/json\">");
    html.push_str(&json.replace('<', "\\u003c"));
    html.push_str("</script>\n");

    html.push_str("<script>");
    html.push_str(SCRIPT);
    html.push_str("</script>\n</body>\n</html>\n");
    html
}

/// Light/dark styling. The SVG fills the viewport below the header; colors come
/// from CSS variables so the graph tracks the OS theme.
const STYLE: &str = "\
:root {
  color-scheme: light dark;
  --bg: #ffffff; --fg: #1a1a1a; --muted: #6b7280;
  --node-bg: #eef2ff; --node-stroke: #6366f1; --node-fg: #1e1b4b;
  --edge: #9ca3af; --edge-hot: #ef4444; --edge-label: #374151;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0f1115; --fg: #e5e7eb; --muted: #9ca3af;
    --node-bg: #1e293b; --node-stroke: #818cf8; --node-fg: #e0e7ff;
    --edge: #4b5563; --edge-hot: #f87171; --edge-label: #cbd5e1;
  }
}
* { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body { display: flex; flex-direction: column; background: var(--bg); color: var(--fg);
  font: 15px/1.5 system-ui, sans-serif; }
header { padding: 0.75rem 1.25rem; border-bottom: 1px solid rgba(128,128,128,0.3); }
header h1 { margin: 0; font-size: 1.1rem; }
header p { margin: 0.2rem 0 0; color: var(--muted); font-size: 0.8rem; }
#graph { flex: 1 1 auto; width: 100%; min-height: 0; touch-action: none; cursor: grab; }
.empty { padding: 1.25rem; color: var(--muted); }
.node rect { fill: var(--node-bg); stroke: var(--node-stroke); stroke-width: 1.5; }
.node text { fill: var(--node-fg); font-size: 12px; font-weight: 600; pointer-events: none; }
.edge path.line { fill: none; stroke: var(--edge); stroke-width: 1.5; }
.edge path.hit { fill: none; stroke: transparent; stroke-width: 12; }
/* dagre reserves a non-overlapping slot for every edge label, so they're all
   shown. The halo (stroke under fill, in the page background color) keeps them
   legible where a label sits over an edge. */
.edge .edge-label { fill: var(--edge-label); font-size: 11px;
  paint-order: stroke; stroke: var(--bg); stroke-width: 3px; stroke-linejoin: round;
  pointer-events: none; }
.edge.hot path.line { stroke: var(--edge-hot); stroke-width: 2.5; }
.edge.hot .edge-label { fill: var(--edge-hot); font-weight: 600; }
/* Each rule-connected cluster gets a subtle container, making the number of
   independent systems in the home visible at a glance. */
.cluster-box { fill: rgba(128,128,160,0.05); stroke: rgba(128,128,128,0.30);
  stroke-dasharray: 5 4; pointer-events: none; }
/* Devices no rule references, parked in the tray below the clusters. */
.node.orphan rect { stroke: var(--muted); stroke-dasharray: 4 3; }
.tray-label { fill: var(--muted); font-size: 11px; font-style: italic; }
/* Rule-count badge on devices touched by 2+ rules. */
.badge circle { fill: var(--node-stroke); }
.badge text { fill: var(--bg); font-size: 10px; font-weight: 700; pointer-events: none; }
/* Click-to-trace: everything outside the clicked device's downstream reach dims;
   reached edges highlight. */
.node, .edge { transition: opacity 0.15s; }
.node.dim, .edge.dim { opacity: 0.12; }
.edge.hi path.line { stroke: var(--node-stroke); stroke-width: 2.5; }
.edge.hi .edge-label { fill: var(--node-fg); font-weight: 600; }
.node.focus rect { stroke: var(--edge-hot); stroke-width: 3; }
.node { cursor: pointer; }
/* Hover tooltip: the rule's parameters. */
#tip { position: fixed; z-index: 10; max-width: 22rem; padding: 0.5rem 0.7rem;
  border: 1px solid rgba(128,128,128,0.35); border-radius: 8px;
  background: var(--bg); color: var(--fg); font-size: 12px; line-height: 1.45;
  white-space: pre-line; box-shadow: 0 4px 16px rgba(0,0,0,0.25);
  pointer-events: none; opacity: 0; transition: opacity 0.08s; }
#tip.show { opacity: 1; }
#tip b { color: var(--node-stroke); }
";
/// The renderer. Vanilla JS reading the embedded `#graph-data` JSON, with dagre
/// (served locally at /dagre.js) doing the layout. Built around the questions a
/// config author asks of the graph:
///
/// * **Clusters** — the graph is split into rule-connected components; each lays
///   out independently (dagre, top-to-bottom ranks) inside its own outlined box,
///   and the boxes are shelf-packed to approximate the viewport's aspect ratio
///   instead of one endless strip. Disconnected "smart rooms" are visibly
///   separate systems.
/// * **Orphan tray** — devices no rule references are parked in a labeled tray at
///   the bottom, so inert devices are impossible to miss.
/// * **Rule badges** — a device touched by 2+ distinct rules gets a count badge.
/// * **Click-to-trace** — clicking a device highlights its downstream reach
///   (every rule edge and device transitively triggerable from it) and dims the
///   rest; click the background or the device again to clear.
///
/// Layout is computed once (static); the page handles pan / wheel-zoom /
/// double-click-to-fit and hover tooltips (a rule's parameters).
const SCRIPT: &str = r#"
(function () {
  var NS = "http://www.w3.org/2000/svg";
  var data = JSON.parse(document.getElementById("graph-data").textContent);
  var svg = document.getElementById("graph");
  function esc(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); }

  // Hover tooltip (a rule's parameters).
  var tip = document.createElement("div"); tip.id = "tip"; document.body.appendChild(tip);
  function showTip(label, detail, ev) { tip.innerHTML = "<b>" + esc(label) + "</b>\n" + esc(detail); tip.classList.add("show"); moveTip(ev); }
  function moveTip(ev) {
    var pad = 14, x = ev.clientX + pad, y = ev.clientY + pad;
    if (x + tip.offsetWidth > window.innerWidth) x = ev.clientX - pad - tip.offsetWidth;
    if (y + tip.offsetHeight > window.innerHeight) y = ev.clientY - pad - tip.offsetHeight;
    tip.style.left = x + "px"; tip.style.top = y + "px";
  }
  function hideTip() { tip.classList.remove("show"); }

  // ---- Split into rule-connected components (union-find), so each cluster can
  // lay out on its own and disconnected systems are visibly separate.
  var parent = {};
  data.nodes.forEach(function (n) { parent[n.id] = n.id; });
  function find(x) { while (parent[x] !== x) { parent[x] = parent[parent[x]]; x = parent[x]; } return x; }
  data.edges.forEach(function (e) {
    if (parent[e.from] === undefined || parent[e.to] === undefined) return;
    parent[find(e.from)] = find(e.to);
  });
  var comps = {};
  data.nodes.forEach(function (n) { var r = find(n.id); (comps[r] = comps[r] || { nodes: [], edges: [] }).nodes.push(n); });
  data.edges.forEach(function (e) {
    if (parent[e.from] === undefined || parent[e.to] === undefined) return;
    comps[find(e.from)].edges.push(e);
  });
  var clusters = [], orphans = [];
  Object.keys(comps).forEach(function (r) {
    var c = comps[r];
    if (c.edges.length) clusters.push(c); else orphans.push.apply(orphans, c.nodes);
  });

  // Distinct rules touching each device (a rule may contribute several edges, so
  // count unique rule names) — drives the count badge.
  var rulesAt = {};
  data.edges.forEach(function (e) {
    (rulesAt[e.from] = rulesAt[e.from] || new Set()).add(e.label);
    (rulesAt[e.to] = rulesAt[e.to] || new Set()).add(e.label);
  });

  var NH = 30;
  function nodeW(n) { return Math.max(56, n.label.length * 8.2 + 22); }

  // ---- Lay out each cluster independently: layered top-to-bottom, with dagre
  // reserving non-overlapping slots for edge labels.
  clusters.forEach(function (c) {
    var g = new dagre.graphlib.Graph({ multigraph: true, directed: true });
    g.setGraph({ rankdir: "TB", nodesep: 40, ranksep: 60, edgesep: 14, marginx: 18, marginy: 18 });
    g.setDefaultEdgeLabel(function () { return {}; });
    c.nodes.forEach(function (n) { g.setNode(String(n.id), { label: n.label, width: nodeW(n), height: NH }); });
    c.edges.forEach(function (e, i) {
      g.setEdge(String(e.from), String(e.to),
        { label: e.label, detail: e.detail, width: e.label.length * 6.0 + 10, height: 16, labelpos: "c" }, "e" + i);
    });
    dagre.layout(g);
    c.g = g; c.w = g.graph().width || 1; c.h = g.graph().height || 1;
  });

  // ---- Shelf-pack the cluster boxes into rows targeting the viewport's aspect
  // ratio, so the scene approximates the screen shape instead of one long strip.
  var cw = svg.clientWidth || 800, ch = svg.clientHeight || 600;
  var GAP = 44;
  clusters.sort(function (a, b) { return b.h - a.h || b.w - a.w; });
  var area = 0, widest = 0;
  clusters.forEach(function (c) { area += (c.w + GAP) * (c.h + GAP); widest = Math.max(widest, c.w); });
  var targetW = Math.max(widest, Math.sqrt(area * (cw / Math.max(ch, 1))));
  var px = 0, py = 0, rowH = 0, sceneW = 0;
  clusters.forEach(function (c) {
    if (px > 0 && px + c.w > targetW) { px = 0; py += rowH + GAP; rowH = 0; }
    c.ox = px; c.oy = py;
    rowH = Math.max(rowH, c.h); px += c.w + GAP;
    sceneW = Math.max(sceneW, c.ox + c.w);
  });
  var sceneH = py + rowH;

  var root = document.createElementNS(NS, "g");
  svg.appendChild(root);
  var defs = document.createElementNS(NS, "defs");
  defs.innerHTML =
    '<marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">' +
    '<path d="M0,0 L10,5 L0,10 z" fill="var(--edge)"></path></marker>';
  svg.insertBefore(defs, root);

  function pathD(points) {
    return points.map(function (p, i) { return (i ? "L" : "M") + p.x + " " + p.y; }).join(" ");
  }

  // ---- Render. Track per-node and per-edge elements plus outgoing adjacency for
  // the click-to-trace interaction.
  var nodeEls = {}, edgeEls = [], outAdj = {};
  var panning = false, panStart = null, movedDist = 0;

  function buildNode(parentG, id, label, w, h, x, y, orphan) {
    var grp = document.createElementNS(NS, "g");
    grp.setAttribute("class", orphan ? "node orphan" : "node");
    grp.setAttribute("transform", "translate(" + x + "," + y + ")");
    var rect = document.createElementNS(NS, "rect");
    rect.setAttribute("x", -w / 2); rect.setAttribute("y", -h / 2);
    rect.setAttribute("width", w); rect.setAttribute("height", h); rect.setAttribute("rx", 6);
    var text = document.createElementNS(NS, "text");
    text.setAttribute("text-anchor", "middle"); text.setAttribute("dy", "0.32em");
    text.textContent = label;
    grp.appendChild(rect); grp.appendChild(text);
    // Badge: how many distinct rules touch this device (shown at 2+).
    var count = rulesAt[id] ? rulesAt[id].size : 0;
    if (count >= 2) {
      var badge = document.createElementNS(NS, "g"); badge.setAttribute("class", "badge");
      badge.setAttribute("transform", "translate(" + (w / 2 - 1) + "," + (-h / 2 + 1) + ")");
      var circ = document.createElementNS(NS, "circle"); circ.setAttribute("r", 9);
      var num = document.createElementNS(NS, "text");
      num.setAttribute("text-anchor", "middle"); num.setAttribute("dy", "0.32em");
      num.textContent = count;
      badge.appendChild(circ); badge.appendChild(num); grp.appendChild(badge);
    }
    grp.addEventListener("click", function (ev) {
      ev.stopPropagation();
      if (movedDist < 5) toggleFocus(id);
    });
    nodeEls[id] = grp;
    parentG.appendChild(grp);
  }

  clusters.forEach(function (c) {
    var cg = document.createElementNS(NS, "g");
    cg.setAttribute("transform", "translate(" + c.ox + "," + c.oy + ")");
    root.appendChild(cg);
    var box = document.createElementNS(NS, "rect");
    box.setAttribute("class", "cluster-box");
    box.setAttribute("x", 0); box.setAttribute("y", 0);
    box.setAttribute("width", c.w); box.setAttribute("height", c.h); box.setAttribute("rx", 10);
    cg.appendChild(box);
    // Edges first so node boxes paint on top.
    c.g.edges().forEach(function (o) {
      var e = c.g.edge(o);
      var grp = document.createElementNS(NS, "g"); grp.setAttribute("class", "edge");
      var d = pathD(e.points);
      var hit = document.createElementNS(NS, "path"); hit.setAttribute("class", "hit"); hit.setAttribute("d", d);
      var line = document.createElementNS(NS, "path"); line.setAttribute("class", "line"); line.setAttribute("d", d);
      line.setAttribute("marker-end", "url(#arrow)");
      var label = document.createElementNS(NS, "text"); label.setAttribute("class", "edge-label");
      label.setAttribute("text-anchor", "middle"); label.setAttribute("dy", "0.32em");
      label.setAttribute("x", e.x); label.setAttribute("y", e.y); label.textContent = e.label;
      grp.appendChild(hit); grp.appendChild(line); grp.appendChild(label); cg.appendChild(grp);
      grp.addEventListener("pointerenter", function (ev) { grp.classList.add("hot"); showTip(e.label, e.detail, ev); });
      grp.addEventListener("pointermove", function (ev) { moveTip(ev); });
      grp.addEventListener("pointerleave", function () { grp.classList.remove("hot"); hideTip(); });
      var idx = edgeEls.length;
      edgeEls.push({ grp: grp, from: +o.v, to: +o.w });
      (outAdj[+o.v] = outAdj[+o.v] || []).push({ to: +o.w, idx: idx });
    });
    c.g.nodes().forEach(function (id) {
      var n = c.g.node(id);
      buildNode(cg, +id, n.label, n.width, n.height, n.x, n.y, false);
    });
  });

  // ---- Orphan tray: devices no rule references, parked below the clusters.
  if (orphans.length) {
    var labelY = sceneH + GAP;
    var trayLabel = document.createElementNS(NS, "text");
    trayLabel.setAttribute("class", "tray-label");
    trayLabel.setAttribute("x", 0); trayLabel.setAttribute("y", labelY);
    trayLabel.textContent = "unconnected — no rule references these";
    root.appendChild(trayLabel);
    var tx2 = 0, rowY = labelY + 12, trayW = Math.max(targetW, sceneW, 300);
    orphans.forEach(function (n) {
      var w = nodeW(n);
      if (tx2 > 0 && tx2 + w > trayW) { tx2 = 0; rowY += NH + 16; }
      buildNode(root, n.id, n.label, w, NH, tx2 + w / 2, rowY + NH / 2, true);
      tx2 += w + 18;
      sceneW = Math.max(sceneW, tx2 - 18);
    });
    sceneH = rowY + NH;
  }
  var GW = Math.max(sceneW, 1), GH = Math.max(sceneH, 1);

  // ---- Click-to-trace: BFS the downstream reach of a device (what can this
  // trigger, transitively?), highlight it, dim everything else.
  var focusId = null;
  function clearFocus() {
    focusId = null;
    edgeEls.forEach(function (e) { e.grp.classList.remove("dim", "hi"); });
    Object.keys(nodeEls).forEach(function (k) { nodeEls[k].classList.remove("dim", "focus"); });
  }
  function toggleFocus(id) {
    if (focusId === id) { clearFocus(); return; }
    clearFocus();
    focusId = id;
    var reach = new Set([id]), q = [id], reachedEdges = new Set();
    while (q.length) {
      var v = q.pop();
      (outAdj[v] || []).forEach(function (a) {
        reachedEdges.add(a.idx);
        if (!reach.has(a.to)) { reach.add(a.to); q.push(a.to); }
      });
    }
    edgeEls.forEach(function (e, i) {
      e.grp.classList.toggle("dim", !reachedEdges.has(i));
      e.grp.classList.toggle("hi", reachedEdges.has(i));
    });
    Object.keys(nodeEls).forEach(function (k) { nodeEls[k].classList.toggle("dim", !reach.has(+k)); });
    nodeEls[id].classList.remove("dim");
    nodeEls[id].classList.add("focus");
  }
  svg.addEventListener("click", function () { if (movedDist < 5) clearFocus(); });

  // ---- View transform: pan / wheel-zoom / fit.
  var tx = 0, ty = 0, scale = 1;
  function measure() { cw = svg.clientWidth; ch = svg.clientHeight; }
  function apply() { root.setAttribute("transform", "translate(" + tx + "," + ty + ") scale(" + scale + ")"); }
  function fit() {
    var pad = 28;
    scale = Math.min(2.2, Math.max(0.12, Math.min((cw - 2 * pad) / GW, (ch - 2 * pad) / GH)));
    tx = (cw - GW * scale) / 2; ty = (ch - GH * scale) / 2;
  }
  svg.addEventListener("pointerdown", function (ev) { panning = true; movedDist = 0; panStart = { x: ev.clientX, y: ev.clientY }; });
  svg.addEventListener("pointermove", function (ev) {
    if (!panning) return;
    var dx = ev.clientX - panStart.x, dy = ev.clientY - panStart.y;
    movedDist += Math.abs(dx) + Math.abs(dy);
    tx += dx; ty += dy;
    panStart = { x: ev.clientX, y: ev.clientY };
    apply();
  });
  window.addEventListener("pointerup", function () { panning = false; });
  svg.addEventListener("wheel", function (ev) {
    ev.preventDefault();
    var r = svg.getBoundingClientRect(), mx = ev.clientX - r.left, my = ev.clientY - r.top;
    var wx = (mx - tx) / scale, wy = (my - ty) / scale;
    scale = Math.min(4, Math.max(0.12, scale * Math.exp(-ev.deltaY * 0.001)));
    tx = mx - wx * scale; ty = my - wy * scale; apply();
  }, { passive: false });
  svg.addEventListener("dblclick", function () { measure(); fit(); apply(); });
  window.addEventListener("resize", function () { measure(); fit(); apply(); });

  measure(); fit(); apply();
})();
"#;

/// The dagre layered-graph layout library (vendored, minified), embedded so the
/// binary stays self-contained. Served at `/dagre.js` and loaded by the page —
/// fully offline, no CDN. dagre computes node ranks/positions and, crucially,
/// reserves non-overlapping slots for edge labels; the page just renders its output.
const DAGRE_JS: &str = include_str!("../assets/dagre.min.js");

/// Start the GUI server on `port`, on its own thread. Returns once the socket is
/// bound (so a bind failure is reported synchronously and can be made fatal at
/// startup); the thread then serves requests for the life of the process.
///
/// Binds `0.0.0.0` so a headless home server can be viewed from another machine
/// on the LAN — the common deployment. The graph is read-only.
pub fn serve(snapshot: Arc<GraphSnapshot>, port: u16) -> Result<(), String> {
    let server = tiny_http::Server::http(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    thread::spawn(move || {
        for request in server.incoming_requests() {
            // Render lazily per request from the shared snapshot — cheap, and it
            // keeps serving the current graph if a future reload swaps the `Arc`.
            match request.url() {
                "/" => {
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .expect("static header is valid");
                    let resp = tiny_http::Response::from_string(render_page(&snapshot))
                        .with_header(header);
                    let _ = request.respond(resp);
                }
                "/dagre.js" => {
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/javascript; charset=utf-8"[..],
                    )
                    .expect("static header is valid");
                    let resp = tiny_http::Response::from_string(DAGRE_JS).with_header(header);
                    let _ = request.respond(resp);
                }
                _ => {
                    let resp = tiny_http::Response::from_string("not found").with_status_code(404);
                    let _ = request.respond(resp);
                }
            }
        }
    });
    Ok(())
}
