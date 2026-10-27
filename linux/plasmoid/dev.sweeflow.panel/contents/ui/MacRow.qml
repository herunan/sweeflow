// A linked Mac: click to stream from it; Rename edits the name in place; Forget unlinks it (asks once).
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
        PlasmaComponents3.Label {
            visible: !row.editing
            text: row.name
            font.bold: row.active
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
            text: "Current"
            opacity: 0.6
        }
        PlasmaComponents3.ToolButton {
            visible: !row.confirming
            text: row.editing ? "Save" : "Rename"
            onClicked: {
                if (row.editing) { row.finishEdit(); return; }
                field.text = row.name;
                row.editing = true;
                field.forceActiveFocus();
                field.selectAll();
            }
        }
        PlasmaComponents3.ToolButton {
            visible: !row.editing
            text: row.confirming ? "Really forget?" : "Forget"
            onClicked: row.confirming ? row.forgotten() : (row.confirming = true)
            PlasmaComponents3.ToolTip.text: "Unlink this Mac (pairing again needs a code)"
            PlasmaComponents3.ToolTip.visible: hovered
        }
    }
}
