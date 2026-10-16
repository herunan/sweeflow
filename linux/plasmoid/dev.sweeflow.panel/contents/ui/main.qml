// The Sweeflow tray widget: the client's state over D-Bus (src/panel.rs), shown as a popup with
// the same layout as the Mac's menu-bar panel (StatusPanel.swift).

import QtQuick
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
            member: member, arguments: args, signature: args.map(() => "s").join(""),
        });
    }
    function set(changes) { call("Set", [JSON.stringify(changes)]); }
    function act(action, arg) { call("Do", [action, arg || ""]); }

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
                    text: "Add a Mac…"
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
                    id: macsButton
                    icon.name: "computer-laptop"
                    text: (root.st.macs || []).length > 1 ? "Macs" : "Mac"
                    onClicked: macsMenu.open()
                    PlasmaComponents3.Menu {
                        id: macsMenu
                        y: -height
                        Repeater {
                            model: root.st.macs || []
                            PlasmaComponents3.MenuItem {
                                required property var modelData
                                text: modelData.name
                                checkable: true
                                checked: modelData.active
                                onTriggered: root.act("switch", modelData.fingerprint)
                            }
                        }
                        PlasmaComponents3.MenuSeparator {}
                        PlasmaComponents3.MenuItem {
                            icon.name: "list-add"
                            text: "Add a Mac…"
                            onTriggered: root.act("pair")
                        }
                        Repeater {
                            model: root.st.macs || []
                            PlasmaComponents3.MenuItem {
                                required property var modelData
                                icon.name: "edit-delete-remove"
                                text: "Forget " + modelData.name
                                onTriggered: root.act("forget", modelData.fingerprint)
                            }
                        }
                    }
                }
                Item { Layout.fillWidth: true }
                PlasmaComponents3.ToolButton {
                    icon.name: "application-exit"
                    text: "Quit"
                    onClicked: root.act("quit")
                }
            }
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.largeSpacing
            visible: root.running && !!root.st.paired
            enabled: !!root.settings

            // Feature tiles: lit when on. Greyed while the Mac is away (the last known state).
            GridLayout {
                Layout.fillWidth: true
                columns: 3
                uniformCellWidths: true
                columnSpacing: Kirigami.Units.smallSpacing
                rowSpacing: Kirigami.Units.smallSpacing
                Repeater {
                    model: [
                        { key: "sound", icon: "audio-volume-high-symbolic", label: "Sound", tip: "Play the Mac's sound on this PC" },
                        { key: "clipboard", icon: "edit-paste-symbolic", label: "Clipboard", tip: "Copy on one, paste on the other" },
                        { key: "dragDrop", icon: "transform-move-symbolic", label: "Drag & Drop", tip: "Drag files between Mac windows and this PC" },
                        { key: "dim", icon: "brightness-low-symbolic", label: "Dim Mac", tip: "Dim the Mac's own screen while streaming" },
                        { key: "desktop", icon: "virtual-desktops-symbolic", label: "Own Desktop", tip: "Put Mac apps on a desktop of their own" },
                        { key: "launcher", icon: "view-app-grid-symbolic", label: "In Launcher", tip: "List Mac apps in the app launcher" },
                    ]
                    PlasmaComponents3.ToolButton {
                        required property var modelData
                        Layout.fillWidth: true
                        Layout.preferredWidth: 1
                        Layout.preferredHeight: Kirigami.Units.gridUnit * 4
                        display: PlasmaComponents3.AbstractButton.TextUnderIcon
                        icon.name: modelData.icon
                        icon.width: Kirigami.Units.iconSizes.medium
                        icon.height: Kirigami.Units.iconSizes.medium
                        text: modelData.label
                        checked: !!root.settings && !!root.settings[modelData.key]
                        enabled: root.streaming
                        onClicked: root.set({ [modelData.key]: !checked })
                        PlasmaComponents3.ToolTip.text: modelData.tip
                        PlasmaComponents3.ToolTip.visible: hovered
                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                enabled: root.streaming
                PlasmaComponents3.Label { text: "Picture"; Layout.fillWidth: true }
                Repeater {
                    model: [
                        { value: "sharp", label: "Sharp", tip: "Best picture" },
                        { value: "light", label: "Light", tip: "Less data, for slower networks" },
                    ]
                    PlasmaComponents3.ToolButton {
                        required property var modelData
                        text: modelData.label
                        checked: !!root.settings && root.settings.quality === modelData.value
                        onClicked: root.set({ quality: modelData.value })
                        PlasmaComponents3.ToolTip.text: modelData.tip
                        PlasmaComponents3.ToolTip.visible: hovered
                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                    }
                }
            }

            Kirigami.Separator { Layout.fillWidth: true }

            RowLayout {
                Layout.fillWidth: true
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    Kirigami.Heading { level: 4; text: "Apps" }
                    PlasmaComponents3.Label {
                        text: "Click to open · switch off to keep on the Mac"
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                }
                PlasmaExtras.SearchField {
                    id: search
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 10
                }
            }

            PlasmaComponents3.ScrollView {
                Layout.fillWidth: true
                Layout.fillHeight: true
                ListView {
                    id: appList
                    clip: true
                    model: (root.st.apps || []).filter(a => a.name.toLowerCase().includes(search.text.toLowerCase()))
                    delegate: PlasmaComponents3.ItemDelegate {
                        required property var modelData
                        readonly property bool shown: !!root.settings && !root.settings.exclude.includes(modelData.id)
                        width: ListView.view.width
                        enabled: root.streaming
                        onClicked: root.act("launch", modelData.id)
                        PlasmaComponents3.ToolTip.text: "Open " + modelData.name + " on the Mac"
                        PlasmaComponents3.ToolTip.visible: hovered
                        PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
                        contentItem: RowLayout {
                            spacing: Kirigami.Units.largeSpacing
                            Kirigami.Icon {
                                source: modelData.icon || "application-x-executable"
                                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                            }
                            PlasmaComponents3.Label {
                                text: modelData.name
                                elide: Text.ElideRight
                                Layout.fillWidth: true
                            }
                            PlasmaComponents3.Switch {
                                checked: shown
                                onToggled: {
                                    const exclude = root.settings.exclude.filter(b => b !== modelData.id);
                                    if (!checked) exclude.push(modelData.id);
                                    root.set({ exclude: exclude });
                                    checked = Qt.binding(() => shown);
                                }
                                PlasmaComponents3.ToolTip.text: shown ? "Shown here" : "Stays on the Mac"
                                PlasmaComponents3.ToolTip.visible: hovered
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
