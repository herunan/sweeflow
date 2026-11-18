// The Sweeflow tray widget: the client's state over D-Bus (src/panel.rs), shown as a popup.
//
// Laid out as Sweeflow's own window is on other desktops (src/panelwin.rs), in Plasma's look:
// the header names the Mac, says where streaming stands and holds its actions (Pause / Resume,
// Start, Stop) as signs; below, three tabs: Apps, Settings and Macs; Quit Sweeflow at the
// bottom. Settings can always be changed: with the Mac away the client keeps them and applies
// them once it's back.

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.components as PlasmaComponents3
import org.kde.plasma.extras as PlasmaExtras
import org.kde.plasma.plasma5support as P5Support
import org.kde.kirigami as Kirigami
import org.kde.plasma.workspace.dbus as DBus

PlasmoidItem {
    id: root

    readonly property string service: "dev.sweeflow.Client"
    readonly property string objectPath: "/dev/sweeflow/Panel"
    readonly property string ifaceName: "dev.sweeflow.Panel"

    DBus.DBusServiceWatcher {
        id: watcher
        busType: DBus.BusType.Session
        watchedService: root.service
    }

    DBus.Properties {
        id: props
        busType: DBus.BusType.Session
        // Re-read from scratch whenever the client (re)starts.
        service: watcher.registered ? root.service : ""
        path: root.objectPath
        iface: root.ifaceName
    }

    readonly property bool running: watcher.registered
    readonly property var st: {
        const json = props.properties["State"];
        try { return json ? JSON.parse(json) : {}; } catch (e) { return {}; }
    }
    readonly property bool paired: running && !!st.paired
    readonly property bool connected: running && !!st.connected
    // Stop was chosen: no connection to the Mac (st.online still says whether it's there).
    readonly property bool stopped: running && !!st.offline
    // Always there: the Mac's, or (while it's away) the last known with the changes made since.
    readonly property var settings: st.settings || ({ exclude: [] })
    readonly property bool streaming: connected && !!settings.enabled
    // Connected, but the Mac's screen is locked: its windows are paused until it's unlocked.
    readonly property bool locked: streaming && !!st.locked
    readonly property string macName: st.mac || "Mac"
    readonly property var placement: st.placement || {}

    // Setup's shortcut for this widget (Cmd+Opt+O): given here, Plasma takes it at once, with no
    // restart of its shell. Not over one the widget has already.
    readonly property var setKey: st.setKey || null
    function takeKey() {
        // For this widget, or ("*") whichever comes up first.
        if (!setKey || (setKey.id !== "*" && String(setKey.id) !== String(Plasmoid.id))) return;
        if (String(Plasmoid.globalShortcut) === "") Plasmoid.globalShortcut = setKey.key;
        act("key-set");
    }
    onSetKeyChanged: takeKey()
    Component.onCompleted: takeKey()

    // Opened from the app menu or by the key (sweeflow open): pop up.
    readonly property real showAt: st.showAt || 0
    onShowAtChanged: if (showAt > 0 && Math.abs(Date.now() - showAt) < 10000) root.expanded = true

    // The lists, each through a text of it: a property only changes when its value does, so what
    // is drawn from them is built again only when they really differ, not at every new State (a
    // button rebuilt under the pointer loses the click, a list its place, a rename the name
    // being typed).
    readonly property string actionsKey: paired ? (st.actions || []).join(",") : ""
    // What can be done about the Mac in use (Do actions), as the client lists it: pause and stop
    // while streaming, resume and stop while paused, start while stopped, stop while waiting.
    readonly property var streamActions: actionsKey === "" ? [] : actionsKey.split(",")
    // The Mac is streaming to another computer: Start is shown but can't be chosen until it's
    // stopped there.
    readonly property bool elsewhere: st.elsewhere === true
    readonly property bool canStart: st.canStart !== false
    // Why Start can't be pressed for this Mac ("" if it can): it's streaming to another computer,
    // or it can't be reached (or isn't known to be reachable yet). `online`: true, false or null.
    // Where a linked Mac stands, as its dot and line in the list (and the header, for the Mac in
    // use): streaming, paused, locked, busy, available, offline, or "" while not known yet.
    function macStateOf(m) {
        if (m.active && connected) return st.locked ? "locked" : st.settings && st.settings.enabled === false ? "paused" : "streaming"
        if (m.online === null || m.online === undefined) return ""
        if (m.online === false) return "offline"
        if (m.busy || (m.active && elsewhere)) return "busy"
        if (m.locked || (m.active && st.locked)) return "locked"
        return "available"
    }
    function macInk(state) {
        return state === "streaming" || state === "paused" ? root.violet : state === "available" ? "#30a46c"
            : state === "busy" || state === "locked" ? "#e8890c" : Kirigami.Theme.disabledTextColor
    }
    readonly property string activeState: { const a = linkedMacs.find(m => m.active); return a ? macStateOf(a) : "" }

    function startBlocked(name, online, active, busy) {
        if ((active && !canStart && elsewhere) || busy) return "Stop streaming elsewhere first"
        if (online === false) return name + " is not online"
        if (online === null || online === undefined) return "Checking whether " + name + " is online"
        return ""
    }
    readonly property string appsJson: JSON.stringify(st.apps || [])
    readonly property var allApps: JSON.parse(appsJson)
    readonly property string macsJson: JSON.stringify((st.macs || []).map(m => ({
        name: m.name, fingerprint: m.fingerprint, active: !!m.active,
        // true, false, or null while not known yet.
        online: m.online === undefined ? null : m.online,
        // Streaming to another computer (as it says on the network).
        busy: !!m.busy,
        locked: !!m.locked,
        // How it's reached (lan, tailscale, vm, internet, or ""), and at what address.
        via: m.via || "", address: m.address || "",
    })))
    // Sweeflow's violet: every highlight, and the streaming dot.
    readonly property color violet: "#8b5cf6"

    // A shortcut as keycaps, one per key: "Super+Alt+O" → [Super] [Alt] [O], "⌘⌥O" → [⌘] [⌥] [O]
    // (keys.rs caps); none: a dim "none".
    function keyCaps(shown) {
        shown = (shown || "").trim();
        if (shown === "") return [];
        if (shown.indexOf("+") >= 0 && shown.length > 1 && shown.indexOf(" ") < 0) {
            const parts = shown.split("+").filter(p => p !== "");
            if (shown.endsWith("++")) parts.push("+");
            return parts;
        }
        let i = 0;
        while (i < shown.length && "⌃⌥⇧⌘".indexOf(shown[i]) >= 0) i++;
        if (i === 0) return [shown];
        const named = { "⌘": "⌘ Cmd", "⌥": "⌥ Opt", "⌃": "⌃ Ctrl", "⇧": "⇧ Shift" };
        const out = shown.slice(0, i).split("").map(c => named[c] || c);
        const rest = shown.slice(i).trim();
        if (rest !== "") out.push(rest);
        return out;
    }
    component KeyCaps: Row {
        property var caps: []
        id: capsRow
        property int fontSize: 12
        spacing: 3
        Repeater {
            model: parent.caps.length ? parent.caps : ["none"]
            delegate: Rectangle {
                required property string modelData
                width: Math.max(height, capText.implicitWidth + 10)
                height: capText.implicitHeight + 4
                radius: Kirigami.Units.cornerRadius
                color: Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, 0.1)
                PlasmaComponents3.Label { id: capText; anchors.centerIn: parent; text: modelData; font.pixelSize: capsRow.fontSize; opacity: modelData === "none" ? 0.6 : 1 }
            }
        }
    }

    // The pointer in the Mac shortcuts picture: the text's colour, edged in the background's.
    component Pointer: Canvas {
        width: 12
        height: 16
        onPaint: {
            const c = getContext("2d");
            c.reset();
            c.beginPath();
            c.moveTo(0.6, 0.6); c.lineTo(0.6, 13); c.lineTo(3.5, 10); c.lineTo(6, 15.4);
            c.lineTo(8, 14.5); c.lineTo(5.5, 9); c.lineTo(10, 9); c.closePath();
            c.fillStyle = Kirigami.Theme.textColor; c.fill();
            c.lineWidth = 1; c.strokeStyle = Kirigami.Theme.backgroundColor; c.stroke();
        }
    }
    readonly property var viaWords: ({ lan: "Local", tailscale: "Remote", vm: "VM", internet: "Remote" })
    // How a Mac is reached, in a few words: "Remote: 100.64.0.3".
    function viaLine(m) {
        const word = root.viaWords[m.via]
        if (!word) return ""
        const where = m.address
        return where.trim() ? word + ": " + where.trim() : word
    }
    readonly property var activeMac: linkedMacs.find(m => m.active) || null
    readonly property var linkedMacs: JSON.parse(macsJson)
    // The Mac being renamed, or asked about unlinking (fingerprints).
    property string editingMac: ""
    property string confirmingMac: ""
    // A Mac renamed or unlinked: its row is back to normal.
    onMacsJsonChanged: { editingMac = ""; confirmingMac = ""; }

    // The client words it (the same line as in its own window).
    readonly property string statusText: !running ? "Not running" : !paired ? "No Mac linked yet" : st.status || ""
    // The streaming actions, as they're named and drawn.
    readonly property var actionLabels: ({ start: "Start", pause: "Pause", resume: "Resume", stop: "Stop" })
    // What each sign does, said on hover.
    // The keys Plasma has now for open / pause / stop (the client reads them: "Super+Alt+P", or as
    // Plasma writes them; "" for one with none), and the Global menu's (fixed).
    readonly property var shortcutKeys: st.keys || ({ open: "Super+Alt+O", pause: "Super+Alt+Y", stop: "Super+Alt+Z", menu: "Super+Alt+U" })
    function withKey(text, key) { return key ? text + " (" + key + ")" : text }
    readonly property var actionTips: ({ start: withKey("Start streaming", shortcutKeys.stop), pause: withKey("Pause streaming", shortcutKeys.pause),
                                         resume: withKey("Resume streaming", shortcutKeys.pause), stop: withKey("Stop streaming", shortcutKeys.stop) })
    readonly property var actionIcons: ({ start: "media-playback-start", pause: "media-playback-pause",
                                          resume: "media-playback-start", stop: "media-playback-stop" })

    function call(member, args) {
        DBus.SessionBus.asyncCall({
            service: root.service, path: root.objectPath, iface: root.ifaceName,
            // No signature: given one, Plasma drops plain JS strings and sends no arguments.
            member: member, arguments: args,
        });
    }
    function set(changes) { call("Set", [JSON.stringify(changes)]); }
    function act(action, arg) { call("Do", [action, arg || ""]); }
    function rename(fingerprint, name) { call("Rename", [fingerprint, name]); }

    P5Support.DataSource {
        id: shell
        engine: "executable"
        onNewData: (source) => disconnectSource(source)
    }

    // Not "sweeflow.svg": a file named like an icon in the theme gets that icon (the app icon).
    readonly property string logo: Qt.resolvedUrl("../icons/sweeflow-tray.svg")
    // The app icon (violet square): inside the popup. The plain logo is the tray icon.
    readonly property string appIcon: Qt.resolvedUrl("../icons/sweeflow-app.svg")
    Plasmoid.icon: logo
    Plasmoid.status: running ? PlasmaCore.Types.ActiveStatus : PlasmaCore.Types.PassiveStatus
    toolTipMainText: "Sweeflow"
    toolTipSubText: statusText

    compactRepresentation: MouseArea {
        onClicked: root.expanded = !root.expanded
        hoverEnabled: true
        Kirigami.Icon {
            id: trayLogo
            anchors.fill: parent
            source: root.logo
            // In the panel's text colour, like the icons beside it.
            isMask: true
            color: Kirigami.Theme.textColor
            active: parent.containsMouse
        }
        // Where the Mac in use stands, in the empty bottom-right corner (the sweet runs from
        // bottom left to top right): the main page's dot, as the other desktops' tray icon has it
        // (setup.rs tray_icon).
        Rectangle {
            readonly property real side: Math.min(trayLogo.paintedWidth, trayLogo.paintedHeight)
            readonly property bool hollow: ["", "paused", "offline"].includes(root.activeState)
            width: Math.max(4, Math.round(side * 0.28))
            height: width
            radius: width / 2
            x: (parent.width + side) / 2 - width
            y: (parent.height + side) / 2 - height
            color: hollow ? "transparent" : root.macInk(root.activeState)
            border.width: hollow ? Math.max(1.5, width * 0.22) : 0
            border.color: root.macInk(root.activeState)
        }
    }

    component ViaSign: PlasmaComponents3.Label {
        // How a Mac is reached, after its status: "· Local: 192.168.1.20", "· Remote:
        // 100.64.0.3", "· VM: 127.0.0.1" (no dot with no status before it).
        property var info: null
        property bool lead: true
        visible: text !== ""
        text: { const l = info ? root.viaLine(info) : ""; return l && lead ? "· " + l : l }
        font: Kirigami.Theme.smallFont
        opacity: 0.7
        elide: Text.ElideRight
    }
    // A streaming action as its sign, the way a player shows them. Its name stays as the text
    // (for screen readers) and is said on hover.
    component SignButton: PlasmaComponents3.ToolButton {
        id: sign
        property string tip: text
        display: QQC2.AbstractButton.IconOnly
        PlasmaComponents3.ToolTip.text: sign.tip
        PlasmaComponents3.ToolTip.visible: hovered && sign.tip !== ""
        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
    }

    // Something to do now, not a setting: what it is, a line about it, and its button (only the
    // button does it).
    component ActionRow: PlasmaComponents3.ItemDelegate {
        id: entry
        property string label
        property string note
        property string buttonText
        property bool available: true
        signal asked()
        Layout.fillWidth: true
        hoverEnabled: false
        down: false
        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing
            ColumnLayout {
                spacing: 0
                Layout.fillWidth: true
                PlasmaComponents3.Label { text: entry.label; elide: Text.ElideRight; Layout.fillWidth: true }
                PlasmaComponents3.Label {
                    text: entry.note
                    font: Kirigami.Theme.smallFont
                    opacity: 0.7
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
            }
            SfButton {
                text: entry.buttonText
                enabled: entry.available
                onClicked: entry.asked()
            }
        }
    }

    fullRepresentation: PlasmaExtras.Representation {
        // Sweeflow's violet for its switches, focus and selection, as in its own window on
        // every desktop (look.rs), instead of Plasma's accent.
        Kirigami.Theme.inherit: false
        Kirigami.Theme.colorSet: Kirigami.Theme.Window
        Kirigami.Theme.highlightColor: root.violet
        Kirigami.Theme.focusColor: root.violet
        Kirigami.Theme.hoverColor: root.violet
        Kirigami.Theme.linkColor: root.violet
        Layout.preferredWidth: Kirigami.Units.gridUnit * 24
        Layout.preferredHeight: Kirigami.Units.gridUnit * 32
        collapseMarginsHint: true

        // Esc puts back a rename or a question first; only then does it close the popup.
        Keys.onEscapePressed: (event) => {
            if (root.editingMac !== "" || root.confirmingMac !== "") {
                root.editingMac = "";
                root.confirmingMac = "";
            } else {
                event.accepted = false;
            }
        }

        // The Mac, where streaming stands, and what to do about it.
        header: PlasmaExtras.PlasmoidHeading {
            RowLayout {
                anchors.fill: parent
                spacing: Kirigami.Units.largeSpacing
                Kirigami.Icon {
                    source: root.appIcon
                    Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    Kirigami.Heading {
                        level: 4
                        text: root.paired ? root.macName : "Sweeflow"
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        visible: root.paired
                        spacing: Kirigami.Units.smallSpacing
                        // The Mac in use's dot, as in the list of Macs.
                        Rectangle {
                            readonly property bool hollow: ["", "paused", "offline"].includes(root.activeState)
                            implicitWidth: Kirigami.Units.smallSpacing * 2
                            implicitHeight: implicitWidth
                            radius: width / 2
                            color: hollow ? "transparent" : root.macInk(root.activeState)
                            border.width: hollow ? 2 : 0
                            border.color: root.macInk(root.activeState)
                        }
                        PlasmaComponents3.Label {
                            text: root.statusText
                            font: Kirigami.Theme.smallFont
                            opacity: 0.7
                            elide: Text.ElideRight
                            Layout.minimumWidth: 0
                        }
                        ViaSign { info: root.activeMac; Layout.fillWidth: true }
                    }
                }
                // What can be done in this state, as signs at the right of the name.
                Repeater {
                    model: root.streamActions
                    delegate: SignButton {
                        required property string modelData
                        readonly property string why: modelData === "start" ? root.startBlocked(root.st.mac || "The Mac", root.st.online === undefined ? null : root.st.online, true, root.linkedMacs.some(m => m.active && m.busy)) : ""
                        readonly property bool blocked: why !== ""
                        icon.name: root.actionIcons[modelData] || ""
                        text: root.actionLabels[modelData] || modelData
                        tip: blocked ? why : (root.actionTips[modelData] || text)
                        // Faded rather than disabled, so the tip still shows on hover.
                        opacity: blocked ? 0.45 : 1
                        onClicked: if (!blocked) root.act(modelData)
                    }
                }
            }
        }

        footer: PlasmaExtras.PlasmoidHeading {
            visible: root.paired
            position: PlasmaComponents3.ToolBar.Footer
            RowLayout {
                anchors.fill: parent
                // The Shortcuts window: Sweeflow's shortcuts, the swap, the ones kept for Linux.
                SfButton {
                    icon.name: "input-keyboard-symbolic"
                    text: "Shortcuts"
                    onClicked: { root.act("shortcuts"); root.expanded = false; }
                }
                Item { Layout.fillWidth: true }
                SfButton {
                    text: "Quit Sweeflow"
                    onClicked: root.act("quit")
                    PlasmaComponents3.ToolTip.text: "Stop Sweeflow on this computer (the Mac's windows close here)"
                    PlasmaComponents3.ToolTip.visible: hovered
                    PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                }
            }
        }

        // Opens on the tab it was left on.
        ColumnLayout {
            anchors.fill: parent
            visible: root.paired
            spacing: 0
            PlasmaComponents3.TabBar {
                id: tabs
                Layout.fillWidth: true
                // The current tab's underline in Sweeflow's violet (Plasma's theme draws its own
                // in the desktop's highlight colour).
                Component.onCompleted: contentItem.highlight = violetTab
                Component {
                    id: violetTab
                    Item {
                        Rectangle {
                            anchors { left: parent.left; right: parent.right; bottom: parent.bottom }
                            height: 2
                            color: root.violet
                        }
                    }
                }
                PlasmaComponents3.TabButton {
                    readonly property int count: root.allApps.length
                    text: count ? "Apps (" + count + ")" : "Apps"
                    icon.name: "view-app-grid-symbolic"
                }
                PlasmaComponents3.TabButton { text: "Settings"; icon.name: "configure" }
                PlasmaComponents3.TabButton { text: "Macs"; icon.name: "computer-laptop" }
            }
            StackLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                currentIndex: tabs.currentIndex
                Loader { sourceComponent: appsPage }
                Loader { sourceComponent: settingsPage }
                Loader { sourceComponent: macsPage }
            }
        }
        Connections {
            target: root
            function onExpandedChanged() {
                if (root.expanded) {
                    // Opened: look again at panels and rules (they can be changed in Plasma's
                    // own settings too).
                    root.act("placement");
                } else {
                    // Closed: a rename or a question left open is put back.
                    root.editingMac = "";
                    root.confirmingMac = "";
                }
            }
        }

        // Apps: pinned, running, the rest (the client sorts them so), under headings.
        Component {
            id: appsPage
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                PlasmaExtras.SearchField {
                    id: search
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    placeholderText: "Search apps"
                    background: FieldFrame { control: search }
                    // Ready to type into whenever the tab opens.
                    onVisibleChanged: if (visible) forceActiveFocus()
                }
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        id: appList
                        clip: true
                        model: root.allApps.filter(a => a.name.toLowerCase().includes(search.text.trim().toLowerCase()))
                        delegate: ColumnLayout {
                            id: appRow
                            required property var modelData
                            required property int index
                            readonly property bool appRunning: modelData.state > 0
                            readonly property bool shown: !(root.settings.exclude || []).includes(modelData.id)
                            function group(a) { return a.pinned ? "Pinned" : a.state > 0 ? "Running" : "Not running"; }
                            readonly property bool firstOfGroup: index === 0 || group(appList.model[index - 1]) !== group(modelData)
                            width: ListView.view.width
                            spacing: 0
                            Kirigami.Heading {
                                visible: appRow.firstOfGroup
                                level: 5
                                text: appRow.group(appRow.modelData)
                                opacity: 0.7
                                Layout.leftMargin: Kirigami.Units.largeSpacing
                                Layout.topMargin: appRow.index === 0 ? 0 : Kirigami.Units.largeSpacing
                                Layout.bottomMargin: Kirigami.Units.smallSpacing
                            }
                            // Click to open it; the switch says whether its windows come here.
                            PlasmaComponents3.ItemDelegate {
                                Layout.fillWidth: true
                                onClicked: root.act("launch", appRow.modelData.id)
                                contentItem: RowLayout {
                                    spacing: Kirigami.Units.smallSpacing * 2
                                    Kirigami.Icon {
                                        source: appRow.modelData.icon || "application-x-executable"
                                        Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                                        Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                                    }
                                    PlasmaComponents3.Label { text: appRow.modelData.name; elide: Text.ElideRight; Layout.fillWidth: true }
                                    // Running on the Mac (a pinned one shows it here too).
                                    Rectangle {
                                        visible: appRow.appRunning
                                        implicitWidth: Kirigami.Units.smallSpacing * 1.5
                                        implicitHeight: implicitWidth
                                        radius: width / 2
                                        color: Kirigami.Theme.highlightColor
                                    }
                                    SfButton {
                                        visible: appRow.appRunning
                                        text: "Quit"
                                        onClicked: root.act("quit-app", appRow.modelData.id)
                                        PlasmaComponents3.ToolTip.text: "Quit " + appRow.modelData.name + " on the Mac"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                    SfButton {
                                        text: appRow.modelData.pinned ? "Unpin" : "Pin"
                                        onClicked: root.act(appRow.modelData.pinned ? "unpin" : "pin", appRow.modelData.id)
                                        PlasmaComponents3.ToolTip.text: appRow.modelData.pinned ? "Unpin from the top" : "Pin to the top"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                    VioletSwitch {
                                        checked: appRow.shown
                                        onToggled: {
                                            const exclude = (root.settings.exclude || []).filter(b => b !== appRow.modelData.id);
                                            if (!checked) exclude.push(appRow.modelData.id);
                                            root.set({ exclude: exclude });
                                            checked = Qt.binding(() => appRow.shown);
                                        }
                                        PlasmaComponents3.ToolTip.text: appRow.shown ? "Block this app from streaming" : "Allow this app to stream"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                }
                            }
                        }
                        PlasmaExtras.PlaceholderMessage {
                            anchors.centerIn: parent
                            width: parent.width - Kirigami.Units.gridUnit * 4
                            visible: appList.count === 0
                            text: search.text.trim() !== "" ? "No matching apps" : "The Mac's apps show here once it has connected."
                        }
                    }
                }
            }
        }

        // Settings, every one a switch, then what can be done to the Mac now.
        Component {
            id: settingsPage
            PlasmaComponents3.ScrollView {
                contentWidth: availableWidth
                ColumnLayout {
                    width: parent.width
                    spacing: 0

                    Item { Layout.preferredHeight: Kirigami.Units.smallSpacing }
                    // The features first, named and ordered as on the setup window's last page.
                    ToggleRow {
                        label: "Apps in launcher"
                        hint: "Mac apps in the app launcher, alongside your Linux apps"
                        on: !!root.settings.launcher
                        onFlipped: (v) => root.set({ launcher: v })
                    }
                    // Plasma only: the other desktops have no menu bar to put the menus in.
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Global menu"
                        hint: "The Mac apps' menus at the top of the screen"
                        on: !!root.placement.globalMenu
                        onFlipped: (v) => root.act("global-menu", v ? "on" : "off")
                    }
                    ToggleRow {
                        label: "Shared clipboard"
                        hint: "Copy on one, paste on the other"
                        on: !!root.settings.clipboard
                        onFlipped: (v) => root.set({ clipboard: v })
                    }
                    ToggleRow {
                        label: "Drag and drop files"
                        hint: "Files, between Mac windows and Linux"
                        on: !!root.settings.dragDrop
                        onFlipped: (v) => root.set({ dragDrop: v })
                    }
                    ToggleRow {
                        label: "Sound passthrough"
                        hint: "The Mac's sound plays on this computer"
                        on: !!root.settings.sound
                        onFlipped: (v) => root.set({ sound: v })
                    }
                    ToggleRow {
                        label: "High quality"
                        hint: "Retina detail and 10-bit colour. Off: half the detail and 8-bit"
                        on: root.settings.quality !== "light"
                        onFlipped: (v) => root.set({ quality: v ? "sharp" : "light" })
                    }
                    ToggleRow {
                        label: "Square window corners"
                        hint: "Mac windows end in square corners instead of the Mac's rounded ones"
                        on: !!root.st.square
                        onFlipped: (v) => root.act("square", v ? "on" : "off")
                    }
                    // A MacBook, connected: the Mac asks for its password (LidSleepRule.swift).
                    ToggleRow {
                        visible: root.connected && !!root.settings.hasLid
                        label: "Stream with lid closed"
                        hint: "Keep the MacBook available while its lid is closed. Asks for the Mac's admin password to turn it on or off."
                        on: !!root.settings.lid
                        onFlipped: (v) => root.set({ lid: v })
                    }
                    ToggleRow {
                        label: "Open at login"
                        hint: "Start Sweeflow when you log in"
                        on: !!root.st.openAtLogin
                        onFlipped: (v) => root.act("login", v ? "on" : "off")
                    }
                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }
                    // Dims the Mac's own screen now; using the Mac itself brings it back.
                    ActionRow {
                        label: "Dim Mac screen"
                        note: "Until someone uses the Mac itself"
                        buttonText: root.settings.dim ? "Dimmed" : "Dim"
                        available: root.streaming && !root.settings.dim
                        onAsked: root.set({ dim: true })
                    }
                    // The Mac's whole screen in a window, to see what's going on there.
                    ActionRow {
                        label: "Troubleshoot (stream display)"
                        note: "Show the Mac's whole screen in a window"
                        buttonText: st.displayShown ? "Hide" : "Show"
                        available: root.streaming || !!st.displayShown
                        onAsked: { root.act("display"); root.expanded = !!st.displayShown; }
                    }
                    Item { Layout.preferredHeight: Kirigami.Units.smallSpacing }
                }
            }
        }

        // The linked Macs, one line each: the name (double-click to rename), an icon saying
        // whether it is online, its stream actions (those of its state for the Mac in use; Start,
        // which switches to it, for another) and Unlink.
        Component {
            id: macsPage
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        clip: true
                        model: root.linkedMacs
                        delegate: PlasmaComponents3.ItemDelegate {
                            id: mac
                            required property var modelData
                            readonly property bool editing: root.editingMac === modelData.fingerprint
                            readonly property bool confirming: root.confirmingMac === modelData.fingerprint
                            width: ListView.view.width
                            // Nothing happens on a stray click on the row: the buttons do things.
                            hoverEnabled: false
                            down: false

                            function save() {
                                const name = field.text.trim();
                                if (name !== "" && name !== modelData.name) root.rename(modelData.fingerprint, name);
                                root.editingMac = "";
                            }

                            // As tall as its tallest state, so the list stays put while a Mac is
                            // renamed or asked about.
                            contentItem: Item {
                                implicitHeight: Math.max(lines.implicitHeight, renaming.implicitHeight, asking.implicitHeight)

                                // One line: the Mac (double-click the name to rename it), whether
                                // it can be reached, then the stream signs and Unlink.
                                RowLayout {
                                    id: lines
                                    anchors.fill: parent
                                    visible: !mac.editing && !mac.confirming
                                    spacing: Kirigami.Units.smallSpacing
                                    // The name, and under it where the Mac stands (a status, never an
                                    // action): Online green; Locked or Busy orange; Not online grey;
                                    // nothing while not known yet. Locked and busy are known only of
                                    // the Mac in use.
                                    ColumnLayout {
                                        id: nameCol
                                        readonly property string macState: root.macStateOf(mac.modelData)
                                        Layout.fillWidth: true
                                        spacing: 0
                                        PlasmaComponents3.Label {
                                            text: mac.modelData.name
                                            font.bold: mac.modelData.active
                                            elide: Text.ElideRight
                                            Layout.fillWidth: true
                                            MouseArea {
                                                id: nameArea
                                                anchors.fill: parent
                                                hoverEnabled: true
                                                onDoubleClicked: { root.confirmingMac = ""; root.editingMac = mac.modelData.fingerprint; }
                                            }
                                            PlasmaComponents3.ToolTip.text: "Double-click to rename"
                                            PlasmaComponents3.ToolTip.visible: nameArea.containsMouse
                                            PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                        }
                                        // The dot says it in colour; the words stay plain.
                                        RowLayout {
                                            visible: nameCol.macState !== "" || mac.modelData.via !== ""
                                            spacing: Kirigami.Units.smallSpacing
                                            // Streaming violet, Paused a violet ring (streaming to this
                                            // computer); Available green; Busy or Locked orange; Not online
                                            // a grey ring.
                                            Rectangle {
                                                visible: nameCol.macState !== ""
                                                readonly property color ink: root.macInk(nameCol.macState)
                                                readonly property bool hollow: nameCol.macState === "paused" || nameCol.macState === "offline"
                                                implicitWidth: Math.round(Kirigami.Units.smallSpacing * 2)
                                                implicitHeight: implicitWidth
                                                radius: width / 2
                                                color: hollow ? "transparent" : ink
                                                border.width: hollow ? 2 : 0
                                                border.color: ink
                                            }
                                            PlasmaComponents3.Label {
                                                text: ({ streaming: "Streaming", paused: "Paused", available: "Available", offline: "Not online", locked: "Locked", busy: "Streaming elsewhere" })[nameCol.macState] || ""
                                                font: Kirigami.Theme.smallFont
                                                opacity: 0.7
                                                elide: Text.ElideRight
                                                Layout.minimumWidth: 0
                                            }
                                            ViaSign { info: mac.modelData; lead: nameCol.macState !== ""; Layout.fillWidth: true }
                                        }
                                    }
                                    Repeater {
                                        model: mac.modelData.active ? root.streamActions : ["start"]
                                        delegate: SignButton {
                                            required property string modelData
                                            icon.name: root.actionIcons[modelData] || ""
                                            text: root.actionLabels[modelData] || modelData
                                            readonly property string why: modelData === "start" ? root.startBlocked(mac.modelData.name, mac.modelData.online, mac.modelData.active, mac.modelData.busy) : ""
                                            readonly property bool blocked: why !== ""
                                            tip: blocked ? why
                                               : mac.modelData.active ? root.actionTips[modelData] : "Start streaming from this Mac"
                                            opacity: blocked ? 0.45 : 1
                                            onClicked: {
                                                if (blocked) return
                                                if (mac.modelData.active) root.act(modelData)
                                                else root.act("switch", mac.modelData.fingerprint)
                                            }
                                        }
                                    }
                                    PlasmaComponents3.ToolButton {
                                        icon.name: "edit-delete-remove"
                                        text: "Unlink"
                                        display: QQC2.AbstractButton.IconOnly
                                        onClicked: { root.editingMac = ""; root.confirmingMac = mac.modelData.fingerprint; }
                                        PlasmaComponents3.ToolTip.text: "Unlink this Mac"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                }

                                // Renamed in place: Enter or Save keeps the name, Esc or Cancel
                                // the old one.
                                RowLayout {
                                    id: renaming
                                    anchors.fill: parent
                                    visible: mac.editing
                                    spacing: Kirigami.Units.smallSpacing
                                    PlasmaComponents3.TextField {
                                        id: field
                                        Layout.fillWidth: true
                                        background: FieldFrame { control: field }
                                        onAccepted: mac.save()
                                        Keys.onEscapePressed: root.editingMac = ""
                                        onVisibleChanged: if (visible) {
                                            text = mac.modelData.name;
                                            forceActiveFocus();
                                            selectAll();
                                        }
                                    }
                                    SfButton {
                                        text: "Save"
                                        onClicked: mac.save()
                                    }
                                    SfButton {
                                        text: "Cancel"
                                        onClicked: root.editingMac = ""
                                    }
                                }

                                // Asked once before a Mac is unlinked.
                                RowLayout {
                                    id: asking
                                    anchors.fill: parent
                                    visible: mac.confirming
                                    spacing: Kirigami.Units.smallSpacing
                                    PlasmaComponents3.Label {
                                        text: "Unlink " + mac.modelData.name + "? To use it again you link it again."
                                        wrapMode: Text.Wrap
                                        Layout.fillWidth: true
                                    }
                                    SfButton {
                                        text: "Unlink"
                                        onClicked: { root.confirmingMac = ""; root.act("forget", mac.modelData.fingerprint); }
                                    }
                                    SfButton {
                                        text: "Cancel"
                                        onClicked: root.confirmingMac = ""
                                    }
                                }
                            }
                        }
                    }
                }
                SfButton {
                    Layout.alignment: Qt.AlignLeft
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    text: "Link a Mac…"
                    onClicked: root.act("pair")
                }
            }
        }

        // No Mac yet, or no client to ask.
        PlasmaExtras.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: !root.paired
            text: root.running ? "No Mac linked yet" : "Sweeflow isn't running"
            helpfulAction: QQC2.Action {
                icon.name: root.running ? "list-add" : "media-playback-start"
                text: root.running ? "Link a Mac…" : "Start Sweeflow"
                onTriggered: root.running ? root.act("pair") : shell.connectSource("systemctl --user start sweeflow")
            }
        }
    }
}
