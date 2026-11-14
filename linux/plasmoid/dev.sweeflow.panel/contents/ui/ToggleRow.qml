// One setting: its name and a switch (a short explanation on hover). Clicking anywhere on the
// row flips it.
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ItemDelegate {
    id: row
    property string label
    property bool on
    /// An optional button beside the switch (shown while on), e.g. "Gather".
    property string actionText
    property string actionTip
    /// A short explanation, said on hover.
    property string hint
    signal flipped(bool value)
    signal extraClicked()

    Layout.fillWidth: true
    onClicked: flipped(!on)
    PlasmaComponents3.ToolTip.text: hint
    PlasmaComponents3.ToolTip.visible: hovered && hint !== ""
    PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        PlasmaComponents3.Label { text: row.label; elide: Text.ElideRight; Layout.fillWidth: true }
        PlasmaComponents3.ToolButton {
            visible: row.actionText !== "" && row.on && row.enabled
            text: row.actionText
            onClicked: row.extraClicked()
            PlasmaComponents3.ToolTip.text: row.actionTip
            PlasmaComponents3.ToolTip.visible: hovered && row.actionTip !== ""
            PlasmaComponents3.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
        VioletSwitch {
            checked: row.on
            onToggled: {
                row.flipped(checked);
                checked = Qt.binding(() => row.on);
            }
        }
    }
}
