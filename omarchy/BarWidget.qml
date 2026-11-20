import QtQuick
import qs.Ui
import "Model.js" as Model

// Bar entry point: Sweeflow's logo, with a dot for where the Mac in use
// stands, that opens the panel. Follows the development guide's bar-widget +
// nested Panel.qml shape (and the built-in clock), forwarding the panel
// lifecycle Quattro uses for clicks and `omarchy-shell shell summon|hide`.
BarWidget {
  id: root
  moduleName: "io.github.herunan.sweeflow"

  readonly property bool opened: panelLoader.item
    ? panelLoader.item.opened === true
    : false
  readonly property bool popoutSwitchClosing: panelLoader.item
    ? panelLoader.item.popoutSwitchClosing === true
    : false

  function open() {
    if (panelLoader.item) panelLoader.item.open()
  }

  function close() {
    if (panelLoader.item) panelLoader.item.close()
  }

  function toggle() {
    if (panelLoader.item) panelLoader.item.toggle()
  }

  function closeForPopoutSwitch() {
    if (panelLoader.item) panelLoader.item.closeForPopoutSwitch()
  }

  function injectPanel() {
    if (!panelLoader.item) return
    panelLoader.item.bar = root.bar
    panelLoader.item.anchorItem = button
    panelLoader.item.hostWidget = root
    panelLoader.item.client = client
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()

  // The client's state: shared by the icon and the panel.
  Client {
    id: client
    // Asked to show itself (`sweeflow open`, its shortcut): open the panel.
    onShowAtChanged: if (showAt > 0 && Math.abs(Date.now() - showAt) < 10000) root.open()
  }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    tooltipText: "Sweeflow: " + (client.paired && client.st.mac ? client.st.mac + ", " : "") + client.statusText
    iconComponent: Component {
      Item {
        SweeflowIcon {
          x: (parent.width - width) / 2
          y: (parent.height - height) / 2
          iconSize: button.iconSize
          color: button.foreground
          // Faded while the client isn't there to ask.
          opacity: client.running ? 1.0 : 0.5
          dotVisible: client.running
          dotColor: Model.stateInk(client.activeState)
          dotHollow: Model.stateHollow(client.activeState)
        }
      }
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.LeftButton) root.toggle()
    }
  }
}
