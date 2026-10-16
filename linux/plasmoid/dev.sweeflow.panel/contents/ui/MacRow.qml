// A linked Mac: click to stream from it; pencil renames it in place; bin forgets it (asks once).
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

    onClicked: if (!editing && !active) activated()

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
        ColumnLayout {
            spacing: 0
            Layout.fillWidth: true
            PlasmaComponents3.Label { visible: !row.editing; text: row.name; elide: Text.ElideRight; Layout.fillWidth: true }
            PlasmaComponents3.TextField {
                id: field
                visible: row.editing
                Layout.fillWidth: true
                onAccepted: row.finishEdit()
                Keys.onEscapePressed: row.editing = false
                onActiveFocusChanged: if (!activeFocus) row.finishEdit()
            }
            PlasmaComponents3.Label {
                visible: !row.editing
                text: row.active ? "Streaming from this Mac" : "Click to switch to it"
                font: Kirigami.Theme.smallFont
                opacity: 0.6
            }
        }
        PlasmaComponents3.ToolButton {
            visible: !row.confirming
            icon.name: row.editing ? "dialog-ok-apply" : "document-edit"
            onClicked: {
                if (row.editing) { row.finishEdit(); return; }
                field.text = row.name;
                row.editing = true;
                field.forceActiveFocus();
                field.selectAll();
            }
            PlasmaComponents3.ToolTip.text: row.editing ? "Save" : "Rename"
            PlasmaComponents3.ToolTip.visible: hovered
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing
            icon.name: "edit-delete-remove"
            text: row.confirming ? "Forget" : ""
            onClicked: row.confirming ? row.forgotten() : (row.confirming = true)
            PlasmaComponents3.ToolTip.text: row.confirming ? "Click again to unlink this Mac" : "Forget"
            PlasmaComponents3.ToolTip.visible: hovered
        }
    }
}
