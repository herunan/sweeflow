import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

// Sweeflow's client, as the plugin sees it: whether it's installed and
// running, its panel State (the JSON property on the session bus, read with
// busctl whenever `gdbus monitor` reports a change), and the calls back
// (Do, Set, Rename), sent with busctl. Nothing here installs, downloads or
// changes the system: the only other commands are `which sweeflow`,
// `systemctl --user show|start sweeflow.service` and `sweeflow open`.
Item {
  id: root
  visible: false

  // The client's program, as found on PATH.
  property string program: "sweeflow"
  readonly property string unit: "sweeflow.service"

  // Assumed there until `which` says otherwise, so the panel doesn't flash
  // the install steps while the shell starts.
  property bool installed: true
  property bool checked: false
  // The user service exists (it's made the first time Sweeflow is opened).
  property bool unitLoaded: false
  property bool running: false
  property bool starting: false

  property string stateJson: ""
  property var st: ({})

  readonly property bool paired: running && !!st.paired
  readonly property bool connected: running && !!st.connected
  // Stop was chosen: no connection to the Mac.
  readonly property bool stopped: running && !!st.offline
  // Always there: the Mac's, or (while it's away) the last known.
  readonly property var settings: st.settings || ({ exclude: [] })
  readonly property bool streaming: connected && !!settings.enabled
  readonly property string macName: st.mac || "Mac"
  readonly property var placement: st.placement || ({})
  readonly property bool elsewhere: st.elsewhere === true
  readonly property bool canStart: st.canStart !== false
  readonly property var keys: st.keys || ({ open: "Super+Alt+O", pause: "Super+Alt+Y", stop: "Super+Alt+Z", menu: "Super+Alt+U" })
  readonly property var updates: st.updates || ({})

  // The lists, each through a text of it: a property only changes when its
  // value does, so what is drawn from them is built again only when they
  // really differ, not at every new State.
  readonly property string actionsKey: paired ? (st.actions || []).join(",") : ""
  readonly property var streamActions: actionsKey === "" ? [] : actionsKey.split(",")
  readonly property string appsJson: JSON.stringify(st.apps || [])
  readonly property var apps: JSON.parse(appsJson)
  readonly property string macsJson: JSON.stringify(Model.macsOf(st))
  readonly property var macs: JSON.parse(macsJson)
  readonly property string excludeJson: JSON.stringify(settings.exclude || [])
  readonly property var exclude: JSON.parse(excludeJson)
  readonly property var activeMac: {
    for (var i = 0; i < macs.length; i++) if (macs[i].active) return macs[i]
    return null
  }
  readonly property string activeState: paired ? Model.macStateOf(activeMac, st, connected) : ""
  // The client words it (the same line as in its own window).
  readonly property string statusText: !installed ? "Not installed"
    : !running ? "Not running"
    : !paired ? "No Mac linked yet"
    : String(st.status || "")

  // Opened by `sweeflow open` or its shortcut: the time it was asked.
  readonly property real showAt: Number(st.showAt || 0)

  function macState(m) { return Model.macStateOf(m, st, connected) }

  function startBlocked(m) {
    return Model.startBlocked(m.name, m.online, m.active, m.busy, canStart, elsewhere)
  }

  // ---- calls to the client

  function call(member, signature, args) {
    if (!running) return
    // "--": a value starting with a dash (a Mac's new name) is still a value.
    Quickshell.execDetached(["busctl", "--user", "--", "call", Model.SERVICE, Model.OBJECT_PATH, Model.INTERFACE,
      member, signature].concat(args))
    followTimer.restart()
  }

  function act(action, arg) { call("Do", "ss", [String(action), String(arg || "")]) }
  function set(changes) { call("Set", "s", [JSON.stringify(changes)]) }
  function rename(fingerprint, name) { call("Rename", "ss", [String(fingerprint), String(name)]) }

  // Start the client: its user service, or (never opened yet, so no service)
  // Sweeflow itself, which sets the service up and opens Link a Mac.
  function start() {
    if (!installed || running) return
    starting = true
    if (unitLoaded) Quickshell.execDetached(["systemctl", "--user", "start", root.unit])
    else Quickshell.execDetached([root.program, "open"])
    startingTimer.restart()
    followTimer.restart()
  }

  // ---- reading

  function refresh() {
    if (!readProc.running) readProc.running = true
  }

  function checkInstalled() {
    if (!whichProc.running) {
      whichProc.command = ["which", root.program]
      whichProc.running = true
    }
    if (!unitProc.running) unitProc.running = true
  }

  function applyState(raw) {
    var state = Model.parseProperty(raw)
    if (state === null) {
      gone()
      return
    }
    var text = JSON.stringify(state)
    running = true
    starting = false
    installed = true
    if (text === stateJson) return
    stateJson = text
    st = state
  }

  function gone() {
    var was = running
    running = false
    stateJson = ""
    st = ({})
    if (was || !checked) checkInstalled()
  }

  onProgramChanged: checkInstalled()
  Component.onCompleted: {
    checkInstalled()
    refresh()
  }

  // One line per signal from the client, and one whenever its bus name gets
  // or loses its owner: each only says "look again".
  Process {
    id: watchProc
    command: ["gdbus", "monitor", "--session", "--dest", Model.SERVICE, "--object-path", Model.OBJECT_PATH]
    running: true
    stdout: SplitParser {
      onRead: function(line) { changedTimer.restart() }
    }
    onExited: watchRestart.restart()
  }

  Timer {
    id: watchRestart
    interval: 5000
    onTriggered: watchProc.running = true
  }

  // Several signals in a row: one read.
  Timer {
    id: changedTimer
    interval: 60
    onTriggered: root.refresh()
  }

  // After a call or a start, in case its signal is missed.
  Timer {
    id: followTimer
    interval: 700
    onTriggered: root.refresh()
  }

  Timer {
    id: startingTimer
    interval: 15000
    onTriggered: root.starting = false
  }

  // A slow look anyway: the monitor could have died, Sweeflow could have been
  // installed or removed since.
  Timer {
    interval: root.starting ? 1000 : 10000
    repeat: true
    running: true
    onTriggered: {
      root.refresh()
      if (!root.running) root.checkInstalled()
    }
  }

  Process {
    id: readProc
    command: ["busctl", "--user", "--json=short", "get-property", Model.SERVICE, Model.OBJECT_PATH, Model.INTERFACE, "State"]
    stdout: StdioCollector { id: readOut; waitForEnd: true }
    stderr: StdioCollector {}
    onExited: function(exitCode) {
      if (exitCode === 0) root.applyState(readOut.text)
      else root.gone()
    }
  }

  Process {
    id: whichProc
    stdout: StdioCollector {}
    stderr: StdioCollector {}
    onExited: function(exitCode) {
      root.installed = exitCode === 0 || root.running
      root.checked = true
    }
  }

  Process {
    id: unitProc
    command: ["systemctl", "--user", "show", root.unit, "--property=LoadState", "--value"]
    stdout: StdioCollector { id: unitOut; waitForEnd: true }
    stderr: StdioCollector {}
    onExited: function(exitCode) {
      root.unitLoaded = exitCode === 0 && String(unitOut.text).trim() === "loaded"
    }
  }
}
