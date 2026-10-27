// A linked Mac: "Use" streams from it instead; the overflow menu renames it (in place) or forgets
// it (asks once).
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ItemDelegate {
    id: row
    property string name
    property bool active
    property bool editing: false
    property bool confirming: false
    signal activated()
    signal renamed(string name)
    signal forgotten()

    // Nothing happens on a stray click: switching Macs is the "Use" button.
    hoverEnabled: false
    down: false

    function finishEdit() {
        if (editing && field.text.trim() !== "" && field.text !== name) renamed(field.text);
        editing = false;
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        Kirigami.Icon {
            source: "computer-laptop"
            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
        }
        PlasmaComponents3.Label {
            visible: !row.editing
            text: row.confirming ? "Forget " + row.name + "? Linking it again needs a new code." : row.name
            font.bold: row.active && !row.confirming
            wrapMode: row.confirming ? Text.Wrap : Text.NoWrap
            elide: Text.ElideRight
            Layout.fillWidth: true
        }
        PlasmaComponents3.TextField {
            id: field
            visible: row.editing
            Layout.fillWidth: true
            onAccepted: row.finishEdit()
            Keys.onEscapePressed: row.editing = false
            onActiveFocusChanged: if (!activeFocus) row.finishEdit()
        }
        PlasmaComponents3.Label {
            visible: row.active && !row.editing && !row.confirming
            text: "In use"
            opacity: 0.6
        }
        PlasmaComponents3.ToolButton {
            visible: !row.active && !row.editing && !row.confirming
            text: "Use"
            onClicked: row.activated()
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
        PlasmaComponents3.ToolButton {
            id: more
            visible: !row.editing && !row.confirming
            icon.name: "overflow-menu"
            PlasmaComponents3.ToolTip.text: "More"
            PlasmaComponents3.ToolTip.visible: hovered
            onClicked: menu.popup(more, 0, more.height)
            PlasmaComponents3.Menu {
                id: menu
                PlasmaComponents3.MenuItem {
                    icon.name: "document-edit"
                    text: "Rename"
                    onTriggered: {
                        field.text = row.name;
                        row.editing = true;
                        field.forceActiveFocus();
                        field.selectAll();
                    }
                }
                PlasmaComponents3.MenuItem {
                    icon.name: "edit-delete-remove"
                    text: "Forget…"
                    onTriggered: row.confirming = true
                }
            }
        }
    }
}
