// One setting: icon, what it does, and a switch. Clicking anywhere on the row flips it.
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ItemDelegate {
    id: row
    property string iconName
    property string label
    property string hint
    property bool on
    signal flipped(bool value)

    Layout.fillWidth: true
    onClicked: flipped(!on)

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        Kirigami.Icon {
            source: row.iconName
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
        }
        ColumnLayout {
            spacing: 0
            Layout.fillWidth: true
            PlasmaComponents3.Label { text: row.label; elide: Text.ElideRight; Layout.fillWidth: true }
            PlasmaComponents3.Label {
                text: row.hint
                font: Kirigami.Theme.smallFont
                opacity: 0.7
                elide: Text.ElideRight
                Layout.fillWidth: true
            }
        }
        PlasmaComponents3.Switch {
            checked: row.on
            onToggled: {
                row.flipped(checked);
                checked = Qt.binding(() => row.on);
            }
        }
    }
}
