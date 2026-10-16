// One setting: icon, name and a switch. Clicking anywhere on the row flips it.
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ItemDelegate {
    id: row
    property string iconName
    property string label
    property string tip
    property bool on
    signal flipped(bool value)

    Layout.fillWidth: true
    onClicked: flipped(!on)
    PlasmaComponents3.ToolTip.text: tip
    PlasmaComponents3.ToolTip.visible: hovered && tip !== ""
    PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        Kirigami.Icon {
            source: row.iconName
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
        }
        PlasmaComponents3.Label { text: row.label; elide: Text.ElideRight; Layout.fillWidth: true }
        PlasmaComponents3.Switch {
            checked: row.on
            onToggled: {
                row.flipped(checked);
                checked = Qt.binding(() => row.on);
            }
        }
    }
}
