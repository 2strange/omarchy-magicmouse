import QtQuick
import Quickshell
import Quickshell.Io

// Touch preview: a live picture of what the Magic Mouse surface reports and what
// the daemon decides. Reads the daemon's probe socket
// ($XDG_RUNTIME_DIR/magic-mouse/probe.sock); run with `qs -p <this folder>`.
ShellRoot {
  id: shell

  readonly property string socketPath: (Quickshell.env("XDG_RUNTIME_DIR") || "/run/user/1000") + "/magic-mouse/probe.sock"

  // geometry from the daemon's hello line
  property var rangeX: [-1100, 1258]
  property var rangeY: [-1589, 2047]
  property var rangeMajor: [0, 1020]
  property real unitsPerMm: 38
  property string model: ""
  property bool mouseConnected: false

  // live frame
  property var contacts: []
  property bool scrolling: false
  property bool coasting: false
  property bool moving: false
  property string axis: "y"
  property string why: ""
  property var log: []          // last decisions: [{t, why}]
  property real fps: 0
  property int frameCount: 0
  property real peakMajor: 0
  property var zones: [3, 3, 0, 18]  // left, right, front, back (palm) in mm

  // ---------- live tuning: reads and writes ~/.config/magic-mouse/config.toml via magic-mouse-config;
  // the daemon reloads the file on change, so a slider takes effect as soon as it is let go
  property var cfg: ({})
  function cfgVal(section, key, fallback) {
    var sec = cfg[section]
    return sec && sec[key] !== undefined ? sec[key] : fallback
  }
  property var writeQueue: []
  function setCfg(path, value) {
    writeQueue = writeQueue.concat([path + "=" + value])
    if (!writer.running) flushWrites()
  }
  function flushWrites() {
    if (writeQueue.length === 0) return
    writer.command = ["magic-mouse-config", "set"].concat(writeQueue)
    writeQueue = []
    writer.running = true
  }
  Process {
    id: reader
    command: ["magic-mouse-config", "get"]
    running: true
    stdout: StdioCollector { onStreamFinished: { try { shell.cfg = JSON.parse(text) } catch (err) {} } }
  }
  Process {
    id: writer
    onExited: function(code) {
      if (code !== 0) console.warn("magic-mouse-config failed", code)
      if (shell.writeQueue.length > 0) shell.flushWrites()
      else reader.running = true
    }
  }

  function handle(line) {
    var o
    try { o = JSON.parse(line) } catch (err) { return }
    if (o.type === "hello") {
      if (o.x) rangeX = o.x
      if (o.y) rangeY = o.y
      if (o.major) rangeMajor = o.major
      if (o.res) unitsPerMm = o.res
      model = o.model || ""
      mouseConnected = true
      return
    }
    if (o.type === "disconnected") { mouseConnected = false; contacts = []; pending = null; return }
    frameCount++
    pending = o  // drawn by the 30 Hz timer; every decision still goes to the log below
    var cs = o.c || []
    for (var i = 0; i < cs.length; i++) peakMajor = Math.max(peakMajor, cs[i][3])
    if (o.why && o.why !== why) {
      var next = log.slice(-11)
      next.push({ t: new Date().toLocaleTimeString(Qt.locale(), "HH:mm:ss.zzz"), why: o.why })
      log = next
    }
    why = o.why || ""
  }

  property var pending: null
  Timer {  // the mouse reports up to ~90 Hz; redraw at 30 Hz
    interval: 33; repeat: true; running: true
    onTriggered: {
      var o = shell.pending
      if (!o) return
      shell.pending = null
      shell.contacts = o.c || []
      shell.scrolling = !!o.scroll
      shell.coasting = !!o.coast
      shell.moving = !!o.moving
      shell.axis = o.axis || "y"
      if (o.zones) shell.zones = o.zones
    }
  }

  Socket {
    id: probe
    path: shell.socketPath
    connected: true
    parser: SplitParser { onRead: function(data) { shell.handle(data) } }
    onConnectedChanged: if (!connected) { shell.mouseConnected = false; shell.contacts = [] }
  }
  Timer {  // reconnect while the daemon restarts
    interval: 1000; repeat: true; running: !probe.connected
    onTriggered: probe.connected = true
  }
  Timer {
    interval: 1000; repeat: true; running: true
    onTriggered: { shell.fps = shell.frameCount; shell.frameCount = 0 }
  }

  readonly property color bg: "#1e1e22"
  readonly property color fg: "#e8e6e3"
  readonly property color dim: "#8a8780"
  readonly property color surface: "#2c2c32"
  readonly property var roleColor: ({ grip: "#6b6b75", rest: "#5b8def", follow: "#e0a33a", palm: "#d0605a", hover: "#9b7fd4" })
  readonly property string mono: "JetBrainsMono Nerd Font"

  FloatingWindow {
    title: "Magic Mouse touch preview"
    implicitWidth: 760
    implicitHeight: 900
    color: shell.bg
    visible: true

    Row {
      anchors.fill: parent
      anchors.margins: 20
      spacing: 24

      // ---------- surface ----------
      Item {
        id: pad
        width: 300
        height: parent.height
        readonly property real spanX: shell.rangeX[1] - shell.rangeX[0]
        readonly property real spanY: shell.rangeY[1] - shell.rangeY[0]
        readonly property real scale: Math.min(width / spanX, (height - 30) / spanY)

        Rectangle {
          id: shellOutline
          width: pad.spanX * pad.scale
          height: pad.spanY * pad.scale
          anchors.horizontalCenter: parent.horizontalCenter
          radius: width * 0.28
          clip: true
          color: shell.surface
          border.color: shell.scrolling ? shell.roleColor.follow : "#44444c"
          border.width: shell.scrolling ? 3 : 1

          // edge zones (scroll.edge_*_mm) and the palm zone at the back
          readonly property real mmPx: shell.unitsPerMm * pad.scale
          Rectangle { x: 0; width: shell.zones[0] * parent.mmPx; height: parent.height; color: "#ffffff"; opacity: 0.06 }
          Rectangle { x: parent.width - width; width: shell.zones[1] * parent.mmPx; height: parent.height; color: "#ffffff"; opacity: 0.06 }
          Rectangle { y: 0; width: parent.width; height: shell.zones[2] * parent.mmPx; color: "#ffffff"; opacity: 0.06 }
          Rectangle { y: parent.height - height; width: parent.width; height: shell.zones[3] * parent.mmPx; color: shell.roleColor.palm; opacity: 0.10 }
          Text { anchors.top: parent.top; anchors.topMargin: 8; anchors.horizontalCenter: parent.horizontalCenter; text: "front"; color: shell.dim; font.family: shell.mono; font.pixelSize: 11 }
          Text { visible: shell.zones[3] > 0; anchors.bottom: parent.bottom; anchors.bottomMargin: 8; anchors.horizontalCenter: parent.horizontalCenter; text: "palm"; color: shell.roleColor.palm; font.family: shell.mono; font.pixelSize: 11 }

          Repeater {
            model: shell.contacts
            delegate: Item {
              required property var modelData
              readonly property real major: modelData[3]
              readonly property real minor: modelData[4]
              // touch_major is a relative contact size (0..1020), not calibrated to position units;
              // the 0.25 factor only makes the circles roughly finger-sized on the drawing
              readonly property real dia: Math.max(10, major * pad.scale * 0.25)
              x: (modelData[1] - shell.rangeX[0]) * pad.scale - dia / 2
              y: (modelData[2] - shell.rangeY[0]) * pad.scale - dia / 2
              width: dia; height: dia
              Rectangle {
                anchors.centerIn: parent
                width: parent.dia
                height: Math.max(10, minor * pad.scale * 0.25)
                radius: width / 2
                color: shell.roleColor[modelData[5]] || shell.fg
                opacity: 0.35 + 0.65 * Math.min(1, major / Math.max(1, shell.rangeMajor[1] * 0.75))
              }
              Text {
                anchors.left: parent.right; anchors.leftMargin: 4
                anchors.verticalCenter: parent.verticalCenter
                text: Math.round(major)
                color: shell.fg; font.family: shell.mono; font.pixelSize: 12
              }
            }
          }
        }
        Text {
          anchors.top: shellOutline.bottom; anchors.topMargin: 6
          anchors.horizontalCenter: parent.horizontalCenter
          text: "y " + shell.rangeY[0] + " … " + shell.rangeY[1]
          color: shell.dim; font.family: shell.mono; font.pixelSize: 11
        }
      }

      // ---------- numbers ----------
      Column {
        width: parent.width - pad.width - 24
        spacing: 10

        Text {
          text: shell.probeState()
          color: shell.fg; font.family: shell.mono; font.pixelSize: 16; font.bold: true
          width: parent.width; elide: Text.ElideRight
        }
        Text {
          text: shell.fps + " frames/s   " + shell.contacts.length + " contact" + (shell.contacts.length === 1 ? "" : "s") + "   peak size " + Math.round(shell.peakMajor)
          color: shell.dim; font.family: shell.mono; font.pixelSize: 12
        }

        Rectangle {
          width: parent.width; height: 64; radius: 8
          color: shell.scrolling ? "#3a3020" : shell.surface
          Column {
            anchors.fill: parent; anchors.margins: 10; spacing: 4
            Text { text: shell.scrolling ? "SCROLLING (" + shell.axis + ")" : shell.coasting ? "GLIDING" : "NOT SCROLLING"; color: shell.scrolling ? shell.roleColor.follow : shell.fg; font.family: shell.mono; font.pixelSize: 14; font.bold: true }
            Text { text: shell.why || "–"; color: shell.fg; font.family: shell.mono; font.pixelSize: 12; width: parent.width; elide: Text.ElideRight }
          }
        }

        Text { text: "mouse moving: " + (shell.moving ? "yes" : "no"); color: shell.moving ? shell.roleColor.follow : shell.dim; font.family: shell.mono; font.pixelSize: 12 }

        Row {
          spacing: 14
          Repeater {
            model: [["follow", "followed"], ["rest", "resting"], ["hover", "not touching"], ["grip", "edge"], ["palm", "palm"]]
            delegate: Row {
              required property var modelData
              spacing: 6
              Rectangle { width: 10; height: 10; radius: 5; color: shell.roleColor[modelData[0]]; anchors.verticalCenter: parent.verticalCenter }
              Text { text: modelData[1]; color: shell.dim; font.family: shell.mono; font.pixelSize: 11 }
            }
          }
        }

        Text { text: "TUNING  (applies when you let go)"; color: shell.dim; font.family: shell.mono; font.pixelSize: 11; font.bold: true; topPadding: 6 }
        Tune { label: "Touch start size"; section: "scroll"; key: "touch_start_size"; from: 0; to: 900; step: 10; fallback: 480; hint: "0 = off" }
        Tune { label: "Touch hold size"; section: "scroll"; key: "touch_hold_size"; from: 0; to: 900; step: 10; fallback: 300 }
        Tune { label: "Two-finger size"; section: "scroll"; key: "touch_pair_size"; from: 0; to: 900; step: 10; fallback: 250 }
        Tune { label: "Touch report rate"; section: "scroll"; key: "touch_rate"; from: 0; to: 90; step: 1; unit: " /s"; fallback: 45; hint: "0 = any" }
        Tune { label: "Scroll start"; section: "scroll"; key: "start_mm"; from: 0.2; to: 3; step: 0.1; decimals: 1; unit: " mm"; fallback: 0.8 }
        Tune { label: "Scroll speed"; section: "scroll"; key: "speed"; from: 0.2; to: 4; step: 0.1; decimals: 1; unit: "×"; fallback: 1.0 }

        Text { text: "SIZE PER CONTACT"; color: shell.dim; font.family: shell.mono; font.pixelSize: 11; font.bold: true; topPadding: 6 }
        Column {
          width: parent.width
          height: 3 * 16  // room for three contacts, always; more are not listed
          clip: true
          Repeater {
          model: shell.contacts.slice(0, 3)
          delegate: Item {
            required property var modelData
            width: parent.width; height: 16
            Rectangle {
              width: parent.width * Math.min(1, modelData[3] / Math.max(1, shell.rangeMajor[1]))
              height: 10; radius: 3; anchors.verticalCenter: parent.verticalCenter
              color: shell.roleColor[modelData[5]] || shell.fg
            }
            Text { anchors.right: parent.right; text: "#" + modelData[0] + "  " + modelData[3] + " × " + modelData[4] + "  " + (modelData[6] || 0) + "/s"; color: shell.fg; font.family: shell.mono; font.pixelSize: 11 }
            Repeater {  // touch start and hold sizes
              model: [shell.cfgVal("scroll", "touch_start_size", 0), shell.cfgVal("scroll", "touch_hold_size", 0)]
              delegate: Rectangle {
                required property var modelData
                visible: modelData > 0
                x: parent.width * Math.min(1, modelData / Math.max(1, shell.rangeMajor[1])) - 1
                width: 2; height: 14; anchors.verticalCenter: parent.verticalCenter
                color: shell.fg; opacity: 0.6
              }
            }
          }
        }
        }

        Text { text: "DECISIONS"; color: shell.dim; font.family: shell.mono; font.pixelSize: 11; font.bold: true; topPadding: 6 }
        Repeater {
          model: shell.log.slice().reverse()
          delegate: Text {
            required property var modelData
            text: modelData.t + "  " + modelData.why
            color: shell.fg; font.family: shell.mono; font.pixelSize: 11
            width: parent.width; elide: Text.ElideRight
          }
        }
      }
    }
  }

  component Tune: Item {
    id: tune
    property string label
    property string section
    property string key
    property real from
    property real to
    property real step: 1
    property int decimals: 0
    property string unit: ""
    property string hint: ""
    property real fallback: 0
    readonly property real saved: Number(shell.cfgVal(section, key, fallback))
    property real shown: saved
    property bool dragging: false
    onSavedChanged: if (!dragging) shown = saved
    width: parent.width; height: 34
    function snap(v) { return Math.max(from, Math.min(to, Math.round(v / step) * step)) }
    Text {
      text: tune.label + (tune.hint ? "  (" + tune.hint + ")" : "")
      color: shell.fg; font.family: shell.mono; font.pixelSize: 12
    }
    Text {
      anchors.right: parent.right
      text: tune.shown.toFixed(tune.decimals) + tune.unit
      color: tune.dragging ? shell.roleColor.follow : shell.fg; font.family: shell.mono; font.pixelSize: 12; font.bold: true
    }
    Rectangle {
      id: track
      anchors.bottom: parent.bottom; anchors.bottomMargin: 4
      width: parent.width; height: 4; radius: 2; color: "#44444c"
      Rectangle { width: parent.width * (tune.shown - tune.from) / (tune.to - tune.from); height: parent.height; radius: 2; color: shell.roleColor.rest }
      Rectangle {
        x: parent.width * (tune.shown - tune.from) / (tune.to - tune.from) - width / 2
        anchors.verticalCenter: parent.verticalCenter
        width: 14; height: 14; radius: 7; color: shell.fg
      }
    }
    MouseArea {
      anchors.fill: parent
      function at(mx) { return tune.snap(tune.from + (tune.to - tune.from) * Math.max(0, Math.min(1, mx / width))) }
      onPressed: function(m) { tune.dragging = true; tune.shown = at(m.x) }
      onPositionChanged: function(m) { if (pressed) tune.shown = at(m.x) }
      onReleased: {
        tune.dragging = false
        if (Math.abs(tune.shown - tune.saved) > 1e-9) shell.setCfg(tune.section + "." + tune.key, tune.shown.toFixed(tune.decimals))
      }
      onWheel: function(w) { w.accepted = false }  // the wheel scrolls nothing here
    }
  }

  function probeState() {
    if (!probe.connected) return "Waiting for magic-mouse-daemon …"
    if (!mouseConnected) return "Daemon running, mouse not connected"
    return model || "Magic Mouse"
  }
}
