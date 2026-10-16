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
        Layout.preferredHeight: Kirigami.Units.gridUnit * 27
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
                                readonly property int running: (root.st.apps || []).filter(a => a.state > 0).length
                                text: (running ? running + " running · " : "") + (hidden ? (total - hidden) + " of " + total : "All " + total)
                                opacity: 0.6
                            }
                            Kirigami.Icon { source: "go-next-symbolic"; Layout.preferredWidth: Kirigami.Units.iconSizes.small; Layout.preferredHeight: Kirigami.Units.iconSizes.small }
                        }
                    }

                    PlasmaComponents3.ItemDelegate {
                        Layout.fillWidth: true
                        enabled: root.streaming
                        onClicked: { root.act("display"); root.expanded = false; }
                        contentItem: RowLayout {
                            spacing: Kirigami.Units.largeSpacing
                            Kirigami.Icon { source: "monitor-symbolic"; Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium; Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium }
                            ColumnLayout {
                                spacing: 0
                                Layout.fillWidth: true
                                PlasmaComponents3.Label { text: "Show Mac display"; Layout.fillWidth: true }
                                PlasmaComponents3.Label { text: "The whole Mac screen in a window"; font: Kirigami.Theme.smallFont; opacity: 0.6; Layout.fillWidth: true; elide: Text.ElideRight }
                            }
                        }
                    }

                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    Repeater {
                        model: [
                            { key: "sound", icon: "audio-volume-high-symbolic", label: "Play Mac sound", hint: "Mac audio plays on Linux" },
                            { key: "clipboard", icon: "edit-paste-symbolic", label: "Shared clipboard", hint: "Copy on one, paste on the other" },
                            { key: "dragDrop", icon: "transform-move-symbolic", label: "Drag and drop files", hint: "Between Mac windows and Linux" },
                            { key: "dim", icon: "brightness-low-symbolic", label: "Dim Mac screen", hint: "While its apps are streaming" },
                            { key: "desktop", icon: "virtual-desktops-symbolic", label: "Own desktop", hint: "Mac apps open on a desktop of their own" },
                            { key: "launcher", icon: "application-menu-symbolic", label: "Mac apps in Linux menu", hint: "Listed alongside your Linux apps" },
                            { key: "sharp", icon: "view-preview-symbolic", label: "High quality", hint: "Turn off for slower connections" },
                        ]
                        delegate: ToggleRow {
                            required property var modelData
                            iconName: modelData.icon
                            label: modelData.label
                            hint: modelData.hint
                            on: !!root.settings && (modelData.key === "sharp" ? root.settings.quality !== "light" : !!root.settings[modelData.key])
                            enabled: root.streaming
                            onFlipped: (v) => root.set(modelData.key === "sharp" ? { quality: v ? "sharp" : "light" } : { [modelData.key]: v })
                            actionText: modelData.key === "desktop" ? "Gather" : ""
                            actionIcon: "view-refresh-symbolic"
                            actionTip: "Bring every Mac window back to the Mac desktop"
                            onExtraClicked: root.act("gather")
                        }
                    }

                    Kirigami.Separator { Layout.fillWidth: true; Layout.topMargin: Kirigami.Units.smallSpacing; Layout.bottomMargin: Kirigami.Units.smallSpacing }

                    ToggleRow {
                        iconName: "system-run-symbolic"
                        label: "Open at login"
                        hint: "Start Sweeflow when you log in"
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
                    text: "Click to open · switch off to keep it on the Mac"
                    font: Kirigami.Theme.smallFont
                    opacity: 0.7
                }
                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    ListView {
                        id: appList
                        clip: true
                        // Running first (the client sorts them so), each group under a heading.
                        model: (root.st.apps || []).filter(a => a.name.toLowerCase().includes(search.text.toLowerCase()))
                        delegate: ColumnLayout {
                            id: appRow
                            required property var modelData
                            required property int index
                            readonly property bool shown: !!root.settings && !root.settings.exclude.includes(modelData.id)
                            readonly property bool firstOfGroup: index === 0
                                || (appList.model[index - 1].state > 0) !== (modelData.state > 0)
                            width: ListView.view.width
                            spacing: 0
                            Kirigami.Heading {
                                visible: appRow.firstOfGroup
                                level: 5
                                text: appRow.modelData.state > 0 ? "Running" : "Not running"
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
                                        visible: appRow.modelData.state > 0
                                        text: ["", "Running, no window", "Minimised", "Open"][appRow.modelData.state] || ""
                                        font: Kirigami.Theme.smallFont
                                        color: appRow.modelData.state === 3 ? Kirigami.Theme.positiveTextColor : Kirigami.Theme.textColor
                                        opacity: appRow.modelData.state === 3 ? 0.9 : 0.6
                                    }
                                }
                                PlasmaComponents3.ToolButton {
                                    visible: appRow.modelData.state > 0
                                    icon.name: "application-exit"
                                    onClicked: root.act("quit-app", appRow.modelData.id)
                                    PlasmaComponents3.ToolTip.text: "Quit " + appRow.modelData.name
                                    PlasmaComponents3.ToolTip.visible: hovered
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
            iconName: "sweeflow"
            text: !root.running ? "Sweeflow isn't running" : "No Mac paired yet"
            explanation: !root.running ? "Start it to see your Mac's apps here." : "Add a Mac to stream its apps to this desktop."
        }
    }
}
