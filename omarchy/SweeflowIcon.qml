import QtQuick
import QtQuick.Shapes
import qs.Commons
import qs.Commons as Commons

// Sweeflow's tray logo (assets/sweeflow-symbolic.svg: the diagonal sweet),
// drawn as shapes so it takes the bar's text colour like the icons beside it.
// The dot in its empty bottom-right corner says where the Mac in use stands,
// as Sweeflow's tray icon does on other desktops (linux/src/setup.rs
// tray_icon): filled, or a ring (paused, not online, not known yet).
Item {
  id: root

  property real iconSize: Style.font.icon
  property color color: Commons.Color.foreground
  property bool dotVisible: false
  property color dotColor: Commons.Color.accent
  property bool dotHollow: false

  // The SVG's view: 119 wide and high, from (4.5, 2.6).
  readonly property real unit: iconSize / 119

  width: iconSize
  height: iconSize
  implicitWidth: iconSize
  implicitHeight: iconSize

  // The SVG's shapes with its transform (translate(64 64) rotate(-20)
  // scale(1.2), less the view's origin) already applied to every point, so
  // the shape's own bounds are the logo's (the bar measures them to size and
  // centre an icon).
  Shape {
    width: 119
    height: 119
    scale: root.unit
    transformOrigin: Item.TopLeft
    preferredRendererType: Shape.CurveRenderer

    Piece { d: "M 32.33 67.46 C 24.95 68.23 11.52 55.88 4.03 59.88 C -3.45 63.88 0.38 74.40 8.68 72.65 C 1.20 76.66 5.04 87.19 13.34 85.44 C 5.86 89.44 9.69 99.96 17.99 98.22 C 26.29 96.47 28.65 78.38 34.80 74.22 Z" }
    Piece { d: "M 84.20 48.58 C 90.35 44.42 92.71 26.33 101.01 24.58 C 109.31 22.84 113.14 33.36 105.66 37.36 C 113.96 35.61 117.80 46.14 110.32 50.15 C 118.62 48.40 122.45 58.92 114.97 62.92 C 107.48 66.92 94.05 54.57 86.67 55.34 Z" }
    Piece { d: "M 54.57 47.87 C 58.88 38.64 76.30 30.38 84.92 47.04 C 91.54 59.96 88.26 79.03 78.68 82.51 C 74.73 83.95 71.30 83.29 67.91 84.52 C 64.53 85.75 62.33 88.47 58.38 89.90 C 48.80 93.39 34.03 80.89 30.80 66.74 C 26.69 48.44 45.35 43.57 54.57 47.87 Z" }
    Piece { d: "M 52.66 48.57 L 50.61 42.93 C 49.37 39.55 49.70 36.23 51.14 33.16 L 54.40 34.40 C 53.47 36.78 53.41 38.71 54.44 41.53 L 56.49 47.17 Z" }
    Piece { d: "M 57.34 37.92 C 57.44 27.67 62.97 21.83 71.48 20.64 C 71.18 30.33 65.64 36.18 57.34 37.92 Z" }
  }

  // Centre (105.5, 103.6) in the SVG; radius 18 filled, 13 + a 10 wide line as a ring.
  Rectangle {
    visible: root.dotVisible
    readonly property real side: Math.max(4, 36 * root.unit)
    width: side
    height: side
    radius: side / 2
    x: (105.5 - 4.5) * root.unit - side / 2
    y: (103.6 - 2.6) * root.unit - side / 2
    color: root.dotHollow ? "transparent" : root.dotColor
    border.width: root.dotHollow ? Math.max(1, side * 0.28) : 0
    border.color: root.dotColor
    antialiasing: true
  }

  component Piece: ShapePath {
    id: piece
    property string d: ""
    fillColor: root.color
    strokeWidth: -1
    strokeColor: "transparent"
    PathSvg { path: piece.d }
  }
}
