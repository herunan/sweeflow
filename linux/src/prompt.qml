// A Mac prompt that can't be streamed, shown here as a plain dialog: its text and its buttons,
// no icons (as macOS shows them). Run by blind.rs:
//   qml6 prompt.qml -- DEFAULT CANCEL HEADING TEXT INDEX LABEL [INDEX LABEL …]
// DEFAULT and CANCEL are button indexes (-1: none). Exits with 10 + the index of the button
// pressed; closing the window (exit 0) answers like the cancel button.
import QtQuick
import QtQuick.Controls

ApplicationWindow {
    id: win

    readonly property var args: {
        const all = Qt.application.arguments
        const at = all.indexOf("--")
        return at >= 0 ? all.slice(at + 1) : all.slice(2)
    }
    readonly property int defaultIndex: parseInt(args[0])
    readonly property int cancelIndex: parseInt(args[1])
    // Laid out from the right, so the last (the Mac's default) is rightmost: listed backwards.
    readonly property var buttons: {
        const list = []
        for (let i = 4; i + 1 < args.length; i += 2)
            list.unshift({ index: parseInt(args[i]), label: args[i + 1] })
        return list
    }

    // As tall as its text and buttons need at this width, and not resizable.
    readonly property int wantedHeight: content.implicitHeight + 40

    title: args[2] || ""
    flags: Qt.Dialog | Qt.CustomizeWindowHint | Qt.WindowTitleHint | Qt.WindowCloseButtonHint
    visible: true
    width: 480
    height: wantedHeight
    minimumWidth: 480
    maximumWidth: 480
    minimumHeight: wantedHeight
    maximumHeight: wantedHeight

    Column {
        id: content
        x: 20
        y: 20
        width: win.width - 40
        spacing: 24

        Label {
            width: parent.width
            wrapMode: Text.WordWrap
            textFormat: Text.PlainText
            text: win.args[3] || ""
        }

        Flow {
            width: parent.width
            spacing: 8
            layoutDirection: Qt.RightToLeft

            Repeater {
                model: win.buttons

                Button {
                    required property var modelData
                    text: modelData.label
                    highlighted: modelData.index === win.defaultIndex
                    focus: modelData.index === win.defaultIndex
                    onClicked: Qt.exit(10 + modelData.index)
                }
            }
        }
    }

    Shortcut {
        sequences: ["Return", "Enter"]
        enabled: win.defaultIndex >= 0
        onActivated: Qt.exit(10 + win.defaultIndex)
    }
    Shortcut {
        sequence: "Escape"
        onActivated: win.cancelIndex >= 0 ? Qt.exit(10 + win.cancelIndex) : Qt.quit()
    }
}
