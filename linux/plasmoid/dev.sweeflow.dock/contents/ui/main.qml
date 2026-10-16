// The Mac Dock: the Mac's apps as a row of icons for any panel, with the Dock's dots (solid:
// a window is open; ring: only minimised ones; faint: running with no window). Pinned apps stay
// when not running. Reads the client over D-Bus, like the tray widget (src/panel.rs).

import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.components as PlasmaComponents3
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
        service: watcher.registered ? root.service : ""
        path: root.objectPath
        iface: root.ifaceName
    }
    readonly property var st: {
        const json = props.properties["State"];
        try { return json ? JSON.parse(json) : {}; } catch (e) { return {}; }
    }

    function act(action, arg) {
        DBus.SessionBus.asyncCall({ service: root.service, path: root.objectPath, iface: root.ifaceName,
                                    member: "Do", arguments: [action, arg || ""] });
    }

    readonly property var pinned: Plasmoid.configuration.pinned || []
    function setPinned(id, on) {
        const p = pinned.filter(b => b !== id);
        if (on) p.push(id);
        Plasmoid.configuration.pinned = p;
    }

    // Pinned (in their order), then the other running apps by name.
    readonly property var items: {
        const apps = root.st.apps || [];
        const byId = {};
        for (const a of apps) byId[a.id] = a;
        const out = pinned.filter(id => byId[id]).map(id => byId[id]);
        for (const a of apps) if (a.state > 0 && !pinned.includes(a.id)) out.push(a);
        return out;
    }

    readonly property bool vertical: Plasmoid.formFactor === PlasmaCore.Types.Vertical
    preferredRepresentation: fullRepresentation
    Plasmoid.backgroundHints: PlasmaCore.Types.DefaultBackground | PlasmaCore.Types.ConfigurableBackground
    toolTipMainText: "Mac Dock"

    fullRepresentation: GridLayout {
        id: grid
        // In a panel: as thick as the panel. Elsewhere (the desktop): Dock-sized icons.
        readonly property bool inPanel: Plasmoid.formFactor === PlasmaCore.Types.Horizontal || root.vertical
        readonly property real thickness: root.vertical ? width : height
        readonly property real cell: inPanel && thickness > 0
            ? Math.max(Kirigami.Units.iconSizes.small, thickness)
            : Kirigami.Units.iconSizes.huge
        flow: root.vertical ? GridLayout.TopToBottom : GridLayout.LeftToRight
        rows: root.vertical ? -1 : 1
        columns: root.vertical ? 1 : -1
        rowSpacing: 0
        columnSpacing: 0
        Layout.minimumWidth: root.vertical ? -1 : Math.max(1, root.items.length) * cell
        Layout.minimumHeight: root.vertical ? Math.max(1, root.items.length) * cell : -1
        Layout.preferredWidth: Layout.minimumWidth
        Layout.preferredHeight: Layout.minimumHeight

        // Nothing to show: the logo, so the widget can still be found (and placed).
        Kirigami.Icon {
            visible: root.items.length === 0
            source: "sweeflow"
            opacity: 0.5
            Layout.preferredWidth: grid.cell * 0.7
            Layout.preferredHeight: grid.cell * 0.7
            Layout.alignment: Qt.AlignCenter
        }

        Repeater {
            model: root.items
            delegate: MouseArea {
                id: cellArea
                required property var modelData
                readonly property bool isPinned: root.pinned.includes(modelData.id)
                Layout.preferredWidth: grid.cell
                Layout.preferredHeight: grid.cell
                hoverEnabled: true
                acceptedButtons: Qt.LeftButton | Qt.RightButton
                onClicked: (mouse) => {
                    if (mouse.button === Qt.RightButton) menu.popup();
                    else root.act("launch", modelData.id);
                }

                PlasmaCore.ToolTipArea {
                    anchors.fill: parent
                    mainText: cellArea.modelData.name
                    subText: ["Not running", "Running, no window", "Minimised", "Open"][cellArea.modelData.state] || ""
                }
                Rectangle {
                    anchors.fill: parent
                    anchors.margins: 2
                    radius: Kirigami.Units.cornerRadius
                    color: Kirigami.Theme.highlightColor
                    opacity: cellArea.containsMouse ? 0.2 : 0
                    Behavior on opacity { NumberAnimation { duration: Kirigami.Units.shortDuration } }
                }
                Kirigami.Icon {
                    anchors.centerIn: parent
                    anchors.verticalCenterOffset: root.vertical ? 0 : -grid.cell * 0.06
                    width: grid.cell * 0.72
                    height: width
                    source: cellArea.modelData.icon || "application-x-executable"
                    scale: cellArea.pressed ? 0.9 : 1
                    Behavior on scale { NumberAnimation { duration: Kirigami.Units.shortDuration } }
                }
                // The Dock's dot.
                Rectangle {
                    visible: cellArea.modelData.state > 0
                    readonly property real size: Math.max(4, grid.cell * 0.08)
                    width: size
                    height: size
                    radius: size / 2
                    anchors.horizontalCenter: root.vertical ? undefined : parent.horizontalCenter
                    anchors.bottom: root.vertical ? undefined : parent.bottom
                    anchors.bottomMargin: 1
                    anchors.verticalCenter: root.vertical ? parent.verticalCenter : undefined
                    anchors.left: root.vertical ? parent.left : undefined
                    anchors.leftMargin: 1
                    color: cellArea.modelData.state === 2 ? "transparent" : Kirigami.Theme.textColor
                    border.color: Kirigami.Theme.textColor
                    border.width: cellArea.modelData.state === 2 ? 1 : 0
                    opacity: cellArea.modelData.state === 1 ? 0.45 : 0.9
                }

                PlasmaComponents3.Menu {
                    id: menu
                    PlasmaComponents3.MenuItem { text: cellArea.modelData.name; enabled: false }
                    PlasmaComponents3.MenuSeparator {}
                    PlasmaComponents3.MenuItem {
                        icon.name: "window-new"
                        text: cellArea.modelData.state > 0 ? "Show" : "Open"
                        onTriggered: root.act("launch", cellArea.modelData.id)
                    }
                    PlasmaComponents3.MenuItem {
                        icon.name: cellArea.isPinned ? "window-unpin" : "window-pin"
                        text: cellArea.isPinned ? "Remove from Dock" : "Keep in Dock"
                        onTriggered: root.setPinned(cellArea.modelData.id, !cellArea.isPinned)
                    }
                    PlasmaComponents3.MenuSeparator { visible: cellArea.modelData.state > 0 }
                    PlasmaComponents3.MenuItem {
                        visible: cellArea.modelData.state > 0
                        icon.name: "application-exit"
                        text: "Quit"
                        onTriggered: root.act("quit-app", cellArea.modelData.id)
                    }
                }
            }
        }
    }
}
