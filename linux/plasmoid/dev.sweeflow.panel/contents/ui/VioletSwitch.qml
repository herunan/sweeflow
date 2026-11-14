// A switch in Sweeflow's violet: Plasma's own is drawn from its theme in the desktop's
// highlight colour, whatever the widget's colours say.
import QtQuick
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

PlasmaComponents3.Switch {
    id: control
    property color violet: "#8b5cf6"
    indicator: Rectangle {
        implicitWidth: Math.round(Kirigami.Units.gridUnit * 1.8)
        implicitHeight: Math.round(Kirigami.Units.gridUnit * 0.9)
        x: control.leftPadding
        y: control.topPadding + (control.availableHeight - height) / 2
        radius: height / 2
        color: control.checked ? control.violet : Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, 0.2)
        border.width: control.visualFocus ? 2 : 0
        border.color: Qt.lighter(control.violet, 1.3)
        opacity: control.enabled ? 1 : 0.5
        Rectangle {
            width: parent.height - 4
            height: width
            radius: width / 2
            y: 2
            x: control.checked ? parent.width - width - 2 : 2
            color: control.checked ? "white" : Kirigami.Theme.textColor
            Behavior on x { NumberAnimation { duration: Kirigami.Units.shortDuration } }
        }
    }
}
