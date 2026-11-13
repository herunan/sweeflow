// Mac apps still running with no window here, right after the task manager's tasks and drawn
// like them: what the taskbar can't list itself, since it only knows windows. When an app's window
// comes, its icon goes (the task manager shows the window); close it, and the icon is back.
// Click: the Mac opens the app (as its Dock icon would). Right-click: Open / Quit. Reads the
// client over D-Bus, like the tray widget (src/panel.rs).

import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.extras as PlasmaExtras
import org.kde.kirigami as Kirigami
import org.kde.ksvg as KSvg
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

    // The Mac's screen is locked: every window is paused there, so its apps all show here.
    readonly property bool locked: !!root.st.locked

    // State 1: running on the Mac with no window here. Apps switched off in Sweeflow's list
    // aren't streamed (always "no window"), so they're left out.
    readonly property var windowless: {
        const exclude = (root.st.settings && root.st.settings.exclude) || [];
        return (root.st.apps || []).filter(a => a.state === 1 && !exclude.includes(a.id));
    }
    // In the order they lost their last window, newest last (as the task manager adds windows).
    property var order: []
    onWindowlessChanged: {
        const ids = root.windowless.map(a => a.id);
        const kept = root.order.filter(id => ids.includes(id));
        for (const id of ids) if (!kept.includes(id)) kept.push(id);
        if (kept.join() !== root.order.join()) root.order = kept;
    }
    readonly property var items: root.order.map(id => root.windowless.find(a => a.id === id)).filter(a => a)

    // The task manager's frame names: by panel edge first, then plain.
    readonly property string edge: Plasmoid.location === PlasmaCore.Types.TopEdge ? "north"
        : Plasmoid.location === PlasmaCore.Types.LeftEdge ? "west"
        : Plasmoid.location === PlasmaCore.Types.RightEdge ? "east" : "south"
    // Its frame's sides, for each task's size.
    KSvg.FrameSvgItem {
        id: metrics
        visible: false
        imagePath: "widgets/tasks"
        prefix: [root.edge + "-normal", "normal"]
    }

    readonly property bool vertical: Plasmoid.formFactor === PlasmaCore.Types.Vertical
    preferredRepresentation: fullRepresentation
    Plasmoid.backgroundHints: PlasmaCore.Types.NoBackground
    // The panel's full thickness, margins included, as the task manager takes.
    Plasmoid.constraintHints: Plasmoid.CanFillArea
    // Nothing to show: no room taken, except while editing the panel (so it can be found).
    Plasmoid.status: root.items.length ? PlasmaCore.Types.ActiveStatus : PlasmaCore.Types.PassiveStatus

    // Takes the panel's free room (as the task manager did before it stopped filling), with the
    // icons at its start: right after the tasks, and no gap left at the panel's end.
    fullRepresentation: Item {
        id: grid
        readonly property real thickness: root.vertical ? width : height
        readonly property real cell: thickness > 0 ? thickness : Kirigami.Units.iconSizes.large
        readonly property bool editing: Plasmoid.containment && Plasmoid.containment.corona
            && Plasmoid.containment.corona.editMode
        // As the task manager's icon-only tasks: as thick as the panel, plus the frame's sides.
        readonly property real taskLength: cell + (root.vertical ? metrics.margins.top + metrics.margins.bottom
                                                                 : metrics.margins.left + metrics.margins.right)
        // Sized from the count (not the children), so it shrinks the moment an icon goes.
        readonly property real length: root.items.length ? root.items.length * taskLength : (editing ? cell : 0)
        Layout.minimumWidth: root.vertical ? -1 : length
        Layout.minimumHeight: root.vertical ? length : -1
        Layout.preferredWidth: Layout.minimumWidth
        Layout.preferredHeight: Layout.minimumHeight
        Layout.fillWidth: !root.vertical
        Layout.fillHeight: root.vertical

        Kirigami.Icon {
            visible: root.items.length === 0 && grid.editing
            source: "sweeflow"
            opacity: 0.5
            width: grid.cell * 0.6
            height: width
            x: (grid.cell - width) / 2
            y: (grid.cell - height) / 2
        }

        Grid {
            // Back over the panel's spacing between widgets, so the icons follow the tasks as
            // closely as the tasks follow each other.
            x: root.vertical ? 0 : -Kirigami.Units.smallSpacing
            y: root.vertical ? -Kirigami.Units.smallSpacing : 0
            rows: root.vertical ? -1 : 1
            columns: root.vertical ? 1 : -1
            Repeater {
                model: root.items
                delegate: MouseArea {
                    id: cellArea
                    required property var modelData
                    width: root.vertical ? grid.cell : grid.taskLength
                    height: root.vertical ? grid.taskLength : grid.cell
                    hoverEnabled: true
                    acceptedButtons: Qt.LeftButton | Qt.RightButton

                    onClicked: (mouse) => {
                        if (mouse.button === Qt.RightButton) menu.openRelative();
                        else root.act("launch", modelData.id);
                    }

                    KSvg.FrameSvgItem {
                        id: frame
                        anchors.fill: parent
                        imagePath: "widgets/tasks"
                        prefix: cellArea.containsMouse
                            ? [root.edge + "-normal-hover", "normal-hover", root.edge + "-hover", "hover", root.edge + "-normal", "normal"]
                            : [root.edge + "-normal", "normal"]
                    }
                    // The task manager's icon inset.
                    readonly property int iconMargin: Math.round(Kirigami.Units.smallSpacing / 4)
                    Kirigami.Icon {
                        id: icon
                        anchors {
                            fill: parent
                            leftMargin: frame.margins.left + iconMargin
                            rightMargin: frame.margins.right + iconMargin
                            topMargin: frame.margins.top + iconMargin
                            bottomMargin: frame.margins.bottom + iconMargin
                        }
                        source: cellArea.modelData.taskIcon || cellArea.modelData.icon || "application-x-executable"
                        active: cellArea.containsMouse
                        scale: cellArea.pressed ? 0.9 : 1
                        Behavior on scale { NumberAnimation { duration: Kirigami.Units.shortDuration } }
                        opacity: root.locked ? 0.6 : 1
                        // Locked Mac: a small lock in the corner (the Sweeflow badge has the other one), so it's plain
                        // why there's no window.
                        Kirigami.Icon {
                            visible: root.locked
                            anchors { left: parent.left; bottom: parent.bottom }
                            width: Math.round(parent.width * 0.45)
                            height: width
                            source: "system-lock-screen-symbolic"
                        }
                    }

                    PlasmaCore.ToolTipArea {
                        anchors.fill: parent
                        mainText: cellArea.modelData.name
                        subText: root.locked ? "Your Mac is locked: unlock it to see this app's windows"
                                             : "Running on your Mac, no window open"
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
                            icon: "window-new"
                            text: "Open"
                            onClicked: root.act("launch", cellArea.modelData.id)
                        }
                        PlasmaExtras.MenuItem { separator: true }
                        PlasmaExtras.MenuItem {
                            icon: "application-exit"
                            text: "Quit"
                            onClicked: root.act("quit-app", cellArea.modelData.id)
                        }
                    }
                }
            }
        }
    }
}
