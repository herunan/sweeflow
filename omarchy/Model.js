// Pure logic for the Sweeflow panel: no Qt here. The same rules as the Plasma
// widget (linux/plasmoid/dev.sweeflow.panel/contents/ui/main.qml) and
// Sweeflow's own window (linux/src/panelwin.rs), which read the same State.

var SERVICE = "dev.sweeflow.Client"
var OBJECT_PATH = "/dev/sweeflow/Panel"
var INTERFACE = "dev.sweeflow.Panel"

// Sweeflow's own colours for where a Mac stands (tray.rs STATE_DOTS).
var VIOLET = "#8b5cf6"
var GREEN = "#30a46c"
var ORANGE = "#e8890c"
var GREY = "#9a96a3"

var ACTION_LABELS = { start: "Start", pause: "Pause", resume: "Resume", stop: "Stop" }
// Nerd Font signs, as a player shows them.
var ACTION_GLYPHS = { start: "", pause: "", resume: "", stop: "" }
var STATE_WORDS = {
  streaming: "Streaming", paused: "Paused", available: "Available",
  offline: "Not online", locked: "Locked", busy: "Streaming elsewhere"
}
var VIA_WORDS = { lan: "Local", tailscale: "Remote", vm: "VM", internet: "Remote" }

// The README's "Arch, CachyOS and Omarchy" steps, with Omarchy's install line.
// Only shown and copied: the plugin never runs them.
var INSTALL_STEPS = [
  "curl -fsSL https://github.com/herunan/sweeflow/releases/latest/download/sweeflow.asc | sudo pacman-key --add -",
  "sudo pacman-key --lsign-key E1D31233BB01463B575ACD5E387237FAEFBBB7B8",
  "printf '\\n[sweeflow]\\nServer = https://github.com/herunan/sweeflow/releases/latest/download\\n' | sudo tee -a /etc/pacman.conf",
  "sudo pacman -Sy sweeflow"
]

function installText() {
  return INSTALL_STEPS.join("\n") + "\n"
}

// `busctl --json=short get-property` output -> the State object, or null.
function parseProperty(raw) {
  try {
    var outer = JSON.parse(String(raw || ""))
    if (!outer || typeof outer.data !== "string") return null
    var state = JSON.parse(outer.data)
    return state && typeof state === "object" ? state : null
  } catch (e) {
    return null
  }
}

function macsOf(st) {
  return (st.macs || []).map(function(m) {
    return {
      name: String(m.name || ""), fingerprint: String(m.fingerprint || ""), active: !!m.active,
      // true, false, or null while not known yet
      online: m.online === undefined ? null : m.online,
      busy: !!m.busy, locked: !!m.locked,
      via: String(m.via || ""), address: String(m.address || "")
    }
  })
}

// Where a linked Mac stands: streaming, paused, locked, busy, available,
// offline, or "" while not known yet.
function macStateOf(m, st, connected) {
  if (!m) return ""
  if (m.active && connected)
    return st.locked ? "locked" : (st.settings && st.settings.enabled === false ? "paused" : "streaming")
  if (m.online === null || m.online === undefined) return ""
  if (m.online === false) return "offline"
  if (m.busy || (m.active && st.elsewhere === true)) return "busy"
  if (m.locked || (m.active && st.locked)) return "locked"
  return "available"
}

function stateInk(state) {
  return state === "streaming" || state === "paused" ? VIOLET
    : state === "available" ? GREEN
    : state === "busy" || state === "locked" ? ORANGE
    : GREY
}

function stateHollow(state) {
  return state === "" || state === "paused" || state === "offline"
}

// Why Start can't be pressed for a Mac ("" if it can).
function startBlocked(name, online, active, busy, canStart, elsewhere) {
  if ((active && !canStart && elsewhere) || busy) return "Stop streaming elsewhere first"
  if (online === false) return name + " is not online"
  if (online === null || online === undefined) return "Checking whether " + name + " is online"
  return ""
}

// How a Mac is reached, in a few words: "Remote: 100.64.0.3".
function viaLine(m) {
  if (!m) return ""
  var word = VIA_WORDS[m.via]
  if (!word) return ""
  var where = String(m.address || "").trim()
  return where ? word + ": " + where : word
}

function withKey(text, key) {
  return key ? text + " (" + key + ")" : text
}

function actionTip(action, keys) {
  keys = keys || {}
  if (action === "start") return withKey("Start streaming", keys.stop)
  if (action === "pause") return withKey("Pause streaming", keys.pause)
  if (action === "resume") return withKey("Resume streaming", keys.pause)
  if (action === "stop") return withKey("Stop streaming", keys.stop)
  return ""
}

// The heading an app is listed under. Not streaming, the Mac doesn't say
// which apps run: one list.
function appGroup(app, streaming) {
  return app.pinned ? "Pinned" : !streaming ? "Apps" : app.state > 0 ? "Running" : "Not running"
}

function filterApps(apps, query) {
  var q = String(query || "").trim().toLowerCase()
  if (q === "") return apps
  return apps.filter(function(a) { return String(a.name || "").toLowerCase().indexOf(q) >= 0 })
}

// The apps as rows, each knowing its heading (shown on the first of a group).
function appRows(apps, query, streaming) {
  var list = filterApps(apps, query)
  var rows = []
  var last = null
  for (var i = 0; i < list.length; i++) {
    var group = appGroup(list[i], streaming)
    rows.push({
      id: String(list[i].id || ""), name: String(list[i].name || ""), icon: String(list[i].icon || ""),
      running: list[i].state > 0, pinned: !!list[i].pinned,
      heading: group !== last ? group : ""
    })
    last = group
  }
  return rows
}

// The exclude list after an app's switch is flipped.
function excludeWith(exclude, id, shown) {
  var out = (exclude || []).filter(function(b) { return b !== id })
  if (!shown) out.push(id)
  return out
}
