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

    readonly property string statusText: {
        if (!running) return "Sweeflow isn't running";
        if (!st.paired) return "No Mac paired yet";
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
        Layout.preferredHeight: Kirigami.Units.gridUnit * 22
        collapseMarginsHint: true

        header: PlasmaExtras.PlasmoidHeading {
            RowLayout {
                anchors.fill: parent
                spacing: Kirigami.Units.largeSpacing
                Kirigami.Icon {
                    source: root.logo
                    Layout.preferredWidth: Kirigami.Units.iconSizes.large
                    Layout.preferredHeight: Kirigami.Units.iconSizes.large
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    Kirigami.Heading {
                        level: 3
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
                            opacity: 0.7
                            elide: Text.ElideRight
                            Layout.fillWidth: true
                        }
                    }
                }
                PlasmaComponents3.Button {
                    visible: root.running && !!root.st.paired
                    icon.name: !root.connected ? (!!root.st.resumeWanted ? "dialog-cancel" : "system-reboot")
                             : root.streaming ? "media-playback-pause" : "media-playback-start"
                    text: !root.connected ? (!!root.st.resumeWanted ? "Cancel" : "Wake Mac")
                        : root.streaming ? "Pause" : "Resume"
                    onClicked: {
                        if (root.connected) root.set({ enabled: !root.streaming });
                        else root.set({ enabled: !root.st.resumeWanted });
                    }
                }
                PlasmaComponents3.Button {
                    visible: !root.running
                    icon.name: "media-playback-start"
                    text: "Start"
                    onClicked: shell.connectSource("systemctl --user start sweeflow")
                }
                PlasmaComponents3.Button {
                    visible: root.running && !root.st.paired
                    icon.name: "list-add"
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
                    icon.name: "computer-laptop"
                    text: "Linked Macs"
                    enabled: stack.visible
                    onClicked: { stack.pop(null); stack.push(macsPage); }
                }
                Item { Layout.fillWidth: true }
                PlasmaComponents3.ToolButton {
                    icon.name: "application-exit"
                    text: "Quit"
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
            function onExpandedChanged() { if (!root.expanded) stack.pop(null); }
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
                            Kirigami.Icon { source: "view-app-grid-symbolic"; Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium; Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium }
                            PlasmaComponents3.Label { text: "Apps"; Layout.fillWidth: true }
                            PlasmaComponents3.Label {
                                readonly property int total: (root.st.apps || []).length
                                readonly property int hidden: root.settings ? root.settings.exclude.length : 0
                                text: hidden ? (total - hidden) + " of " + total : "All " + total
                                opacity: 0.6
                            }
                            Kirigami.Icon { source: "go-next-symbolic"; Layout.preferredWidth: Kirigami.Units.iconSizes.small; Layout.preferredHeight: Kirigami.Units.iconSizes.small }
                        }
                    }

                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    Repeater {
                        model: [
                            { key: "sound", icon: "audio-volume-high-symbolic", label: "Play Mac sound" },
                            { key: "clipboard", icon: "edit-paste-symbolic", label: "Clipboard" },
                            { key: "dragDrop", icon: "transform-move-symbolic", label: "Drag and drop files" },
                            { key: "dim", icon: "brightness-low-symbolic", label: "Dim Mac screen" },
                            { key: "desktop", icon: "virtual-desktops-symbolic", label: "Own desktop" },
                            { key: "launcher", icon: "application-menu-symbolic", label: "Mac apps in Linux menu" },
                            { key: "sharp", icon: "view-preview-symbolic", label: "High quality", tip: "Turn off for slower connections" },
                        ]
                        delegate: ToggleRow {
                            required property var modelData
                            iconName: modelData.icon
                            label: modelData.label
                            tip: modelData.tip || ""
                            on: !!root.settings && (modelData.key === "sharp" ? root.settings.quality !== "light" : !!root.settings[modelData.key])
                            enabled: root.streaming
                            onFlipped: (v) => root.set(modelData.key === "sharp" ? { quality: v ? "sharp" : "light" } : { [modelData.key]: v })
                        }
                    }

                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    ToggleRow {
                        iconName: "system-run-symbolic"
                        label: "Open at login"
                        on: !!root.st.openAtLogin
                        onFlipped: (v) => root.act("login", v ? "on" : "off")
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
                        icon.name: "go-previous-symbolic"
                        onClicked: stack.pop()
                        PlasmaComponents3.ToolTip.text: "Back"
                        PlasmaComponents3.ToolTip.visible: hovered
                    }
                    Kirigami.Heading { level: 4; text: "Linked Macs"; Layout.fillWidth: true }
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
                        icon.name: "go-previous-symbolic"
                        onClicked: stack.pop()
                        PlasmaComponents3.ToolTip.text: "Back"
                        PlasmaComponents3.ToolTip.visible: hovered
                    }
                    Kirigami.Heading { level: 4; text: "Apps" }
                    PlasmaExtras.SearchField {
                        id: search
                        Layout.fillWidth: true
                        Component.onCompleted: forceActiveFocus()
                    }
                }
                PlasmaComponents3.Label {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.largeSpacing
                    text: "Click to open on the Mac · switch off to keep it there"
                    font: Kirigami.Theme.smallFont
                    opacity: 0.7
                }
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        clip: true
                        model: (root.st.apps || []).filter(a => a.name.toLowerCase().includes(search.text.toLowerCase()))
                        delegate: PlasmaComponents3.ItemDelegate {
                            id: appRow
                            required property var modelData
                            readonly property bool shown: !!root.settings && !root.settings.exclude.includes(modelData.id)
                            width: ListView.view.width
                            onClicked: root.act("launch", modelData.id)
                            contentItem: RowLayout {
                                spacing: Kirigami.Units.largeSpacing
                                Kirigami.Icon {
                                    source: appRow.modelData.icon || "application-x-executable"
                                    Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                                    Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                                }
                                PlasmaComponents3.Label { text: appRow.modelData.name; elide: Text.ElideRight; Layout.fillWidth: true }
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

        PlasmaExtras.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: !root.running || !root.st.paired
            iconName: "sweeflow"
            text: !root.running ? "Sweeflow isn't running" : "No Mac paired yet"
            explanation: !root.running ? "Start it to see your Mac's apps here." : "Add a Mac to stream its apps to this desktop."
        }
    }
}
