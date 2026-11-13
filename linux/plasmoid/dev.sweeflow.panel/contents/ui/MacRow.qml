// A linked Mac: its name (click it to rename it in place), Online / Not online, its stream
// actions (for the Mac in use those of its state; for another, Start, which switches to it),
// Rename, and Unlink last (asks once).
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ItemDelegate {
    id: row
    property string name
    /// "laptop", "imac" or "desktop" (Mac mini, Studio, Pro); "" until it has said.
    property string kind
    property bool active
    /// Reachable on the network: true, false, or null while not known yet.
    property var online: null
    /// Do actions ("start", "pause", "resume", "stop"), in order.
    property var actions: []
    property var actionLabels: ({})
    property bool editing: false
    property bool confirming: false
    signal actionAsked(string action)
    signal renamed(string name)
    signal unlinked()

    // Nothing happens on a stray click on the row: the buttons do things, the name renames.
    hoverEnabled: false
    down: false

    function startEdit() {
        field.text = row.name;
        row.editing = true;
        field.forceActiveFocus();
        field.selectAll();
    }

    function finishEdit() {
        if (editing && field.text.trim() !== "" && field.text !== name) renamed(field.text);
        editing = false;
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        Kirigami.Icon {
            source: row.kind === "desktop" ? "computer" : row.kind === "imac" ? "video-display" : "computer-laptop"
            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
        }
        ColumnLayout {
            visible: !row.editing
            spacing: 0
            Layout.fillWidth: true
            PlasmaComponents3.Label {
                id: nameLabel
                text: row.confirming ? "Unlink " + row.name + "? Linking it again needs a new code." : row.name
                font.bold: row.active && !row.confirming
                font.underline: nameArea.containsMouse && !row.confirming
                wrapMode: row.confirming ? Text.Wrap : Text.NoWrap
                elide: Text.ElideRight
                Layout.fillWidth: true
                MouseArea {
                    id: nameArea
                    anchors.fill: parent
                    enabled: !row.confirming
                    hoverEnabled: true
                    cursorShape: Qt.IBeamCursor
                    onClicked: row.startEdit()
                }
                PlasmaComponents3.ToolTip.text: "Click to rename"
                PlasmaComponents3.ToolTip.visible: nameArea.containsMouse
                PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
            }
            // A status, never an action.
            PlasmaComponents3.Label {
                visible: !row.confirming && row.online !== null
                text: row.online ? "Online" : "Not online"
                font: Kirigami.Theme.smallFont
                opacity: 0.7
            }
        }
        PlasmaComponents3.TextField {
            id: field
            visible: row.editing
            Layout.fillWidth: true
            onAccepted: row.finishEdit()
            Keys.onEscapePressed: row.editing = false
            onActiveFocusChanged: if (!activeFocus) row.finishEdit()
        }
        Repeater {
            model: !row.editing && !row.confirming ? row.actions : []
            delegate: PlasmaComponents3.ToolButton {
                required property string modelData
                text: row.actionLabels[modelData] || modelData
                onClicked: row.actionAsked(modelData)
                PlasmaComponents3.ToolTip.text: row.active ? "" : "Stream from this Mac instead"
                PlasmaComponents3.ToolTip.visible: hovered && !row.active
                PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
            }
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing && !row.confirming
            icon.name: "edit-rename"
            text: "Rename"
            onClicked: row.startEdit()
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing && !row.confirming
            icon.name: "edit-delete-remove"
            text: "Unlink"
            onClicked: row.confirming = true
            PlasmaComponents3.ToolTip.text: "Unlink this Mac (linking it again needs a new code)"
            PlasmaComponents3.ToolTip.visible: hovered
            PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
        PlasmaComponents3.ToolButton {
            visible: row.editing
            icon.name: "dialog-ok-apply"
            text: "Save"
            onClicked: row.finishEdit()
        }
        PlasmaComponents3.ToolButton {
            visible: row.confirming
            icon.name: "edit-delete-remove"
            text: "Unlink"
            onClicked: { row.confirming = false; row.unlinked(); }
        }
        PlasmaComponents3.ToolButton {
            visible: row.confirming
            text: "Cancel"
            onClicked: row.confirming = false
        }
    }
}
