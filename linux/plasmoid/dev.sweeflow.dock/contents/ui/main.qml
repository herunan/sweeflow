// The Mac Dock: the Mac's pinned and running apps as a row of icons. Under each: nothing when
// not running, a dot when running without a window here, a bar when it has one. Hovering shows live previews of the app's windows here (as the task manager
// does); clicking brings its window up, or opens the app. Reads the client over D-Bus, like the
// tray widget (src/panel.rs), and the windows from Plasma's task manager model.

import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.components as PlasmaComponents3
import org.kde.plasma.extras as PlasmaExtras
import org.kde.kirigami as Kirigami
import org.kde.plasma.workspace.dbus as DBus
import org.kde.taskmanager as TaskManager
import org.kde.pipewire as PipeWire
import org.kde.kitemmodels as KItemModels

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

    // Every window, to find each Mac app's (app id "sweeflow-<bundle id>").
    TaskManager.TasksModel {
        id: tasks
        groupMode: TaskManager.TasksModel.GroupDisabled
        filterByVirtualDesktop: false
        filterByScreen: false
        filterByActivity: false
        filterMinimized: false
    }

    // Pins are Sweeflow's (shared with the tray's Apps page).
    readonly property var pinned: root.st.pinned || []
    function setPinned(id, on) { root.act(on ? "pin" : "unpin", id); }

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

                // This app's windows here.
                KItemModels.KSortFilterProxyModel {
                    id: wins
                    sourceModel: tasks
                    filterRowCallback: (row, parent) =>
                        String(tasks.data(tasks.index(row, 0), TaskManager.AbstractTasksModel.AppId)).replace(/\.desktop$/, "")
                            === "sweeflow-" + cellArea.modelData.id
                }
                function activate(i) { tasks.requestActivate(wins.mapToSource(wins.index(i, 0))); }

                onClicked: (mouse) => {
                    if (mouse.button === Qt.RightButton) { menu.openRelative(); return; }
                    // Its window comes up (one already active goes down, as in the task manager);
                    // no window here: open the app on the Mac.
                    if (wins.count === 0) { root.act("launch", modelData.id); return; }
                    const first = wins.index(0, 0);
                    if (wins.count === 1 && wins.data(first, TaskManager.AbstractTasksModel.IsActive))
                        tasks.requestToggleMinimized(wins.mapToSource(first));
                    else
                        activate(0);
                }

                PlasmaCore.ToolTipArea {
                    anchors.fill: parent
                    interactive: wins.count > 0
                    mainText: wins.count > 0 ? "" : cellArea.modelData.name
                    subText: wins.count > 0 ? "" : (cellArea.modelData.state > 0 ? "No window open" : "Not running")
                    mainItem: wins.count > 0 ? previews : null
                    ColumnLayout {
                        id: previews
                        visible: false
                        spacing: Kirigami.Units.smallSpacing
                        PlasmaComponents3.Label { text: cellArea.modelData.name; font.bold: true }
                        RowLayout {
                            spacing: Kirigami.Units.smallSpacing
                            Repeater {
                                model: wins
                                delegate: MouseArea {
                                    id: thumb
                                    required property var model
                                    required property int index
                                    readonly property string winId: (model.WinIdList || [])[0] || ""
                                    Layout.preferredWidth: Kirigami.Units.gridUnit * 12
                                    Layout.preferredHeight: Kirigami.Units.gridUnit * 8 + title.height
                                    hoverEnabled: true
                                    onClicked: cellArea.activate(index)
                                    Rectangle {
                                        anchors.fill: parent
                                        radius: Kirigami.Units.cornerRadius
                                        color: Kirigami.Theme.highlightColor
                                        opacity: thumb.containsMouse ? 0.25 : 0
                                    }
                                    TaskManager.ScreencastingRequest { id: cast; uuid: thumb.winId }
                                    PipeWire.PipeWireSourceItem {
                                        anchors { left: parent.left; right: parent.right; top: parent.top; margins: Kirigami.Units.smallSpacing }
                                        height: Kirigami.Units.gridUnit * 8 - Kirigami.Units.smallSpacing
                                        nodeId: cast.nodeId
                                        visible: nodeId > 0
                                    }
                                    Kirigami.Icon {
                                        anchors.centerIn: parent
                                        visible: cast.nodeId <= 0
                                        width: Kirigami.Units.iconSizes.large
                                        height: width
                                        source: cellArea.modelData.icon || "application-x-executable"
                                    }
                                    PlasmaComponents3.Label {
                                        id: title
                                        anchors { left: parent.left; right: parent.right; bottom: parent.bottom; margins: Kirigami.Units.smallSpacing }
                                        text: thumb.model.display || ""
                                        elide: Text.ElideRight
                                        horizontalAlignment: Text.AlignHCenter
                                        font: Kirigami.Theme.smallFont
                                    }
                                }
                            }
                        }
                    }
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
                // Running: a dot; a window open here (or minimised): a bar.
                Rectangle {
                    visible: cellArea.modelData.state > 0
                    readonly property real size: Math.max(4, grid.cell * 0.07)
                    readonly property bool windows: cellArea.modelData.state >= 2 || wins.count > 0
                    width: root.vertical ? size : (windows ? grid.cell * 0.32 : size)
                    height: root.vertical ? (windows ? grid.cell * 0.32 : size) : size
                    radius: size / 2
                    Behavior on width { NumberAnimation { duration: Kirigami.Units.shortDuration } }
                    Behavior on height { NumberAnimation { duration: Kirigami.Units.shortDuration } }
                    anchors.horizontalCenter: root.vertical ? undefined : parent.horizontalCenter
                    anchors.bottom: root.vertical ? undefined : parent.bottom
                    anchors.bottomMargin: 1
                    anchors.verticalCenter: root.vertical ? parent.verticalCenter : undefined
                    anchors.left: root.vertical ? parent.left : undefined
                    anchors.leftMargin: 1
                    color: windows ? Kirigami.Theme.highlightColor : Kirigami.Theme.textColor
                    opacity: windows ? 1 : 0.7
                }

                // A native menu in a window of its own: one drawn inside the panel gets clipped
                // by the panel's edge.
                PlasmaExtras.Menu {
                    id: menu
                    visualParent: cellArea
                    placement: Plasmoid.location === PlasmaCore.Types.TopEdge ? PlasmaExtras.Menu.BottomPosedLeftAlignedPopup
                             : Plasmoid.location === PlasmaCore.Types.LeftEdge ? PlasmaExtras.Menu.RightPosedTopAlignedPopup
                             : Plasmoid.location === PlasmaCore.Types.RightEdge ? PlasmaExtras.Menu.LeftPosedTopAlignedPopup
                             : PlasmaExtras.Menu.TopPosedLeftAlignedPopup
                    PlasmaExtras.MenuItem { text: cellArea.modelData.name; section: true }
                    PlasmaExtras.MenuItem {
                        text: "Open"
                        onClicked: root.act("launch", cellArea.modelData.id)
                    }
                    PlasmaExtras.MenuItem {
                        text: cellArea.isPinned ? "Remove from Dock" : "Keep in Dock"
                        onClicked: root.setPinned(cellArea.modelData.id, !cellArea.isPinned)
                    }
                    PlasmaExtras.MenuItem { separator: true; visible: cellArea.modelData.state > 0 }
                    PlasmaExtras.MenuItem {
                        visible: cellArea.modelData.state > 0
                        text: "Quit"
                        onClicked: root.act("quit-app", cellArea.modelData.id)
                    }
                }
            }
        }
    }
}
