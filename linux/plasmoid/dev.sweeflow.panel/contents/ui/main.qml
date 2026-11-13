// The Sweeflow tray widget: the client's state over D-Bus (src/panel.rs), shown as a popup.
//
// Layout: the header says where things stand and holds its actions (Pause / Resume, Start,
// Stop); below, three tabs: Options (every setting, in sections),
// Linked Macs and Apps. Settings can always be changed: with the Mac away the
// client keeps them and applies them once it's back.

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

    // Opened from the app menu (sweeflow open): pop up.
    readonly property real showAt: st.showAt || 0
    onShowAtChanged: if (showAt > 0 && Math.abs(Date.now() - showAt) < 10000) root.expanded = true

    // One of: notRunning, unpaired, stopped, away (streaming wanted, the Mac not there), locked,
    // streaming, paused.
    readonly property string phase: !running ? "notRunning" : !paired ? "unpaired" : stopped ? "stopped"
                                  : !connected ? "away" : locked ? "locked" : settings.enabled ? "streaming" : "paused"

    // The client words it (the same line as its tray icon and its own window).
    readonly property string statusText: !running ? "Not running" : !paired ? "No Mac linked yet" : st.status || ""
    // The streaming actions (Do actions), as they're shown.
    readonly property var actionLabels: ({ start: "Start", pause: "Pause", resume: "Resume", stop: "Stop" })
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
            anchors.fill: parent
            source: root.logo
            // In the panel's text colour, like the icons beside it.
            isMask: true
            color: Kirigami.Theme.textColor
            active: parent.containsMouse
            // Paused or away: faded, with a small badge saying which.
            opacity: root.streaming && !root.locked ? 1 : 0.55
            Kirigami.Icon {
                visible: !root.streaming || root.locked
                anchors { right: parent.right; bottom: parent.bottom }
                width: parent.width / 2
                height: width
                source: root.locked ? "system-lock-screen-symbolic"
                      : root.connected ? "media-playback-paused-symbolic"
                      : root.stopped ? "media-playback-stop-symbolic" : "network-disconnect-symbolic"
            }
        }
    }

    // A row that opens or does something: icon (optional), name, and on the right a note and/or
    // an arrow; a short explanation on hover.
    component LinkRow: PlasmaComponents3.ItemDelegate {
        id: link
        property string iconName
        property string label
        property string note
        property bool arrow: false
        /// Said on hover.
        property string hint
        Layout.fillWidth: true
        PlasmaComponents3.ToolTip.text: hint
        PlasmaComponents3.ToolTip.visible: hovered && hint !== ""
        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing
            Kirigami.Icon {
                visible: link.iconName !== ""
                source: link.iconName
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }
            PlasmaComponents3.Label { text: link.label; elide: Text.ElideRight; Layout.fillWidth: true }
            PlasmaComponents3.Label { visible: link.note !== ""; text: link.note; opacity: 0.6 }
            Kirigami.Icon {
                visible: link.arrow
                source: "go-next-symbolic"
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
            }
        }
    }

    // A tab's tool row (search, Link a Mac…).
    component PageBar: RowLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing
    }

    fullRepresentation: PlasmaExtras.Representation {
        Layout.preferredWidth: Kirigami.Units.gridUnit * 24
        Layout.preferredHeight: Kirigami.Units.gridUnit * 32
        collapseMarginsHint: true

        // Where things stand, and what can be done about it.
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
                        spacing: Kirigami.Units.smallSpacing
                        Rectangle {
                            width: Kirigami.Units.smallSpacing * 2
                            height: width
                            radius: width / 2
                            color: root.phase === "streaming" ? Kirigami.Theme.positiveTextColor
                                 : root.phase === "paused" || root.phase === "locked" ? Kirigami.Theme.neutralTextColor
                                 // Not streaming, but the Mac is there to stream from.
                                 : root.phase === "stopped" && root.st.online === true ? Kirigami.Theme.neutralTextColor
                                 : Kirigami.Theme.disabledTextColor
                        }
                        PlasmaComponents3.Label {
                            text: root.statusText
                            font: Kirigami.Theme.smallFont
                            opacity: 0.7
                            elide: Text.ElideRight
                            Layout.fillWidth: true
                        }
                    }
                }
                // Before there's a Mac to stream from: the one thing to do.
                PlasmaComponents3.Button {
                    visible: root.phase === "notRunning" || root.phase === "unpaired"
                    icon.name: root.phase === "unpaired" ? "list-add" : "media-playback-start"
                    text: root.phase === "unpaired" ? "Link a Mac…" : "Start Sweeflow"
                    onClicked: root.phase === "unpaired" ? root.act("pair") : shell.connectSource("systemctl --user start sweeflow")
                }
                // What can be done about the Mac in use, as the client lists it: Pause and Stop
                // while streaming, Resume and Stop while paused, Start while stopped, Stop while
                // waiting for the Mac.
                Repeater {
                    model: root.paired ? (root.st.actions || []) : []
                    delegate: PlasmaComponents3.Button {
                        required property string modelData
                        icon.name: root.actionIcons[modelData] || ""
                        text: root.actionLabels[modelData] || modelData
                        flat: modelData === "stop"
                        onClicked: root.act(modelData)
                    }
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
                PlasmaComponents3.TabButton { text: "Options"; icon.name: "configure" }
                PlasmaComponents3.TabButton { text: "Linked Macs"; icon.name: "computer-laptop" }
                PlasmaComponents3.TabButton {
                    readonly property int count: (root.st.apps || []).length
                    text: count ? "Apps (" + count + ")" : "Apps"
                    icon.name: "view-app-grid-symbolic"
                }
            }
            StackLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                currentIndex: tabs.currentIndex
                Loader { sourceComponent: mainPage }
                Loader { sourceComponent: macsPage }
                Loader { sourceComponent: appsPage }
            }
        }
        Connections {
            target: root
            // Opened: look again at panels and rules
            // (they can be changed in Plasma's own settings too).
            function onExpandedChanged() { if (root.expanded) root.act("placement"); }
        }

        // Options: every setting, in sections.
        Component {
            id: mainPage
            PlasmaComponents3.ScrollView {
                contentWidth: availableWidth
                ColumnLayout {
                    width: parent.width
                    spacing: 0

                    Item { Layout.preferredHeight: Kirigami.Units.smallSpacing }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Global Menu"
                        hint: "The Mac apps' menus at the top of the screen"
                        on: !!root.placement.globalMenu
                        onFlipped: (v) => root.act("global-menu", v ? "on" : "off")
                    }
                    ToggleRow {
                        label: "Application launcher"
                        hint: "Mac apps in the app launcher, alongside your Linux apps"
                        on: !!root.settings.launcher
                        onFlipped: (v) => root.set({ launcher: v })
                    }
                    ToggleRow {
                        label: "Square window corners"
                        hint: "Mac windows end in square corners instead of the Mac's rounded ones"
                        on: !!root.st.square
                        onFlipped: (v) => root.act("square", v ? "on" : "off")
                    }
                    Repeater {
                        model: [
                            { key: "sound", label: "Play Mac sound here", hint: "Mac audio plays on Linux" },
                            { key: "clipboard", label: "Shared clipboard", hint: "Copy on one, paste on the other" },
                            { key: "dragDrop", label: "Drag and drop files", hint: "Between Mac windows and Linux" },
                            { key: "sharp", label: "High quality", hint: "Turn off for slower connections" },
                        ]
                        delegate: ToggleRow {
                            required property var modelData
                            label: modelData.label
                            hint: modelData.hint
                            on: modelData.key === "sharp" ? root.settings.quality !== "light" : !!root.settings[modelData.key]
                            onFlipped: (v) => root.set(modelData.key === "sharp" ? { quality: v ? "sharp" : "light" } : { [modelData.key]: v })
                        }
                    }
                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.largeSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }
                    ToggleRow {
                        label: "Open at login"
                        hint: "Start Sweeflow when you log in"
                        on: !!root.st.openAtLogin
                        onFlipped: (v) => root.act("login", v ? "on" : "off")
                    }
                    // Dims the MacBook's own screen now; using the Mac itself brings it back.
                    LinkRow {
                        label: "Dim Mac screen"
                        note: root.settings.dim ? "Dimmed" : ""
                        enabled: root.streaming && !root.settings.dim
                        hint: "Turns the Mac's own screen down until someone uses the Mac itself"
                        onClicked: root.set({ dim: true })
                    }
                    // The Mac's whole screen in a window, to see what's going on there.
                    LinkRow {
                        label: "Troubleshoot (stream display)"
                        enabled: root.streaming
                        hint: "Show the Mac's whole screen in a window"
                        onClicked: { root.act("display"); root.expanded = false; }
                    }
                    // Last, out of the way.
                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.largeSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }
                    LinkRow {
                        iconName: "application-exit"
                        label: "Quit Sweeflow"
                        hint: "Stop Sweeflow on this computer (the Mac's windows close here)"
                        onClicked: root.act("quit")
                    }
                    Item { Layout.preferredHeight: Kirigami.Units.smallSpacing }
                }
            }
        }

        Component {
            id: macsPage
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                PageBar {
                    Item { Layout.fillWidth: true }
                    PlasmaComponents3.ToolButton {
                        icon.name: "list-add"
                        text: "Link a Mac…"
                        onClicked: root.act("pair")
                    }
                }
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        clip: true
                        model: root.st.macs || []
                        delegate: MacRow {
                            required property var modelData
                            width: ListView.view.width
                            name: modelData.name
                            active: modelData.active
                            kind: modelData.kind || ""
                            // true, false, or null while not known yet.
                            online: modelData.online === undefined ? null : modelData.online
                            // The Mac in use: its state's actions; another: Start (switches to it).
                            actions: modelData.active ? (root.st.actions || []) : ["start"]
                            actionLabels: root.actionLabels
                            onActionAsked: (action) => modelData.active ? root.act(action) : root.act("switch", modelData.fingerprint)
                            onRenamed: (n) => root.rename(modelData.fingerprint, n)
                            onUnlinked: root.act("forget", modelData.fingerprint)
                        }
                    }
                }
            }
        }

        Component {
            id: appsPage
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                PageBar {
                    PlasmaExtras.SearchField {
                        id: search
                        Layout.fillWidth: true
                        // Ready to type into whenever the tab opens.
                        onVisibleChanged: if (visible) forceActiveFocus()
                    }
                }
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        id: appList
                        clip: true
                        // Pinned, running, the rest (the client sorts them so), each under a heading.
                        model: (root.st.apps || []).filter(a => a.name.toLowerCase().includes(search.text.toLowerCase()))
                        delegate: ColumnLayout {
                            id: appRow
                            required property var modelData
                            required property int index
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
                                    spacing: Kirigami.Units.largeSpacing
                                    Kirigami.Icon {
                                        source: appRow.modelData.icon || "application-x-executable"
                                        Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                                        Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                                    }
                                    PlasmaComponents3.Label { text: appRow.modelData.name; elide: Text.ElideRight; Layout.fillWidth: true }
                                    PlasmaComponents3.ToolButton {
                                        visible: appRow.modelData.state > 0
                                        icon.name: "application-exit"
                                        onClicked: root.act("quit-app", appRow.modelData.id)
                                        PlasmaComponents3.ToolTip.text: "Quit " + appRow.modelData.name + " on the Mac"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                    PlasmaComponents3.ToolButton {
                                        icon.name: appRow.modelData.pinned ? "window-unpin" : "window-pin"
                                        opacity: appRow.modelData.pinned || hovered ? 1 : 0.35
                                        onClicked: root.act(appRow.modelData.pinned ? "unpin" : "pin", appRow.modelData.id)
                                        PlasmaComponents3.ToolTip.text: appRow.modelData.pinned ? "Unpin" : "Pin: first in this list"
                                        PlasmaComponents3.ToolTip.visible: hovered
                                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                    }
                                    PlasmaComponents3.Switch {
                                        checked: appRow.shown
                                        onToggled: {
                                            const exclude = (root.settings.exclude || []).filter(b => b !== appRow.modelData.id);
                                            if (!checked) exclude.push(appRow.modelData.id);
                                            root.set({ exclude: exclude });
                                            checked = Qt.binding(() => appRow.shown);
                                        }
                                        PlasmaComponents3.ToolTip.text: appRow.shown ? "Its windows come here" : "Its windows stay on the Mac"
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
                            text: search.text !== "" ? "No matching apps" : "The Mac's apps show here once it has connected"
                        }
                    }
                }
            }
        }

        PlasmaExtras.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: !root.paired
            iconName: "sweeflow"
            text: !root.running ? "Sweeflow isn't running" : "No Mac linked yet"
            explanation: !root.running ? "Start it to see your Mac's apps here." : "Link a Mac to stream its apps to this desktop."
        }
    }
}
