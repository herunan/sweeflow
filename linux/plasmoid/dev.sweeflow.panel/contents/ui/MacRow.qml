// A linked Mac: click its name to rename it (in place). The Mac in use has Disconnect (Connect
// once disconnected), the others Use; every Mac has Forget (asks once).
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
    /// Disconnected (Disconnect): the Mac in use isn't streaming.
    property bool offline: false
    property bool editing: false
    property bool confirming: false
    signal activated()
    signal renamed(string name)
    signal forgotten()
    signal connectAsked()
    signal disconnectAsked()

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
        PlasmaComponents3.Label {
            id: nameLabel
            visible: !row.editing
            text: row.confirming ? "Forget " + row.name + "? Linking it again needs a new code." : row.name
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
        PlasmaComponents3.TextField {
            id: field
            visible: row.editing
            Layout.fillWidth: true
            onAccepted: row.finishEdit()
            Keys.onEscapePressed: row.editing = false
            onActiveFocusChanged: if (!activeFocus) row.finishEdit()
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing && !row.confirming && !row.active
            text: "Use"
            onClicked: row.activated()
            PlasmaComponents3.ToolTip.text: "Stream from this Mac instead"
            PlasmaComponents3.ToolTip.visible: hovered
            PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing && !row.confirming && row.active
            icon.name: row.offline ? "network-connect" : "network-disconnect"
            text: row.offline ? "Connect" : "Disconnect"
            onClicked: row.offline ? row.connectAsked() : row.disconnectAsked()
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing && !row.confirming
            icon.name: "edit-delete-remove"
            text: "Forget"
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
            text: "Forget"
            onClicked: { row.confirming = false; row.forgotten(); }
        }
        PlasmaComponents3.ToolButton {
            visible: row.confirming
            text: "Cancel"
            onClicked: row.confirming = false
        }
    }
}
