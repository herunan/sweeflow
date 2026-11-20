import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

// Sweeflow's panel: the same content and words as its own window
// (linux/src/panelwin.rs) and its Plasma widget (linux/plasmoid), drawn with
// the shell's theme and controls. The header names the Mac, says where
// streaming stands and holds its actions (Pause / Resume, Start, Stop) as
// signs; below, three tabs: Apps, Settings and Macs; Shortcuts, Check for
// updates and Quit Sweeflow at the bottom.
//
// Everything shown comes from the client's State and everything done goes
// back to the client (Client.qml). Without the client the panel says so:
// how to install it (shown and copied, never run), or a Start button.
Panel {
  id: root
  moduleName: "io.github.herunan.sweeflow"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  // Client.qml, owned by BarWidget.qml.
  property var client: null

  readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
  readonly property color urgent: bar ? bar.urgent : Commons.Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.5)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  readonly property bool installed: client ? client.installed : true
  readonly property bool running: client ? client.running : false
  readonly property bool paired: client ? client.paired : false
  readonly property var st: client ? client.st : ({})
  readonly property var prefs: client ? client.settings : ({})

  // Opens on the tab it was left on.
  property string tab: "apps"
  property string query: ""
  // The Mac being renamed, or asked about unlinking (fingerprints).
  property string editingMac: ""
  property string confirmingMac: ""
  // A text field has the keyboard: keys go to it, not to the panel.
  property bool typing: false
  property bool copied: false

  readonly property var appRows: client ? Model.appRows(client.apps, query, client.streaming) : []
  readonly property var tabs: {
    var count = client ? client.apps.length : 0
    return [
      { value: "apps", label: count ? "Apps (" + count + ")" : "Apps" },
      { value: "settings", label: "Settings" },
      { value: "macs", label: "Macs" }
    ]
  }

  function open() {
    root.query = ""
    if (client) {
      // pick up an install, a start or a stop since the last look
      client.checkInstalled()
      client.refresh()
      // Hyprland's rules can be changed outside Sweeflow too
      client.act("placement")
    }
    root.controller.show()
  }

  function close() {
    // a rename or a question left open is put back
    root.editingMac = ""
    root.confirmingMac = ""
    root.controller.hide()
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.hostWidget || root, direction)
    return false
  }

  function act(action, arg) {
    if (client) client.act(action, arg)
  }

  function set(changes) {
    if (client) client.set(changes)
  }

  function stepTab(direction) {
    var order = ["apps", "settings", "macs"]
    var i = Math.max(0, order.indexOf(root.tab)) + direction
    root.tab = order[Math.max(0, Math.min(order.length - 1, i))]
  }

  function copyInstall() {
    // Only onto the clipboard: the user runs it, in their own terminal.
    Quickshell.execDetached(["wl-copy", "--", Model.installText()])
    root.copied = true
    copiedTimer.restart()
  }

  onOpenedChanged: if (opened) {
    Qt.callLater(function() { if (panel.loadedContent) panel.loadedContent.takeFocus() })
  }
  onTabChanged: {
    root.editingMac = ""
    root.confirmingMac = ""
    if (panel.loadedContent) panel.loadedContent.takeFocus()
  }

  Connections {
    target: root.client
    // A Mac renamed or unlinked: its row is back to normal.
    function onMacsJsonChanged() {
      root.editingMac = ""
      root.confirmingMac = ""
    }
  }

  Timer {
    id: copiedTimer
    interval: 2000
    onTriggered: root.copied = false
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.hostWidget || root
    bar: root.bar
    open: root.opened
    focusTarget: panel.loadedContent ? panel.loadedContent.focusItem : null
    contentWidth: panel.fittedContentWidth(Style.space(440))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight)

    content: PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      implicitHeight: root.paired ? Style.space(600) : header.implicitHeight + plain.implicitHeight + Style.space(28)
      blocked: root.typing

      // The search field while the apps show, so a name can be typed at once.
      readonly property Item focusItem: root.paired && root.tab === "apps" ? searchField : keyCatcher
      function takeFocus() { focusItem.forceActiveFocus() }

      onCloseRequested: {
        // Esc puts back a rename or a question first; only then does it close.
        if (root.editingMac !== "" || root.confirmingMac !== "") {
          root.editingMac = ""
          root.confirmingMac = ""
        } else {
          root.close()
        }
      }
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onMoveRequested: function(dx, dy) {
        if (!root.paired) return
        if (dx !== 0) root.stepTab(dx)
        else if (root.tab === "apps") appList.flick(0, -dy * 900)
        else if (root.tab === "settings") settingsFlick.flick(0, -dy * 900)
      }

      // ---- the Mac, where streaming stands, and what to do about it
      RowLayout {
        id: header
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        spacing: Style.space(12)

        Image {
          // The app icon (violet square); the plain logo is the bar's icon.
          source: Qt.resolvedUrl("../assets/sweeflow-app.svg")
          sourceSize.width: Style.space(72)
          sourceSize.height: Style.space(72)
          Layout.preferredWidth: Style.space(36)
          Layout.preferredHeight: Style.space(36)
          opacity: root.running ? 1.0 : 0.55
        }

        ColumnLayout {
          Layout.fillWidth: true
          spacing: Style.space(2)

          Text {
            Layout.fillWidth: true
            textFormat: Text.PlainText
            text: root.paired ? root.client.macName : "Sweeflow"
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.title
            font.bold: true
            elide: Text.ElideRight
          }

          RowLayout {
            visible: root.paired
            Layout.fillWidth: true
            spacing: Style.space(6)

            // The Mac in use's dot, as in the list of Macs.
            StateDot { standing: root.client ? root.client.activeState : "" }
            Note { text: root.client ? root.client.statusText : "" }
            ViaSign {
              info: root.client ? root.client.activeMac : null
              Layout.fillWidth: true
            }
          }
        }

        // What can be done in this state, as signs at the right of the name.
        Repeater {
          model: root.client && root.paired ? root.client.streamActions : []
          delegate: SignButton {
            required property string modelData
            action: modelData
            why: modelData === "start" && root.client.activeMac ? root.client.startBlocked(root.client.activeMac) : ""
            tip: Model.actionTip(modelData, root.client.keys)
            onAsked: root.act(modelData)
          }
        }
      }

      // ---- no client to ask, or no Mac yet
      Column {
        id: plain
        visible: !root.paired
        anchors.top: header.bottom
        anchors.topMargin: Style.space(20)
        anchors.left: parent.left
        anchors.right: parent.right
        spacing: Style.space(12)

        Text {
          width: parent.width
          textFormat: Text.PlainText
          horizontalAlignment: Text.AlignHCenter
          wrapMode: Text.WordWrap
          text: !root.installed ? "Sweeflow isn't installed"
            : !root.running ? "Sweeflow isn't running"
            : "No Mac linked yet"
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.subtitle
          font.bold: true
        }

        // Not installed: how to, for a terminal. The plugin installs nothing.
        Column {
          visible: !root.installed
          width: parent.width
          spacing: Style.space(10)

          Note {
            width: parent.width
            wrapMode: Text.WordWrap
            elide: Text.ElideNone
            text: "This plugin is Sweeflow's panel. The streaming is done by Sweeflow itself, which you install with pacman. In a terminal:"
          }

          BorderSurface {
            width: parent.width
            implicitHeight: steps.implicitHeight + Style.space(20)
            radius: Style.cornerRadius
            color: Style.normalFillFor(root.foreground, Commons.Color.accent)
            borderSpec: Border.controlSpec("normal", root.foreground, Commons.Color.accent)

            Column {
              id: steps
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              anchors.leftMargin: Style.space(10)
              anchors.rightMargin: Style.space(10)
              spacing: Style.space(8)

              Repeater {
                model: Model.INSTALL_STEPS
                delegate: Text {
                  required property string modelData
                  width: steps.width
                  textFormat: Text.PlainText
                  text: modelData
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                  wrapMode: Text.WrapAnywhere
                }
              }
            }
          }

          Button {
            anchors.horizontalCenter: parent.horizontalCenter
            text: root.copied ? "Copied" : "Copy"
            bordered: true
            foreground: root.foreground
            fontFamily: root.fontFamily
            onClicked: root.copyInstall()
          }

          Note {
            width: parent.width
            wrapMode: Text.WordWrap
            elide: Text.ElideNone
            text: "Then open Sweeflow from the app launcher and link your Mac."
          }
        }

        Button {
          visible: root.installed && !root.running
          anchors.horizontalCenter: parent.horizontalCenter
          text: "Start Sweeflow"
          bordered: true
          enabled: root.client ? !root.client.starting : false
          opacity: enabled ? 1.0 : 0.5
          foreground: root.foreground
          fontFamily: root.fontFamily
          onClicked: if (root.client) root.client.start()
        }

        Button {
          visible: root.running && !root.paired
          anchors.horizontalCenter: parent.horizontalCenter
          text: "Link a Mac…"
          bordered: true
          foreground: root.foreground
          fontFamily: root.fontFamily
          onClicked: {
            root.act("pair")
            root.close()
          }
        }
      }

      // ---- a Mac is linked
      ColumnLayout {
        visible: root.paired
        anchors.top: header.bottom
        anchors.topMargin: Style.space(12)
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        spacing: Style.space(10)

        // A newer Sweeflow, and how to get it (update.rs panel_json).
        BorderSurface {
          id: upd
          readonly property var u: root.client ? (root.client.updates.update || null) : null
          visible: !!u
          Layout.fillWidth: true
          implicitHeight: updRow.implicitHeight + Style.space(16)
          radius: Style.cornerRadius
          color: Style.selectedFillFor(root.foreground, Commons.Color.accent)
          borderSpec: Border.controlSpec("normal", root.foreground, Commons.Color.accent)

          RowLayout {
            id: updRow
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            anchors.leftMargin: Style.space(10)
            anchors.rightMargin: Style.space(10)
            spacing: Style.space(10)

            ColumnLayout {
              Layout.fillWidth: true
              spacing: Style.space(2)
              Text {
                Layout.fillWidth: true
                textFormat: Text.PlainText
                text: upd.u ? "Sweeflow " + upd.u.version + " is available" : ""
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                font.bold: true
                elide: Text.ElideRight
              }
              Note {
                Layout.fillWidth: true
                wrapMode: Text.WordWrap
                elide: Text.ElideNone
                text: upd.u ? String(upd.u.words || "") : ""
              }
            }

            // The update window: what's new, and the update (or how it's done).
            Button {
              text: upd.u && upd.u.how === "self" ? "Update" : "What's New"
              bordered: true
              foreground: root.foreground
              fontFamily: root.fontFamily
              onClicked: {
                root.act("update-window", "")
                root.close()
              }
            }
          }
        }

        // The Mac on another version of Sweeflow.
        Note {
          readonly property string words: root.client ? String(root.client.updates.macNote || "") : ""
          visible: words !== ""
          text: words
          Layout.fillWidth: true
          wrapMode: Text.WordWrap
          elide: Text.ElideNone
        }

        ButtonGroup {
          options: root.tabs
          value: root.tab
          foreground: root.foreground
          fontFamily: root.fontFamily
          focusable: false
          onChanged: function(value) { root.tab = value }
        }

        Item {
          Layout.fillWidth: true
          Layout.fillHeight: true

          // ---- Apps: pinned, running, the rest (the client sorts them so), under headings.
          ColumnLayout {
            anchors.fill: parent
            visible: root.tab === "apps"
            spacing: Style.space(8)

            TextField {
              id: searchField
              Layout.fillWidth: true
              placeholderText: "Search apps"
              foreground: root.foreground
              font.family: root.fontFamily
              text: root.query
              onTextEdited: root.query = text
              onActiveFocusChanged: root.typing = activeFocus
              Keys.onEscapePressed: {
                if (text !== "") {
                  root.query = ""
                } else {
                  root.close()
                }
              }
              Keys.onTabPressed: root.switchPanel(1)
              Keys.onBacktabPressed: root.switchPanel(-1)
              Keys.onDownPressed: appList.flick(0, -900)
              Keys.onUpPressed: appList.flick(0, 900)
            }

            ListView {
              id: appList
              Layout.fillWidth: true
              Layout.fillHeight: true
              clip: true
              boundsBehavior: Flickable.StopAtBounds
              model: root.appRows
              spacing: Style.space(2)
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

              delegate: Column {
                id: appRow
                required property var modelData
                readonly property bool shown: root.client ? root.client.exclude.indexOf(modelData.id) < 0 : true
                width: ListView.view.width
                spacing: Style.space(2)

                PanelSectionHeader {
                  visible: appRow.modelData.heading !== ""
                  text: appRow.modelData.heading
                  foreground: root.foreground
                  fontFamily: root.fontFamily
                  leftPadding: Style.space(6)
                  bottomPadding: Style.space(2)
                  topPadding: Style.space(8)
                }

                // Click to open it; the switch says whether its windows come here.
                CursorSurface {
                  width: parent.width
                  implicitHeight: Style.space(38)
                  hasCursor: rowHover.hovered
                  foreground: root.foreground

                  HoverHandler { id: rowHover }

                  MouseArea {
                    anchors.fill: parent
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.act("launch", appRow.modelData.id)
                  }

                  RowLayout {
                    anchors.fill: parent
                    anchors.leftMargin: Style.space(8)
                    anchors.rightMargin: Style.space(4)
                    spacing: Style.space(8)

                    Item {
                      Layout.preferredWidth: Style.space(24)
                      Layout.preferredHeight: Style.space(24)
                      Image {
                        anchors.fill: parent
                        visible: appRow.modelData.icon !== ""
                        source: appRow.modelData.icon
                        sourceSize.width: Style.space(48)
                        sourceSize.height: Style.space(48)
                        asynchronous: true
                        fillMode: Image.PreserveAspectFit
                      }
                      Text {
                        // no icon saved for it (Apps in launcher is off)
                        anchors.centerIn: parent
                        visible: appRow.modelData.icon === ""
                        textFormat: Text.PlainText
                        text: "a"
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.iconLarge
                      }
                    }

                    Text {
                      Layout.fillWidth: true
                      textFormat: Text.PlainText
                      text: appRow.modelData.name
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.body
                      elide: Text.ElideRight
                    }

                    // Running on the Mac (a pinned one shows it here too).
                    Rectangle {
                      visible: appRow.modelData.running
                      implicitWidth: Style.space(6)
                      implicitHeight: Style.space(6)
                      radius: width / 2
                      color: Model.VIOLET
                    }

                    RowButton {
                      visible: appRow.modelData.running
                      text: "Quit"
                      tooltipText: "Quit " + appRow.modelData.name + " on the Mac"
                      onClicked: root.act("quit-app", appRow.modelData.id)
                    }

                    RowButton {
                      text: appRow.modelData.pinned ? "Unpin" : "Pin"
                      tooltipText: appRow.modelData.pinned ? "Unpin from the top" : "Pin to the top"
                      onClicked: root.act(appRow.modelData.pinned ? "unpin" : "pin", appRow.modelData.id)
                    }

                    ToggleSwitch {
                      id: appSwitch
                      checked: appRow.shown
                      foreground: root.foreground
                      onToggled: root.set({ exclude: Model.excludeWith(root.client.exclude, appRow.modelData.id, !appRow.shown) })

                      PanelToolTip {
                        visible: appSwitch.containsMouse
                        text: appRow.shown ? "Block this app from streaming" : "Allow this app to stream"
                        fontFamily: root.fontFamily
                      }
                    }
                  }
                }
              }

              Note {
                anchors.centerIn: parent
                width: parent.width - Style.space(48)
                visible: appList.count === 0
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                elide: Text.ElideNone
                text: root.query.trim() !== "" ? "No matching apps" : "The Mac's apps show here once it has connected."
              }
            }
          }

          // ---- Settings, every one a switch, then what can be done to the Mac now.
          Flickable {
            id: settingsFlick
            anchors.fill: parent
            visible: root.tab === "settings"
            clip: true
            contentWidth: width
            contentHeight: settingsColumn.implicitHeight
            boundsBehavior: Flickable.StopAtBounds
            flickableDirection: Flickable.VerticalFlick
            interactive: contentHeight > height
            ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

            Column {
              id: settingsColumn
              width: settingsFlick.width
              spacing: Style.space(4)

              // The features first, named and ordered as on the setup window's last page.
              Setting {
                label: "Apps in launcher"
                hint: "Mac apps in the app launcher, alongside your Linux apps"
                on: !!root.prefs.launcher
                onFlipped: function(v) { root.set({ launcher: v }) }
              }
              // Plasma only: the other desktops have no menu bar to put the menus in.
              Setting {
                visible: !!(root.client && root.client.placement.plasma)
                label: "Global menu"
                hint: "The Mac apps' menus at the top of the screen"
                on: !!(root.client && root.client.placement.globalMenu)
                onFlipped: function(v) { root.act("global-menu", v ? "on" : "off") }
              }
              Setting {
                label: "Shared clipboard"
                hint: "Copy on one, paste on the other"
                on: !!root.prefs.clipboard
                onFlipped: function(v) { root.set({ clipboard: v }) }
              }
              Setting {
                label: "Drag and drop files"
                hint: "Files, between Mac windows and Linux"
                on: !!root.prefs.dragDrop
                onFlipped: function(v) { root.set({ dragDrop: v }) }
              }
              Setting {
                label: "Sound passthrough"
                hint: "The Mac's sound plays on this computer"
                on: !!root.prefs.sound
                onFlipped: function(v) { root.set({ sound: v }) }
              }
              Setting {
                label: "High quality"
                hint: "Retina detail and 10-bit colour. Off: half the detail and 8-bit"
                on: root.prefs.quality !== "light"
                onFlipped: function(v) { root.set({ quality: v ? "sharp" : "light" }) }
              }
              Setting {
                label: "Square window corners"
                hint: "Mac windows end in square corners instead of the Mac's rounded ones"
                on: !!root.st.square
                onFlipped: function(v) { root.act("square", v ? "on" : "off") }
              }
              // A MacBook, connected: the Mac asks for its password (LidSleepRule.swift).
              Setting {
                visible: !!(root.client && root.client.connected && root.prefs.hasLid)
                label: "Stream with lid closed"
                note: root.prefs.lidAsking ? "Waiting for the Mac's password" : ""
                enabled: !root.prefs.lidAsking
                hint: "Keep the MacBook available while its lid is closed. Asks for the Mac's admin password to turn it on or off."
                on: !!root.prefs.lid
                onFlipped: function(v) { root.set({ lid: v }) }
              }
              Setting {
                label: "Open at login"
                hint: "Start Sweeflow when you log in"
                on: !!root.st.openAtLogin
                onFlipped: function(v) { root.act("login", v ? "on" : "off") }
              }

              Item { width: 1; height: Style.space(2) }
              PanelSeparator { foreground: root.foreground }
              Item { width: 1; height: Style.space(2) }

              // Dims the Mac's own screen now; using the Mac itself brings it back.
              ActionRow {
                label: "Dim Mac screen"
                note: "Until someone uses the Mac itself"
                buttonText: root.prefs.dim ? "Dimmed" : "Dim"
                available: !!(root.client && root.client.streaming) && !root.prefs.dim
                onAsked: root.set({ dim: true })
              }
              // The Mac's whole screen in a window, to see what's going on there.
              ActionRow {
                label: "Troubleshoot (stream display)"
                note: "Show the Mac's whole screen in a window"
                buttonText: root.st.displayShown ? "Hide" : "Show"
                available: !!(root.client && root.client.streaming) || !!root.st.displayShown
                onAsked: {
                  var shown = !!root.st.displayShown
                  root.act("display")
                  if (!shown) root.close()
                }
              }
            }
          }

          // ---- The linked Macs, one line each: the name (double-click to rename),
          // where it stands, its stream actions (those of its state for the Mac in
          // use; Start, which switches to it, for another) and Unlink.
          ColumnLayout {
            anchors.fill: parent
            visible: root.tab === "macs"
            spacing: Style.space(8)

            ListView {
              Layout.fillWidth: true
              Layout.fillHeight: true
              clip: true
              boundsBehavior: Flickable.StopAtBounds
              model: root.client ? root.client.macs : []
              spacing: Style.space(4)
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

              delegate: Item {
                id: mac
                required property var modelData
                readonly property bool editing: root.editingMac === modelData.fingerprint
                readonly property bool confirming: root.confirmingMac === modelData.fingerprint
                readonly property string standing: root.client ? root.client.macState(modelData) : ""
                width: ListView.view.width
                // As tall as its tallest state, so the list stays put while a Mac
                // is renamed or asked about.
                implicitHeight: Math.max(lines.implicitHeight, renaming.implicitHeight, asking.implicitHeight) + Style.space(12)

                function save() {
                  var name = field.text.trim()
                  if (name !== "" && name !== modelData.name && root.client) root.client.rename(modelData.fingerprint, name)
                  root.editingMac = ""
                }

                RowLayout {
                  id: lines
                  anchors.left: parent.left
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  anchors.leftMargin: Style.space(6)
                  visible: !mac.editing && !mac.confirming
                  spacing: Style.space(6)

                  ColumnLayout {
                    Layout.fillWidth: true
                    spacing: Style.space(2)

                    Text {
                      Layout.fillWidth: true
                      textFormat: Text.PlainText
                      text: mac.modelData.name
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.body
                      font.bold: mac.modelData.active
                      elide: Text.ElideRight

                      MouseArea {
                        id: nameArea
                        anchors.fill: parent
                        hoverEnabled: true
                        onDoubleClicked: {
                          root.confirmingMac = ""
                          root.editingMac = mac.modelData.fingerprint
                        }
                      }

                      PanelToolTip {
                        visible: nameArea.containsMouse
                        text: "Double-click to rename"
                        fontFamily: root.fontFamily
                      }
                    }

                    // The dot says it in colour; the words stay plain.
                    RowLayout {
                      visible: mac.standing !== "" || mac.modelData.via !== ""
                      Layout.fillWidth: true
                      spacing: Style.space(6)

                      StateDot {
                        visible: mac.standing !== ""
                        standing: mac.standing
                      }
                      Note { text: Model.STATE_WORDS[mac.standing] || "" }
                      ViaSign {
                        info: mac.modelData
                        lead: mac.standing !== ""
                        Layout.fillWidth: true
                      }
                    }
                  }

                  Repeater {
                    model: mac.modelData.active && root.client ? root.client.streamActions : ["start"]
                    delegate: SignButton {
                      required property string modelData
                      action: modelData
                      why: modelData === "start" && root.client ? root.client.startBlocked(mac.modelData) : ""
                      tip: mac.modelData.active ? Model.actionTip(modelData, root.client ? root.client.keys : null)
                        : "Start streaming from this Mac"
                      onAsked: {
                        if (mac.modelData.active) root.act(modelData)
                        else root.act("switch", mac.modelData.fingerprint)
                      }
                    }
                  }

                  PanelActionButton {
                    iconText: ""
                    tooltipText: "Unlink this Mac"
                    bordered: true
                    foreground: root.foreground
                    hoverColor: root.urgent
                    fontFamily: root.fontFamily
                    size: Style.space(28)
                    onClicked: {
                      root.editingMac = ""
                      root.confirmingMac = mac.modelData.fingerprint
                    }
                  }
                }

                // Renamed in place: Enter or Save keeps the name, Esc or Cancel the old one.
                RowLayout {
                  id: renaming
                  anchors.left: parent.left
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  visible: mac.editing
                  spacing: Style.space(6)

                  TextField {
                    id: field
                    Layout.fillWidth: true
                    foreground: root.foreground
                    font.family: root.fontFamily
                    onAccepted: mac.save()
                    onActiveFocusChanged: root.typing = activeFocus
                    Keys.onEscapePressed: root.editingMac = ""
                    onVisibleChanged: if (visible) {
                      text = mac.modelData.name
                      forceActiveFocus()
                      selectAll()
                    } else if (activeFocus) {
                      keyCatcher.forceActiveFocus()
                    }
                  }
                  RowButton {
                    text: "Save"
                    onClicked: mac.save()
                  }
                  RowButton {
                    text: "Cancel"
                    onClicked: root.editingMac = ""
                  }
                }

                // Asked once before a Mac is unlinked.
                RowLayout {
                  id: asking
                  anchors.left: parent.left
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  anchors.leftMargin: Style.space(6)
                  visible: mac.confirming
                  spacing: Style.space(6)

                  Text {
                    Layout.fillWidth: true
                    textFormat: Text.PlainText
                    text: "Unlink " + mac.modelData.name + "? To use it again you link it again."
                    color: root.foreground
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.body
                    wrapMode: Text.Wrap
                  }
                  RowButton {
                    text: "Unlink"
                    onClicked: {
                      root.confirmingMac = ""
                      root.act("forget", mac.modelData.fingerprint)
                    }
                  }
                  RowButton {
                    text: "Cancel"
                    onClicked: root.confirmingMac = ""
                  }
                }
              }
            }

            Button {
              text: "Link a Mac…"
              bordered: true
              foreground: root.foreground
              fontFamily: root.fontFamily
              onClicked: {
                root.act("pair")
                root.close()
              }
            }
          }
        }

        PanelSeparator {
          foreground: root.foreground
          Layout.fillWidth: true
        }

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.space(8)

          // The Shortcuts window: Sweeflow's shortcuts, the swap, the ones kept for Linux.
          Button {
            text: "Shortcuts"
            bordered: true
            foreground: root.foreground
            fontFamily: root.fontFamily
            onClicked: {
              root.act("shortcuts")
              root.close()
            }
          }
          Button {
            text: "Check for updates"
            bordered: true
            foreground: root.foreground
            fontFamily: root.fontFamily
            onClicked: {
              root.act("update-window", "")
              root.close()
            }
          }
          Item { Layout.fillWidth: true }
          Button {
            text: "Quit Sweeflow"
            tooltipText: "Stop Sweeflow on this computer (the Mac's windows close here)"
            bordered: true
            foreground: root.foreground
            fontFamily: root.fontFamily
            onClicked: root.act("quit")
          }
        }
      }
    }
  }

  // Small dim text: a status, a note under a name.
  component Note: Text {
    textFormat: Text.PlainText
    color: root.dim
    font.family: root.fontFamily
    font.pixelSize: Style.font.caption
    elide: Text.ElideRight
  }

  // Where a Mac stands: Streaming violet, Paused a violet ring; Available
  // green; Busy or Locked orange; Not online (or not known yet) a grey ring.
  component StateDot: Rectangle {
    property string standing: ""
    readonly property color ink: Model.stateInk(standing)
    readonly property bool hollow: Model.stateHollow(standing)
    implicitWidth: Style.space(8)
    implicitHeight: Style.space(8)
    radius: width / 2
    color: hollow ? "transparent" : ink
    border.width: hollow ? Math.max(1, Style.space(2)) : 0
    border.color: ink
  }

  // How a Mac is reached, after its status: "· Local: 192.168.1.20" (no dot
  // with no status before it).
  component ViaSign: Note {
    property var info: null
    property bool lead: true
    readonly property string line: Model.viaLine(info)
    visible: line !== ""
    text: line !== "" && lead ? "· " + line : line
  }

  // A streaming action as its sign, the way a player shows them; its name is
  // said on hover. One that can't be chosen now is faded and says why.
  component SignButton: PanelActionButton {
    id: sign
    property string action: ""
    property string why: ""
    property string tip: ""
    signal asked()

    iconText: Model.ACTION_GLYPHS[action] || ""
    tooltipText: why !== "" ? why : (tip !== "" ? tip : (Model.ACTION_LABELS[action] || action))
    bordered: true
    foreground: root.foreground
    fontFamily: root.fontFamily
    size: Style.space(28)
    // Faded rather than disabled, so the reason still shows on hover.
    opacity: why !== "" ? 0.45 : 1
    onClicked: if (sign.why === "") sign.asked()
  }

  // A button inside a row of a list.
  component RowButton: Button {
    bordered: true
    foreground: root.foreground
    fontFamily: root.fontFamily
    fontSize: Style.font.bodySmall
    verticalPadding: Style.space(4)
    horizontalPadding: Style.space(8)
  }

  // A setting as a switch: its name, a line about it, and (lid) a note while
  // the Mac is asking for its password.
  component Setting: Toggle {
    id: setting
    property string hint: ""
    property string note: ""
    property bool on: false
    signal flipped(bool value)

    width: parent ? parent.width : implicitWidth
    opacity: enabled ? 1.0 : 0.5
    description: note !== "" ? note : hint
    checked: on
    foreground: root.foreground
    fontFamily: root.fontFamily
    titleSize: Style.font.body
    onClicked: setting.flipped(!setting.on)
  }

  // Something to do now, not a setting: what it is, a line about it, and its
  // button (only the button does it).
  component ActionRow: Item {
    id: entry
    property string label: ""
    property string note: ""
    property string buttonText: ""
    property bool available: true
    signal asked()

    width: parent ? parent.width : implicitWidth
    implicitHeight: Math.max(entryLabels.implicitHeight, entryButton.implicitHeight) + Style.space(12)

    Column {
      id: entryLabels
      anchors.left: parent.left
      anchors.leftMargin: Style.spacing.rowPaddingX
      anchors.right: entryButton.left
      anchors.rightMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.spacing.xs

      Text {
        width: parent.width
        textFormat: Text.PlainText
        text: entry.label
        color: root.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        font.bold: true
        elide: Text.ElideRight
      }
      Note {
        width: parent.width
        text: entry.note
      }
    }

    Button {
      id: entryButton
      anchors.right: parent.right
      anchors.rightMargin: Style.spacing.rowPaddingX
      anchors.verticalCenter: parent.verticalCenter
      text: entry.buttonText
      bordered: true
      enabled: entry.available
      opacity: enabled ? 1.0 : 0.45
      foreground: root.foreground
      fontFamily: root.fontFamily
      onClicked: entry.asked()
    }
  }
}
