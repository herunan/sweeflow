// The Sweeflow tray widget: the client's state over D-Bus (src/panel.rs), shown as a popup.
//
// Layout: the header says where things stand and holds every connection control (Pause /
// Resume, Wake, Connect, Disconnect); below, the apps (a page of their own) and every setting in
// sections; the footer opens Linked Macs. Settings can always be changed: with the Mac away the
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
    readonly property bool offline: running && !!st.offline
    // Always there: the Mac's, or (while it's away) the last known with the changes made since.
    readonly property var settings: st.settings || ({ exclude: [] })
    readonly property bool streaming: connected && !!settings.enabled
    readonly property string macName: st.mac || "Mac"
    readonly property var placement: st.placement || {}

    // Opened from the app menu (sweeflow open): pop up.
    readonly property real showAt: st.showAt || 0
    onShowAtChanged: if (showAt > 0 && Math.abs(Date.now() - showAt) < 10000) root.expanded = true

    // One of: stopped, unpaired, offline, away, streaming, paused.
    readonly property string phase: !running ? "stopped" : !paired ? "unpaired" : offline ? "offline"
                                  : !connected ? "away" : settings.enabled ? "streaming" : "paused"

    readonly property string statusText: {
        switch (phase) {
        case "stopped": return "Not running";
        case "unpaired": return "No Mac linked yet";
        case "offline": return "Disconnected";
        case "away": return st.pending ? "Can't reach the Mac · changes wait for it" : "Can't reach the Mac";
        case "streaming": return "Streaming";
        default: return "Paused";
        }
    }

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
    toolTipSubText: paired ? macName + " · " + statusText : statusText

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
            opacity: root.streaming ? 1 : 0.55
            Kirigami.Icon {
                visible: !root.streaming
                anchors { right: parent.right; bottom: parent.bottom }
                width: parent.width / 2
                height: width
                source: root.connected ? "media-playback-paused-symbolic" : "network-disconnect-symbolic"
            }
        }
    }

    // A section's start: a dividing line, then its title (small capitals, as Plasma's own
    // applets title their sections).
    component Section: ColumnLayout {
        property alias text: title.text
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.bottomMargin: Kirigami.Units.smallSpacing
        Kirigami.Separator { Layout.fillWidth: true }
        PlasmaComponents3.Label {
            id: title
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.smallSpacing
            font.capitalization: Font.AllUppercase
            font.weight: Font.DemiBold
            font.pointSize: Kirigami.Theme.smallFont.pointSize
            font.letterSpacing: 0.5
            opacity: 0.6
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

    // A sub-page's title bar: back, the title, and room for more on the right.
    component PageBar: RowLayout {
        id: bar
        default property alias extra: more.data
        property string title
        signal back()
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing
        PlasmaComponents3.ToolButton {
            icon.name: "go-previous-symbolic"
            onClicked: bar.back()
            PlasmaComponents3.ToolTip.text: "Back"
            PlasmaComponents3.ToolTip.visible: hovered
        }
        Kirigami.Heading { level: 4; text: bar.title }
        RowLayout { id: more; Layout.fillWidth: true }
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
                                 : root.phase === "paused" ? Kirigami.Theme.neutralTextColor
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
                // The one thing to do next.
                PlasmaComponents3.Button {
                    icon.name: ({ stopped: "media-playback-start", unpaired: "list-add", offline: "network-connect",
                                  away: "system-reboot", streaming: "media-playback-pause", paused: "media-playback-start" })[root.phase]
                    text: ({ stopped: "Start", unpaired: "Link a Mac…", offline: "Connect", away: "Wake Mac",
                             streaming: "Pause", paused: "Resume" })[root.phase]
                    onClicked: {
                        switch (root.phase) {
                        case "stopped": shell.connectSource("systemctl --user start sweeflow"); break;
                        case "unpaired": root.act("pair"); break;
                        case "offline": root.act("connect"); break;
                        case "away": root.act("wake"); break;
                        default: root.set({ enabled: !root.streaming });
                        }
                    }
                }
                // Stops connecting (the Mac's windows close here) without unlinking it.
                PlasmaComponents3.Button {
                    visible: ["away", "streaming", "paused"].includes(root.phase)
                    icon.name: "network-disconnect"
                    text: "Disconnect"
                    flat: true
                    onClicked: root.act("disconnect")
                }
            }
        }

        footer: PlasmaExtras.PlasmoidHeading {
            visible: root.running
            position: PlasmaComponents3.ToolBar.Footer
            RowLayout {
                anchors.fill: parent
                PlasmaComponents3.ToolButton {
                    icon.name: "computer-laptop"
                    text: "Linked Macs"
                    // Not while already there.
                    visible: stack.visible && !(stack.currentItem && stack.currentItem.objectName === "macs")
                    onClicked: { stack.pop(null); stack.push(macsPage); }
                }
                Item { Layout.fillWidth: true }
                PlasmaComponents3.ToolButton {
                    icon.name: "application-exit"
                    text: "Quit Sweeflow"
                    onClicked: root.act("quit")
                }
            }
        }

        QQC2.StackView {
            id: stack
            anchors.fill: parent
            visible: root.paired
            initialItem: mainPage
        }
        Connections {
            target: root
            // Opens where it was left (Apps, Linked Macs…). Opened: look again at panels and rules
            // (they can be changed in Plasma's own settings too).
            function onExpandedChanged() { if (root.expanded) root.act("placement"); }
        }

        // Main page: the apps (a page of their own), then every setting, in sections.
        Component {
            id: mainPage
            PlasmaComponents3.ScrollView {
                contentWidth: availableWidth
                ColumnLayout {
                    width: parent.width
                    spacing: 0

                    LinkRow {
                        readonly property var apps: root.st.apps || []
                        readonly property int open: apps.filter(a => a.state > 0).length
                        iconName: "view-app-grid-symbolic"
                        label: "Apps"
                        note: apps.length ? apps.length + (apps.length === 1 ? " app" : " apps") + " · " + open + " running" : ""
                        hint: "Open Mac apps, and choose which ones show here"
                        arrow: true
                        onClicked: stack.push(appsPage)
                    }

                    Section { text: "Where Mac apps appear" }
                    ToggleRow {
                        label: "Their own desktop"
                        hint: "A desktop just for Mac windows"
                        on: !!root.settings.desktop
                        onFlipped: (v) => root.set({ desktop: v })
                        actionText: root.connected ? "Gather" : ""
                        actionTip: "Bring every Mac window back to the Mac desktop"
                        onExtraClicked: root.act("gather")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Task Manager"
                        hint: "Mac apps listed with your Linux apps in the taskbar"
                        on: !!root.placement.taskbar
                        onFlipped: (v) => root.act("taskbar", v ? "on" : "off")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Mac Dock"
                        hint: "A dock of Mac apps, with window previews"
                        on: !!root.placement.dock
                        onFlipped: (v) => root.act("dock", v ? "own" : "off")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Global Menu"
                        hint: "The Mac apps' menus at the top of the screen"
                        on: !!root.placement.globalMenu
                        onFlipped: (v) => root.act("global-menu", v ? "on" : "off")
                    }
                    ToggleRow {
                        label: "Application launcher"
                        hint: "Mac apps in the app menu, alongside your Linux apps"
                        on: !!root.settings.launcher
                        onFlipped: (v) => root.set({ launcher: v })
                    }

                    Section { text: "Streaming" }
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
                    Item { Layout.preferredHeight: Kirigami.Units.smallSpacing }
                }
            }
        }

        Component {
            id: macsPage
            ColumnLayout {
                objectName: "macs"
                spacing: Kirigami.Units.smallSpacing
                PageBar {
                    title: "Linked Macs"
                    onBack: stack.pop()
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
                            offline: root.offline
                            onConnectAsked: root.act("connect")
                            onDisconnectAsked: root.act("disconnect")
                            onActivated: root.act("switch", modelData.fingerprint)
                            onRenamed: (n) => root.rename(modelData.fingerprint, n)
                            onForgotten: root.act("forget", modelData.fingerprint)
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
                    title: "Apps"
                    onBack: stack.pop()
                    PlasmaExtras.SearchField {
                        id: search
                        Layout.fillWidth: true
                        Component.onCompleted: forceActiveFocus()
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
                                        PlasmaComponents3.ToolTip.text: appRow.modelData.pinned ? "Unpin" : "First here, and kept in the Mac Dock"
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
