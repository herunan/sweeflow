// Sweeflow's button, as in its own windows (look.slint's Btn): a light fill and a thin line,
// both violet under the pointer (or while it's checked, its popup open).
import QtQuick
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.ToolButton {
    id: control
    readonly property color violet: "#8b5cf6"
    readonly property color ink: Kirigami.Theme.textColor
    readonly property bool lit: enabled && (hovered || checked || visualFocus)
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing
    topPadding: Kirigami.Units.smallSpacing
    bottomPadding: Kirigami.Units.smallSpacing
    background: Rectangle {
        implicitHeight: Kirigami.Units.gridUnit * 1.6
        implicitWidth: implicitHeight
        radius: Kirigami.Units.cornerRadius
        color: control.down ? Qt.rgba(0.545, 0.361, 0.965, 0.3)
             : control.lit ? Qt.rgba(0.545, 0.361, 0.965, 0.16)
             : Qt.rgba(control.ink.r, control.ink.g, control.ink.b, 0.08)
        border.width: 1
        border.color: control.lit ? control.violet : Qt.rgba(control.ink.r, control.ink.g, control.ink.b, 0.3)
    }
}
