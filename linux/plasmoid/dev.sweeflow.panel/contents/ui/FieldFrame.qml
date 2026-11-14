// A text field's frame with Sweeflow's violet when it has the keyboard: Plasma's own is drawn
// from its theme in the desktop's highlight colour.
import QtQuick
import org.kde.kirigami as Kirigami

Rectangle {
    property Item control
    property color violet: "#8b5cf6"
    implicitHeight: Kirigami.Units.gridUnit * 1.6
    radius: Kirigami.Units.cornerRadius
    color: Kirigami.Theme.backgroundColor
    border.width: control && control.activeFocus ? 2 : 1
    border.color: control && control.activeFocus ? violet
        : Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, control && control.hovered ? 0.4 : 0.2)
}
