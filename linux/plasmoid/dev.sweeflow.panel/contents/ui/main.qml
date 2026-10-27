// The Sweeflow tray widget: the client's state over D-Bus (src/panel.rs), shown as a popup with
// the same layout as the Mac's menu-bar panel (StatusPanel.swift).

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
    readonly property bool connected: running && !!st.connected
    readonly property var settings: st.settings || null
    readonly property bool streaming: connected && !!settings && settings.enabled
    readonly property string macName: st.mac || "Mac"
    readonly property var placement: st.placement || {}
    readonly property bool offline: running && !!st.offline

    // Opened from the app menu (sweeflow open): pop up.
    readonly property real showAt: st.showAt || 0
    onShowAtChanged: if (showAt > 0 && Math.abs(Date.now() - showAt) < 10000) root.expanded = true

    readonly property string statusText: {
        if (!running) return "Sweeflow isn't running";
        if (!st.paired) return "No Mac paired yet";
        if (offline) return "Disconnected";
        if (!connected) return st.resumeWanted ? "Not connected · will stream when it's back" : "Not connected";
        return streaming ? "Streaming apps" : "Paused";
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

    readonly property string logo: Qt.resolvedUrl("../icons/sweeflow.svg")
    Plasmoid.icon: logo
    Plasmoid.status: running ? PlasmaCore.Types.ActiveStatus : PlasmaCore.Types.PassiveStatus
    toolTipMainText: running && st.paired ? macName : "Sweeflow"
    toolTipSubText: statusText

    compactRepresentation: MouseArea {
        onClicked: root.expanded = !root.expanded
        hoverEnabled: true
        Kirigami.Icon {
            anchors.fill: parent
            source: root.logo
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

    fullRepresentation: PlasmaExtras.Representation {
        Layout.preferredWidth: Kirigami.Units.gridUnit * 22
        Layout.preferredHeight: Kirigami.Units.gridUnit * 30
        collapseMarginsHint: true

        header: PlasmaExtras.PlasmoidHeading {
            RowLayout {
                anchors.fill: parent
                spacing: Kirigami.Units.largeSpacing
                Kirigami.Icon {
                    source: root.logo
                    Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    Kirigami.Heading {
                        level: 4
                        text: root.running && !!root.st.paired ? root.macName : "Sweeflow"
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        spacing: Kirigami.Units.smallSpacing
                        Rectangle {
                            width: Kirigami.Units.smallSpacing * 2
                            height: width
                            radius: width / 2
                            color: root.streaming ? Kirigami.Theme.positiveTextColor
                                 : root.connected ? Kirigami.Theme.neutralTextColor
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
                PlasmaComponents3.Button {
                    visible: root.offline
                    text: "Connect"
                    onClicked: root.act("connect")
                }
                PlasmaComponents3.Button {
                    visible: root.running && !!root.st.paired && !root.offline
                    text: !root.connected ? (!!root.st.resumeWanted ? "Cancel" : "Wake Mac")
                        : root.streaming ? "Pause" : "Resume"
                    onClicked: {
                        if (root.connected) root.set({ enabled: !root.streaming });
                        else root.set({ enabled: !root.st.resumeWanted });
                    }
                }
                PlasmaComponents3.Button {
                    visible: !root.running
                    text: "Start"
                    onClicked: shell.connectSource("systemctl --user start sweeflow")
                }
                PlasmaComponents3.Button {
                    visible: root.running && !root.st.paired
                    text: "Link a Mac…"
                    onClicked: root.act("pair")
                }
            }
        }

        footer: PlasmaExtras.PlasmoidHeading {
            visible: root.running
            position: PlasmaComponents3.ToolBar.Footer
            RowLayout {
                anchors.fill: parent
                PlasmaComponents3.ToolButton {
                    text: "Linked Macs"
                    enabled: stack.visible
                    onClicked: { stack.pop(null); stack.push(macsPage); }
                }
                Item { Layout.fillWidth: true }
                PlasmaComponents3.ToolButton {
                    // Stops connecting (the Mac's windows close here) without forgetting it.
                    visible: !!root.st.paired
                    text: root.offline ? "Connect" : "Disconnect"
                    onClicked: root.act(root.offline ? "connect" : "disconnect")
                }
                PlasmaComponents3.ToolButton {
                    text: "Quit Sweeflow"
                    onClicked: root.act("quit")
                }
            }
        }

        // Main page: switches. "Apps ›" springs out a second page with the full list.
        QQC2.StackView {
            id: stack
            anchors.fill: parent
            visible: root.running && !!root.st.paired
            initialItem: mainPage
        }
        Connections {
            target: root
            // Closed: back to the main page. Opened: look again at panels and rules (they can be
            // changed in Plasma's own settings too).
            function onExpandedChanged() { if (!root.expanded) stack.pop(null); else root.act("placement"); }
        }

        Component {
            id: mainPage
            PlasmaComponents3.ScrollView {
                contentWidth: availableWidth
                ColumnLayout {
                    width: parent.width
                    spacing: 0
                    enabled: !!root.settings

                    PlasmaComponents3.ItemDelegate {
                        Layout.fillWidth: true
                        enabled: root.streaming
                        onClicked: stack.push(appsPage)
                        contentItem: RowLayout {
                            spacing: Kirigami.Units.largeSpacing
                            PlasmaComponents3.Label { text: "Apps"; Layout.fillWidth: true }
                            PlasmaComponents3.Label {
                                readonly property int total: (root.st.apps || []).length
                                readonly property int hidden: root.settings ? root.settings.exclude.length : 0
                                readonly property int running: (root.st.apps || []).filter(a => a.state > 0).length
                                text: (running ? running + " running · " : "") + (hidden ? (total - hidden) + " of " + total : "All " + total)
                                opacity: 0.6
                            }
                            PlasmaComponents3.Label { text: "›"; opacity: 0.6 }
                        }
                    }

                    PlasmaComponents3.ItemDelegate {
                        Layout.fillWidth: true
                        enabled: root.streaming
                        onClicked: { root.act("display"); root.expanded = false; }
                        contentItem: PlasmaComponents3.Label { text: "Show Mac display" }
                    }

                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    Kirigami.Heading {
                        level: 5
                        text: "Streaming"
                        opacity: 0.7
                        Layout.leftMargin: Kirigami.Units.largeSpacing
                        Layout.bottomMargin: Kirigami.Units.smallSpacing
                    }
                    Repeater {
                        model: [
                            { key: "sound", label: "Play Mac sound" },
                            { key: "clipboard", label: "Shared clipboard" },
                            { key: "dragDrop", label: "Drag and drop files" },
                            { key: "dim", label: "Dim Mac screen" },
                            { key: "sharp", label: "High quality" },
                        ]
                        delegate: ToggleRow {
                            required property var modelData
                            label: modelData.label
                            on: !!root.settings && (modelData.key === "sharp" ? root.settings.quality !== "light" : !!root.settings[modelData.key])
                            enabled: root.streaming
                            onFlipped: (v) => root.set(modelData.key === "sharp" ? { quality: v ? "sharp" : "light" } : { [modelData.key]: v })
                        }
                    }
                    ToggleRow {
                        label: "Open at login"
                        on: !!root.st.openAtLogin
                        onFlipped: (v) => root.act("login", v ? "on" : "off")
                    }


                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    // Mac apps: where they show up (the KDE ones only on Plasma).
                    Kirigami.Heading {
                        level: 5
                        text: "Mac apps"
                        opacity: 0.7
                        Layout.leftMargin: Kirigami.Units.largeSpacing
                        Layout.bottomMargin: Kirigami.Units.smallSpacing
                    }
                    ToggleRow {
                        label: "Mac windows in own desktop"
                        on: !!root.settings && root.settings.desktop
                        enabled: !!root.settings
                        onFlipped: (v) => root.set({ desktop: v })
                        actionText: "Gather"
                        actionTip: "Bring every Mac window back to the Mac desktop"
                        onExtraClicked: root.act("gather")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Mac apps in Task Manager"
                        on: !!root.placement.taskbar
                        onFlipped: (v) => root.act("taskbar", v ? "on" : "off")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Mac Dock"
                        on: !!root.placement.dock
                        onFlipped: (v) => root.act("dock", v ? "own" : "off")
                    }
                    ToggleRow {
                        visible: !!root.placement.plasma
                        label: "Global Menu"
                        on: !!root.placement.globalMenu
                        onFlipped: (v) => root.act("global-menu", v ? "on" : "off")
                    }
                    ToggleRow {
                        label: "Mac apps in Linux menu"
                        on: !!root.settings && root.settings.launcher
                        enabled: !!root.settings
                        onFlipped: (v) => root.set({ launcher: v })
                    }
                }
            }
        }

        Component {
            id: macsPage
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                RowLayout {
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    PlasmaComponents3.ToolButton {
                        text: "‹ Back"
                        onClicked: stack.pop()
                    }
                    Kirigami.Heading { level: 4; text: "Linked Macs"; Layout.fillWidth: true }
                    PlasmaComponents3.ToolButton {
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
                RowLayout {
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    PlasmaComponents3.ToolButton {
                        text: "‹ Back"
                        onClicked: stack.pop()
                    }
                    Kirigami.Heading { level: 4; text: "Apps" }
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
                            readonly property bool shown: !!root.settings && !root.settings.exclude.includes(modelData.id)
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
                                ColumnLayout {
                                    spacing: 0
                                    Layout.fillWidth: true
                                    PlasmaComponents3.Label { text: appRow.modelData.name; elide: Text.ElideRight; Layout.fillWidth: true }
                                    PlasmaComponents3.Label {
                                        visible: appRow.modelData.state > 0 || appRow.modelData.pinned
                                        text: ["Not running", "Running, no window", "Minimised", "Open"][appRow.modelData.state] || ""
                                        font: Kirigami.Theme.smallFont
                                        color: appRow.modelData.state === 3 ? Kirigami.Theme.positiveTextColor : Kirigami.Theme.textColor
                                        opacity: appRow.modelData.state === 3 ? 0.9 : 0.6
                                    }
                                }
                                PlasmaComponents3.ToolButton {
                                    visible: appRow.modelData.state > 0
                                    text: "Quit"
                                    onClicked: root.act("quit-app", appRow.modelData.id)
                                    PlasmaComponents3.ToolTip.text: "Quit " + appRow.modelData.name + " on the Mac"
                                    PlasmaComponents3.ToolTip.visible: hovered
                                    PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                }
                                PlasmaComponents3.ToolButton {
                                    text: appRow.modelData.pinned ? "Unpin" : "Pin"
                                    opacity: appRow.modelData.pinned || hovered ? 1 : 0.35
                                    onClicked: root.act(appRow.modelData.pinned ? "unpin" : "pin", appRow.modelData.id)
                                    PlasmaComponents3.ToolTip.text: appRow.modelData.pinned ? "Unpin" : "Pin (first here and kept in the Mac Dock)"
                                    PlasmaComponents3.ToolTip.visible: hovered
                                    PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                                }
                                PlasmaComponents3.Switch {
                                    checked: appRow.shown
                                    onToggled: {
                                        const exclude = root.settings.exclude.filter(b => b !== appRow.modelData.id);
                                        if (!checked) exclude.push(appRow.modelData.id);
                                        root.set({ exclude: exclude });
                                        checked = Qt.binding(() => appRow.shown);
                                    }
                                }
                            }
                        }
                        }
                    }
                }
            }
        }

        PlasmaExtras.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: !root.running || !root.st.paired
            text: !root.running ? "Sweeflow isn't running" : "No Mac paired yet"
            explanation: !root.running ? "Start it to see your Mac's apps here." : "Add a Mac to stream its apps to this desktop."
        }
    }
}
